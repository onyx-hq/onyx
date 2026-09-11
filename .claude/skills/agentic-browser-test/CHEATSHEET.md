# Cheatsheet — `.flow.test.yml` by hand

For devs who want to author / repair a flow without invoking the skill, or
who want to read an existing flow and understand it. The
[`README.md`](https://github.com/oxy-hq/oxygen-internal/blob/main/web-app/tests/agentic/README.md)
in `oxy-hq/oxygen-internal` on `main` is the source of truth — re-read
it whenever you suspect this cheatsheet has drifted.

---

## File location & naming

```
web-app/tests/agentic/flows/<descriptive-kebab-name>.flow.test.yml
```

Lower-kebab. **Prefix the stem with the bucket it'll land in**:
`builder-…`, `semantic-…`, `chat-…`, `threads-…`, `ide-…`,
`metric-tree-…`. CI bucketing keys off this prefix.

Always include the `# yaml-language-server: $schema=` header pointing at
`json-schemas/flow-test.json` — IDE autocomplete and inline diagnostics
depend on it.

---

## Top-level shape

```yaml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json

name: <human-readable summary>     # required, free-form
target: <enum>                     # documentation hint; see schema for current enum
settings:
  runs: 1
  model: claude-sonnet-4-6
  judge_model: claude-haiku-4-5-20251001
  trace: on-failure                 # on-failure | always | never
  cache_actions: true
  max_steps: 30                     # upper bound on LLM tool-pick iterations per step
  backend_mode: local               # local | cloud — default 'local'
setup:                              # ordered fixture commands run before each case
  - <command>
cases:                              # at least one
  - name: <descriptive>
    tags: [<surface>, <classification>]
    steps:                          # ordered
      - act: |
          <text>
        cache_scope: shared         # 'shared' | 'flow' (default 'flow')
      - wait_for: <primitive>
    expect:                         # zero or more
      - assert: <claim>
      - judge:  <claim>
```

`target:` enum: `chat`, `ide`, `threads`, `onboarding`, `any`.

---

## `setup:` fixture commands (3)

Authoritative: `web-app/tests/agentic/fixtures/reset.ts:SetupCommand`.

| Command | Mode | Notes |
|---|---|---|
| `reset_test_file` | local | Empties `demo_project/test.sql`. Symlink-safe. |
| `restore_demo_file:<rel>` | local | Reverts `demo_project/<rel>` to `git show HEAD:demo_project/<rel>`. Used by flows that mutate demo files (builder editing `insights.app.yml`, semantic-builder-ask editing `sales_semantics/views/oxymart.view.yml`). Refuses paths escaping repo, containing `..`, or resolving through symlinks. |
| `goto:<path>` | both | Navigate to `<OXY_BASE_URL><path>`. |

The set is intentionally minimal — none of these can make a network call. No UI creates orgs any more, so a cloud-mode flow that needs one runs against a backend `oxy seed`ed beforehand (`scripts/verify-all.sh` phase 4).

**Don't invent commands.** Unknown setup commands throw at load time.

---

## `act:` — natural-language step

The LLM reads the page via `browser_snapshot` (compact a11y-tree, ≤12 kB)
and chooses one of the generic browser tools to act.

**Selector hierarchy** (the runtime auto-records 2–3 fallback strategies per
action in this order, see `runner/selectors.ts:materializeStrategies`):

1. `[data-testid=foo]` — most stable.
2. `role=button[name='Save']` — stable across CSS refactors.
3. `text=Save` — fragile against copy edits.
4. CSS class / structure — fragile against component refactors.

**Effective `act:` prompts:**

- Quote the testid verbatim: `Click [data-testid=agent-selector-button]`.
- Number sub-steps for tightly-coupled actions.
- Disambiguate when the page has duplicates.
- Tool-hint when the right primitive is non-obvious (Monaco → `browser_keyboard_type`).

**Tokens that don't pay rent** (the 2026-05-11 tightening dropped these):

- Rationale paragraphs explaining the verb choice.
- "Edit @insights" verb prefixes when Cmd+I auto-prepends `@insights`.
- Over-cautious disambiguators when the testid is unambiguous.
- Radix `DropdownMenu` prose when `role=menuitem[name='…']` carries the info.

**Atomic vs compound.** Default to atomic (one logical action per `act:`).
Use compound only when actions are causally coupled (e.g. Cmd+I dialog —
pressing Meta+i twice closes it). Cap compound at 5 actions.

---

## `wait_for:` — gate the next step (4 primitives)

Authoritative: `runWaitFor` in `runner/tool-registry.ts`.

| Primitive | What it waits for |
|---|---|
| `streaming_complete` | Chat / builder SSE stream has ended. |
| `network_idle` | Playwright `networkidle` (no requests for ~500 ms). |
| `selector:<sel>[;timeout_ms=<n>]` | Selector becomes visible. Default 30 s; override the suffix for build phases / agentic runs. |
| `selector_hidden:<sel>[;timeout_ms=<n>]` | Selector disappears. Use when the act/wait sequence finishes faster than the UI it triggers (e.g. warm replay screenshots before a `[data-testid=app-preview-loading]` spinner clears). |

After every `act:` whose post-condition is non-trivial, pair with a
`wait_for:` that names the gate proving the action worked.

---

## Tools (10)

Authoritative: `runner/tool-registry.ts:TOOLS`. Only the six state-changing
tools persist into the action cache.

| Tool | State-changing? | When to hint at it |
|---|---|---|
| `browser_snapshot` | no | Default; LLM calls without prompting. `region: "main"` or `region: "<css>"` for noisy pages. |
| `browser_click` | yes | Default for clicks; LLM picks without prompting. |
| `browser_type` | yes | For `<input>` / `<textarea>` with a stable selector. NOT for Monaco. |
| `browser_press_key` | yes | Single key or chord — `Enter`, `Meta+s`, `Control+Enter`. |
| `browser_keyboard_type` | yes | Required for Monaco — click `.monaco-editor` to focus, then `browser_keyboard_type`. |
| `browser_file_upload` | yes | `<input type="file">` (DuckDB onboarding wizard). Paths repo-relative; `..` and absolute paths refused. |
| `browser_navigate` | yes | Direct URL changes (vs clicking through). |
| `browser_screenshot` | no | Rarely used — judge already screenshots. |
| `browser_wait_for_selector` | no | 10 s visibility wait. Prefer `wait_for:` between steps over this in-step variant. |
| `browser_get_page_text` | no | Fallback when snapshot is too noisy. |

---

## `expect:` — assert vs judge

### `assert:` (free, deterministic)

Authoritative: `runner/judge.ts`.

```yaml
- assert: "selector <sel> is visible"
- assert: "selector <sel> is not visible"
- assert: "selector <sel> has attribute <attr>=<value>"
- assert: "selector <sel> has a non-empty value"   # inputValue(); controlled inputs
- assert: "selector <sel> does not contain text \"<text>\""  # textContent(); non-inputs
- assert: "text \"<exact text>\" is visible"
- assert: "save button is not visible"        # IDE-specific helper, 5 s waitFor
```

Use asserts for every structural claim. $0.

### `judge:` (~$0.002 each)

LLM-as-judge against current screenshot + DOM text. Use for soft semantic
claims that aren't reducible to a structural check. One per case for the
dev's stated success criterion in plain English.

---

## Action cache contract

Cache file: `tests/agentic/.cache/bespoke-actions.json`. Schema version
`CACHE_VERSION = 3` (in `runner/action-cache.ts`).

Every successful state-changing tool call (`browser_click`, `browser_type`,
`browser_press_key`, `browser_keyboard_type`, `browser_navigate`,
`browser_file_upload`) is recorded into the action cache **with 2–3 ranked
fallback selector strategies** materialized from the resolved DOM
(testid > role+name > text > css). On warm replay, the runtime walks
strategies by rank — the first that resolves wins. No LLM call.

**Cache key:**

- `cache_scope: flow` (default): `sha256(flow_file | case_name | step_index | step_text)`.
- `cache_scope: shared`: `sha256("shared|" + step_text)`. Lets two flows
  with byte-identical step text share one recording.

**Editing a step's text invalidates only that step's entry** (and forces
re-record of every later step in the same case — page state can't be
replayed past a hole). Adjacent cases / flows are unaffected.

**Invalidation is drop-and-redrive.** If a recorded selector strategy
fails *and* every fallback also fails, the entry's actions can't replay;
the runtime invalidates and re-derives. No partial-replay path.

### Tier-1 vs Tier-2 healing

- **Tier-1 silent re-rank:** a fallback strategy resolves on replay. The
  cache's strategy ranks update in place (winning strategy → rank 0). $0
  LLM cost. Logged as `selector_drift_events` for telemetry. **No action
  required.**
- **Tier-2 staged heal:** every recorded strategy fails. The runtime does
  an intent-aware redrive, stages the new recording to
  `.cache/healing-staging.json` (NOT the main cache), and writes a CI
  PR-comment summary. Promote with:
  ```bash
  pnpm test:agentic --accept-healing <flow>
  ```
  Then `git status` + commit the cache diff. Use `/accept-agentic-healing
  <flow>` for the wrapped command.

### `cache_actions: false`

Disables the whole cache. Operational only — use to force every step cold
for a benchmark. **Not** required for secret correctness — egress
substitution covers that.

---

## `${VAR}` substitution

`act:` text supports `${VAR}` placeholders. Validated at YAML load time
(missing variable throws). Substituted only at egress (Anthropic API send
+ Playwright dispatch). The action cache and result artifact always store
the placeholder.

Current `SECRET_ENV_VARS` allowlist (`runner/secrets.ts`):

- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`
- `GEMINI_API_KEY`
- `CLICKHOUSE_PASSWORD`
- `OXY_DATABASE_URL`

Adding a new secret requires extending this list. `redactArgs()` is a
defense-in-depth — it throws if a plaintext value still appears after
redaction. Values shorter than 8 chars are treated as stubs (e.g.
`GEMINI_API_KEY=empty`) and not substituted.

---

## CLI surface — `pnpm test:agentic`

Authoritative: `runner/cli.ts:parseArgs` + `runner/cli-modes.ts`.

### Run modes

```bash
pnpm test:agentic                                # all flows
pnpm test:agentic chat-ask                       # filename match (substring)
pnpm test:agentic chat-ask ide-save              # multiple positional filters (OR-combined)
pnpm test:agentic --tag critical                 # tag filter
pnpm test:agentic --output results.json          # also write JSON to <path>
HEADED=1 pnpm test:agentic <pattern>             # see browser
DEBUG=1 pnpm test:agentic <pattern>              # stream per-iteration LLM reasoning
pnpm test:agentic --no-auto-backend              # don't auto-spawn `oxy serve`
pnpm test:agentic --no-auto-frontend             # don't auto-spawn Vite
```

**Multi-positional substring filters** are how the CI bucket matrix passes
its flow list to the runner. Each positional arg is a substring; a flow
matches if its filename contains ANY of the listed substrings.

`--no-auto-backend` / `--no-auto-frontend` are the documented escape
hatches when the dev wants to drive their own `oxy serve` (e.g. to debug
with a persistent Postgres volume between runs).

### Non-execution subcommands

```bash
pnpm test:agentic --list                                 # enumerate flows + cases + tags
pnpm test:agentic --dry-run                              # validate YAML + lint + cost preview
pnpm test:agentic --inspect-cache                        # dump current cache contents
pnpm test:agentic --watch <pattern>                      # rerun --dry-run on YAML edit
pnpm test:agentic --check-coverage --staged              # staged paths from stdin (pre-commit hook)
pnpm test:agentic --check-coverage --path <p>            # explicit path
pnpm test:agentic --scaffold <name> --from <component>   # scaffold a flow from a component's testids
pnpm test:agentic --accept-healing <flow>                # promote staged Tier-2 recording
```

### Durability lint (`--dry-run`)

`runner/lint.ts` warns on (does not fail):

- `text-only-selector`: step uses `text=` without a testid or `role=…[name=…]`.
- `css-structure-selector`: step uses bare CSS class / structure selector.
- `no-selector-hint`: step has no explicit selector or `browser_*` tool hint.

Two ignored warnings is fine for canonical flows (file-input id selector +
`oxymart` text table-picker selector). More than two means the flow has
room to improve.

---

## CI mechanics

The agentic-tests job is a reusable workflow at
`.github/workflows/agentic-tests.yaml` (called from `ci.yaml` via
`workflow_call`, also dispatchable standalone). A `resolve-matrix`
setup job emits the matrix as JSON. Flows run in **6 domain
buckets** (not one job per flow):

| Bucket | Flows | Mode |
|---|---|---|
| `builder` | `builder-edits-app`, `builder-rejected-suggestion` | local |
| `semantic` | `semantic-builder-ask` | local |
| `ask-agent` | `chat-ask`, `chat-panel-agent-switch` | local |
| `threads` | `threads-list` | local |
| `ide` | `ide-save` | local |
| `metric-tree` | `metric-tree`, `metric-tree-scenario` | local |

Buckets share `backend_mode`. Adding a cloud-mode flow to a local-mode
bucket needs a bucket split first.

`ide-compile-error` is in `flows/` but **not in any bucket** — gated on a
Monaco SQL diagnostic service shipping.

### `agentic_only` fast-iteration dispatch

```bash
gh workflow run "CI check" --repo oxy-hq/oxygen-internal \
  --ref <branch> --field agentic_only=true
```

Cuts CI feedback from ~45 min to ~15 min by skipping typos / fmt-web /
build-web / smoke / E2E / cargo clippy / cargo nextest. Only the agentic
matrix + cargo build run.

### Bucket cache key

```
agentic-actions-${runner.os}-${matrix.flow.name}-${hashFiles(
  flows/*.flow.test.yml,
  runner/runtimes/bespoke.ts,
  runner/runtimes/replay.ts,
  runner/selectors.ts,
  runner/tool-registry.ts,
  runner/action-cache.ts
)}
```

Bucket-prefix `restore-keys` (`agentic-actions-${os}-${bucket}-`) bring in
the most recent cache file for the bucket so per-step entries warm-replay
even when a different flow's YAML moved.

---

## Common LLM failure modes & fixes

| Symptom | Fix |
|---|---|
| Step burns 30 s on a `browser_click` | Wrong selector — the model is waiting on Playwright's old default. The runtime uses 5 s; if you still see 30 s you're on an outdated branch. |
| Step takes 10+ iterations | Vague `act:` prompt. Add explicit selectors or numbered sub-steps. |
| `assert: "selector ... is visible"` flakes | Page renders late. Insert `wait_for: selector:<sel>` before the assertion, or use `judge:` (captures a fresh screenshot at evaluation time). |
| Warm replay screenshots before spinner clears | Insert `wait_for: "selector_hidden:[data-testid=…-loading]"` before the screenshot/judge step. |
| Monaco appears empty after typing | Use `browser_keyboard_type`, not `browser_type`. Click `.monaco-editor` first. |
| Save button assertion races (IDE flow) | Use `assert: "save button is not visible"` — the helper does a 5 s waitFor. |
| `claude-sonnet-4-7` returns 404 | Not yet GA on the account. Use `claude-sonnet-4-6`. |
| Step succeeds locally, fails in CI | Action cache replayed stale selectors. Try `--inspect-cache`. If Tier-2 healed, run `--accept-healing <flow>`. If `CACHE_VERSION` doesn't match, the entry is silently dropped — re-record. |

---

## Cost reference

| Scenario | Per-case cost |
|---|---|
| Cold (cache miss, well-anchored prompt) | ~$0.05–0.15 |
| Cold (cache miss, vague prompt) | ~$0.20–0.40 |
| Cold (cloud-mode onboarding) | ~$1.00 (build phase dominates) |
| Warm (cache hit, full flow) | ~$0.002–0.005 (judge floor) |

Per-step cost is in the markdown report at `tests/agentic/.results/<ts>.md`
plus the JSON next to it. Rates from `runner/pricing.ts`.

### `_budgets.yml`

Per-flow ceilings live in `web-app/tests/agentic/flows/_budgets.yml`. The
reporter compares observed cost against the ceiling and writes `⚠️` to the
markdown summary on overage (advisory only, never fails CI). Defaults
when adding a new flow:

| Shape | `cold_usd` | `warm_usd` |
|---|---|---|
| Simple (1–3 act steps, no builder/onboarding) | 0.30 | 0.005 |
| Builder / multi-step | 0.50 | 0.01 |
| Cloud-mode onboarding-class | 1.00 | 0.01 |

---

## Output artefacts

- `tests/agentic/.results/<ts>.md` — markdown summary (per-case pass/fail, per-step debug table, cost budget comparison). Auto-appended to `$GITHUB_STEP_SUMMARY` in CI.
- `tests/agentic/.results/<ts>.json` — same data programmatically.
- `tests/agentic/.results/healing.json` — CI artifact, non-empty when a Tier-2 heal happened. CI posts a PR comment with the contents.
- `tests/agentic/.traces/<flow>-<case>.zip` — Playwright trace on failure. `pnpm exec playwright show-trace <path>`.
- `tests/agentic/.cache/bespoke-actions.json` — action cache.
- `tests/agentic/.cache/healing-staging.json` — staged Tier-2 recordings awaiting `--accept-healing`.

---

## Hard rules — red lines

1. **Read-only against external systems.** Flows / fixtures must never seed, drop, or mutate any DB, warehouse, port-forward, or shared service. The setup-command surface (`fixtures/reset.ts`) is intentionally limited to `goto:`, `reset_test_file`, and `restore_demo_file:` — none of which can make a network call. Any proposed setup command that wants to call out is rejected at code review.
2. **Never type secrets as plaintext.** Use `${VAR}` placeholders from `SECRET_ENV_VARS`. Adding a new secret requires extending the allowlist.
3. **Never auto-promote Tier-2 healing.** Promotion is gated on `--accept-healing <flow>` so a human reviews the new selectors.

---

## When to *not* use this layer

- Behaviour without a UI footprint — Vitest in `web-app`, `cargo nextest` in Rust crates.
- Oxy agent / workflow output correctness — `oxy-test-drafter` skill + `*.agent.test.yml` / `*.aw.test.yml`.
- Backend HTTP contract — Hurl smoke tests at `tests/smoke/smoke.hurl`, or Rust integration tests next to the crate.
