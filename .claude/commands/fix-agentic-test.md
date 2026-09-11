---
name: fix-agentic-test
description: Triage a failing agentic browser flow — classify into Tier-1, Tier-2, behavioral failure, or cache-health, then walk the dev through the right fix
activeForm: Triaging failing agentic browser flow
argument-hint: "<flow-stem-or-bucket>"
allowed-tools:
  - Bash
  - Read
  - Edit
  - Grep
  - AskUserQuestion
---

# Triage and repair a failing agentic browser flow

When a flow is failing in CI (or locally), classify the failure into one
of four buckets and route the dev to the right fix. Don't guess — the
fixes are very different and applying the wrong one can hide a real bug
or pay needless re-record cost.

`$ARGUMENTS` is the flow stem (e.g. `chat-ask`, `builder-edits-app`) or
a CI bucket name (`builder`, `semantic`, `ask-agent`, `threads`, `ide`, `metric-tree`).

This command must be run from the root of an `oxy-hq/oxygen-internal`
checkout.

## Steps

### 1. Sanity-check + locate artefacts

```bash
test -f web-app/tests/agentic/README.md || { echo "Not in an oxy-hq/oxygen-internal checkout"; exit 2; }
```

Find the most recent results files:

```bash
RESULTS_DIR=web-app/tests/agentic/.results
LATEST_MD=$(ls -t "$RESULTS_DIR"/*.md 2>/dev/null | head -1)
LATEST_JSON=$(ls -t "$RESULTS_DIR"/*.json 2>/dev/null | head -1)
HEALING="$RESULTS_DIR/healing.json"
STAGING=web-app/tests/agentic/.cache/healing-staging.json
```

If no results exist locally, the dev probably saw the failure in CI.
Two paths:

- **Download CI artefacts** from the failed run (`agentic-results-<bucket>` artifact contains the JSON, markdown summary, traces, and backend logs).
- **Reproduce locally**: `/run-agentic-tests <flow-stem>` and continue
  triage from the local results.

### 2. Read the latest summary

```bash
cat "$LATEST_MD"
```

In the markdown summary look for:

- The failing case (`❌ <case-name>`).
- Per-step debug table — `iterations`, `error`, `from_cache`,
  `selector_drift_events` for each step.
- Cost-budget warnings (`⚠️ over budget`).

### 3. Classify the failure

Walk through the four buckets in order. First match wins.

#### Bucket A — Tier-1 self-heal happened (no action needed)

**Signal:** test passed BUT `selector_drift_events > 0` in `step_debug`
for one or more steps.

What it means: a primary recorded selector failed, a fallback resolved,
the cache's strategy ranks updated silently. $0 LLM cost. The test
ran clean.

**Action:** none. Surface for awareness only — the dev should know the
underlying component changed (a testid was renamed, a label was edited,
etc.) but they don't need to do anything. The cache will keep replaying
fine.

If the drift event count is consistently high across many runs, mention
that a testid audit on the affected component would convert the silent
re-rank into a no-op (testid primary survives the most kinds of churn).

#### Bucket B — Tier-2 healing staged

**Signal:** `$HEALING` or `$STAGING` exists and is non-empty.

```bash
test -s "$HEALING" && echo "Healing artefact present" && cat "$HEALING"
test -s "$STAGING" && echo "Staged recordings present" && cat "$STAGING"
```

What it means: every recorded selector strategy for one or more actions
failed. The runtime did an intent-aware redrive starting from the
partially-replayed page state, and staged the new recording to
`.cache/healing-staging.json` (NOT the main cache). Promotion is
manual.

**Action:**

1. Inspect what was staged: `pnpm test:agentic --inspect-cache` (this
   reads `bespoke-actions.json`; the staging file uses the same shape —
   `cat .cache/healing-staging.json | jq .` works too).
2. Open the trace for visual confirmation that the new selectors hit the
   right element:
   ```bash
   pnpm exec playwright show-trace web-app/tests/agentic/.traces/<flow>-<case>.zip
   ```
3. If the new selectors look right, promote them:
   ```bash
   pnpm test:agentic --accept-healing <flow>
   ```
   (Or invoke `/accept-agentic-healing <flow>`.) This moves the staged
   entries into `bespoke-actions.json`.
4. **Commit the cache diff** — `git diff
   web-app/tests/agentic/.cache/bespoke-actions.json` shows what
   changed. The new selectors are now the test's ground truth.

If the staged selectors look wrong (e.g. they target a button on a
different page because the redrive landed elsewhere), the underlying
product behaviour changed in a way the redrive can't recover from on its
own — escalate to bucket C.

The cache-invalidation taxonomy in
`internal-docs/agentic-browser-testing-spec.md` (§ Cache invalidation
taxonomy) covers this layer (`C2: all recorded strategies failed`).

#### Bucket C — Behavioral failure

**Signal:** test failed, `$HEALING` empty (no Tier-2 stage), `error`
field present on one or more `step_debug` entries, or `judge` ruled `no`
on an `expect:` claim.

What it means: either the product behaviour legitimately changed and the
flow's claims are stale, or a real bug snuck in.

**Action — read the trace + step_debug:**

1. Find the step that errored:
   ```bash
   jq '.flows[].cases[].runs[] | select(.error != null) | {step_index, error, tool_calls: [.step_debug[].tool_calls]}' "$LATEST_JSON" \
     | head -50
   ```
   Or scroll the markdown summary's per-step debug table.
2. Open the Playwright trace at the path printed in
   `step_debug[].trace_path`:
   ```bash
   pnpm exec playwright show-trace web-app/tests/agentic/.traces/<flow>-<case>.zip
   ```
   Frame-by-frame DOM, network, screenshots. Usually it's obvious what
   the LLM tried and where the page state diverged from expectations.
3. Decide the fix:
   - **The product behaviour changed legitimately.** Edit the flow YAML:
     update the `act:` text (which invalidates that step's cache entry —
     and every later step in the same case — and re-derives on next run),
     update the `expect:` claims, or both. If the change matches a
     canonical prelude shift, mirror the change in `canonical-prompts.md`
     too.
   - **The test exposes a real bug.** Don't change the test. Fix the
     product. The test stays as the regression guard.
   - **The page renders slower than the wait gates assume.** Either tighten
     a `wait_for:` with a longer `;timeout_ms=` suffix or add a
     `wait_for: "selector_hidden:<loading-spinner>"` before the screenshot
     / judge.
   - **Monaco / file-upload / aria-hidden quirk.** Reread the
     `Common failure modes` section in `web-app/tests/agentic/README.md`
     — the paid-for bugs are catalogued there.

#### Bucket D — Cache health concern

**Signal:** suspect stale cache entries. Symptoms include:

- Test passes locally but fails in CI on a fresh runner (or vice versa).
- `--inspect-cache` shows entries with `version` other than the current
  `CACHE_VERSION` (`3` today — check `runner/action-cache.ts`).
- A recent runtime change touched `runner/tool-registry.ts`,
  `runner/selectors.ts`, `runner/action-cache.ts`, or
  `runner/runtimes/bespoke.ts` and replays are doing surprising things.

What it means: the cache schema and the runtime got out of sync. The
runtime should ignore entries with mismatched `version` (per
`createActionCache().get()`), but in marginal cases a partial replay can
still happen.

**Action:**

1. If the runtime change should invalidate all cached entries: bump
   `CACHE_VERSION` in `runner/action-cache.ts`. This forces every entry
   to be ignored on read; the next run re-records cold. One-time cost,
   then the suite re-warms.
2. If only some entries are suspect: delete the cache file entirely:
   ```bash
   rm web-app/tests/agentic/.cache/bespoke-actions.json
   ```
   The next run will re-record everything cold. ~$2 for the full
   8-flow suite (per the steady-state cost model in
   `agentic-browser-testing-spec.md`).
3. If the CI cache and local cache disagree: the bucket cache key in
   `.github/workflows/agentic-tests.yaml` (`agentic-actions-${runner.os}-${matrix.flow.name}-${hashFiles(…)}`) might be picking up a stale restore. Force the
   CI cache to invalidate by editing a hashed input (any flow YAML, any
   runner source file). The next workflow run records fresh.

### 4. Report back

Show the dev:

1. **Classification**: which of the four buckets this falls into.
2. **The exact fix** for the bucket.
3. **What to commit**: if Tier-2 promotion, the cache diff; if
   behavioral fix, the flow YAML diff (and possibly the product fix in a
   separate commit); if cache-version bump, the `CACHE_VERSION` constant
   change.
4. **Verification command**: rerun the flow after the fix and confirm
   green:
   ```bash
   /run-agentic-tests <flow-stem>
   ```
5. **Fast CI loop** if iterating in CI:
   ```bash
   gh workflow run "CI check" --repo oxy-hq/oxygen-internal \
     --ref <branch> --field agentic_only=true
   ```

## Error handling

- **No `.results/` directory present locally.** The dev saw the failure
  in CI but hasn't reproduced. Two paths:
  1. Download the CI artefacts (`agentic-results-<bucket>` artifact)
     and replay from those.
  2. Reproduce locally via `/run-agentic-tests <flow-stem>`.
- **`healing.json` says staged, but staging file is empty.** Means a
  previous `--accept-healing <flow>` already promoted them. Confirm with
  `git status` — the promotion writes a diff to
  `bespoke-actions.json`. If the diff has been committed, the test is
  fine; if not, run `--inspect-cache` and walk the dev through committing
  the diff.
- **Test fails on a flow but `step_debug` is empty.** The flow failed
  before reaching any step (e.g. backend didn't start, YAML didn't parse).
  Tail `web-app/tests/agentic/.logs/backend.log` and read the YAML
  loader's last error.
- **CI shows red but local run is green.** Most often a cache health
  issue (bucket D). Try `--inspect-cache` to see what's recorded; if it
  looks stale, delete the file or bump `CACHE_VERSION`.

## Notes

- **Don't auto-promote Tier-2 recordings.** This is a hard rule. The
  staged recording lives in `.cache/healing-staging.json` specifically so
  a developer reviews the new selectors before they become ground truth.
  Use `/accept-agentic-healing <flow>` (or `pnpm test:agentic
  --accept-healing <flow>`) for the explicit promotion path.
- **Don't bump `CACHE_VERSION` casually.** It invalidates the entire
  suite, paying ~$2 in re-record cost for the current 8-flow suite.
  Only bump on intentional shape changes to the runtime that break
  replay semantics.
- **Don't delete `bespoke-actions.json` to "fix a flaky test".** Flaky
  warm replay is a runtime bug (probably a selector strategy that should
  have a fallback but doesn't). File against the runner — don't paper
  over.
- The full incident retrospective for the 2026-05-06 ClickHouse drop is
  in `internal-docs/agentic-browser-testing-spec.md` — the
  read-only-against-external-systems policy at the top of
  `web-app/tests/agentic/README.md` exists because of that incident. A
  failing flow that wants a new `seed_*` setup command targeting any
  host outside the `ALLOWED_BASE_URLS` allowlist (`localhost:3001` /
  `127.0.0.1:3001`) is a hard reject — the current `fixtures/reset.ts`
  surface (`reset_test_file` / `restore_demo_file:` / `goto:`) is
  intentionally local-only and any new setup command requires explicit
  design review.
