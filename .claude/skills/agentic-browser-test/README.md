# Agentic Browser Test Skill

End-to-end skill for the agentic browser-testing layer in
[`oxy-hq/oxygen-internal`](https://github.com/oxy-hq/oxygen-internal).
Covers all four reasons a developer or coding agent interacts with the
system:

1. **Creating** a new flow for a new feature or regression.
2. **Updating** an existing flow when the product surface changes.
3. **Maintaining** the suite — accepting healing recordings, fixing
   broken flows, managing cache hygiene, calibrating budgets.
4. **Running / debugging** a failing flow locally or in CI.

The dev never has to learn the YAML schema, what `act:` / `wait_for:` /
`expect:` is, what Tier-1 vs Tier-2 healing means, how the action cache
invalidates, or which CI bucket a new flow lands in.

## What it does

When you're working in a Claude Code session against `oxy-hq/oxygen-internal`
and you say something like:

> "test that clicking the new 'Reset' button clears the workspace"
>
> "add a regression test for the bug I just fixed where pressing Cmd+Enter
>  in the agent builder dialog submitted twice"
>
> "the agentic / builder job is red — figure out why"
>
> "I got a PR comment saying healing was staged — what now?"

…this skill writes / repairs / triages `.flow.test.yml` files under
`web-app/tests/agentic/flows/` and routes you to the right CLI subcommand.

## How it works

1. **Reads the source of truth on every invocation** — this skill is
   colocated in the same repo as the runner, so paths below are
   repo-relative (the suite shipped in PR #2275). The bespoke runtime,
   schema, CI matrix, and authoring conventions evolve fast. The skill
   re-reads:
   - `web-app/tests/agentic/README.md` — runner mechanics.
   - `json-schemas/flow-test.json` — schema.
   - `web-app/tests/agentic/canonical-prompts.md` — verbatim shared-scope
     step text.
   - `web-app/tests/agentic/flows/*.flow.test.yml` — canonical examples.
   - `web-app/tests/agentic/flows/_budgets.yml` — per-flow cost ceilings.
   - `web-app/tests/agentic/runner/` — runtime source.
   - `.github/workflows/agentic-tests.yaml` — reusable matrix
     (called from `ci.yaml` via `workflow_call`).
   - `internal-docs/agentic-browser-testing-{spec,team-overview}.md`.

2. **Picks the right primitives for what you described.**
   - `target:` — chat / ide / threads / onboarding / any.
   - `backend_mode:` — local (default, port 3000) or cloud (port 3001).
   - `setup:` — the 3 documented fixture commands
     (`reset_test_file`, `restore_demo_file:<rel>`, `goto:<path>`).
     None can make a network call; data a cloud-mode flow reads (an
     org — no UI creates one any more) is `oxy seed`ed beforehand.
   - `act:` — natural-language steps with explicit `[data-testid=…]`
     selectors when they exist (drops cold cost ~5×).
   - `wait_for:` — `streaming_complete` / `network_idle` /
     `selector:<sel>` / `selector_hidden:<sel>` (with optional
     `;timeout_ms=<n>` suffix) after each user action.
   - `cache_scope: shared` when copying canonical prelude text verbatim
     from `canonical-prompts.md`.
   - `assert:` for structural claims, `judge:` for the dev's stated
     success criterion.
   - `${VAR}` placeholders for secrets (`SECRET_ENV_VARS` in
     `runner/secrets.ts` — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
     `GEMINI_API_KEY`, `CLICKHOUSE_PASSWORD`, `OXY_DATABASE_URL`).

3. **Self-checks before handoff.**
   - Parses through `runner/yaml-loader.ts`.
   - Validates against `json-schemas/flow-test.json`.
   - Runs `pnpm test:agentic --dry-run <flow>` for the durability lint.
   - Appends a `_budgets.yml` entry.
   - Tells the dev which **CI bucket** the new flow lands in.

4. **For failures, triages by tier.** The skill knows the difference
   between Tier-1 silent re-rank (no action), Tier-2 staged heal (run
   `--accept-healing`), a behavioral failure (read the trace), and a
   cache-version drift (bump `CACHE_VERSION`).

## Slash commands

The skill ships five commands:

| Command | What it does |
|---|---|
| `/test-feature <description>` | One-shot generation. Skips Q&A, takes a free-form description, writes the YAML, runs the self-checks, adds a `_budgets.yml` entry, tells you the CI bucket. |
| `/agentic-test-add-case <flow-file> <description>` | Extends an existing flow with a new case rather than authoring from scratch. Useful for adding regression cases to a flow that already covers a feature. |
| `/run-agentic-tests <pattern>` | Runs the runner with `HEADED=1 DEBUG=1`. The runner auto-spawns the right backend (local vs cloud) based on each flow's `backend_mode`. Also explains the `agentic_only` fast-iteration dispatch for CI iteration. |
| `/fix-agentic-test <flow-or-bucket>` | Triage a failing flow. Reads the latest `.results/` summary + `healing.json`, classifies into Tier-1 / Tier-2 / behavioral / cache-health, walks the dev through the right fix. |
| `/accept-agentic-healing <flow>` | Thin wrapper around `pnpm test:agentic --accept-healing <flow>`. Shows the cache diff and reminds the dev to commit it. |

## CI buckets

The agentic-tests job is a reusable workflow at
`.github/workflows/agentic-tests.yaml` (called from `ci.yaml` via
`workflow_call`, also dispatchable standalone with optional
`flow_bucket` / `oxy_binary_run_id` inputs). A `resolve-matrix`
setup job emits the bucket matrix as JSON. Flows run in **6 domain
buckets**, not one job per flow:

| Bucket | Flows in CI | Mode |
|---|---|---|
| `builder` | `builder-edits-app`, `builder-rejected-suggestion` | local |
| `semantic` | `semantic-builder-ask` | local |
| `ask-agent` | `chat-ask`, `chat-panel-agent-switch` | local |
| `threads` | `threads-list` | local |
| `ide` | `ide-save` | local |
| `metric-tree` | `metric-tree`, `metric-tree-scenario` | local |

Filename → bucket mapping for new flows:

- `builder-*` → `builder`
- `semantic-*` → `semantic`
- `chat-*` → `ask-agent`
- `threads-*` → `threads`
- `ide-*` → `ide`
- `metric-tree-*` → `metric-tree`

A flow that doesn't match any prefix needs a new bucket entry in the
`resolve-matrix` job's inline JSON in `agentic-tests.yaml`. The skill
surfaces this.

## Action cache + healing

Every successful state-changing action gets recorded into
`tests/agentic/.cache/bespoke-actions.json` with 2–3 ranked fallback
selector strategies (`testid > role_name > text > css`). On warm replay,
the runtime walks strategies in order — no LLM call. Cold first-run cost
is ~$0.05–$1 per case (depending on shape); warm is ~$0.002–$0.005.

When a recorded primary selector drifts:

- **Tier-1 silent re-rank** — a fallback resolved. The cache entry's
  ranks update in place. $0 LLM cost. No developer action.
- **Tier-2 staged heal** — every recorded strategy failed. The runtime
  does an intent-aware redrive, stages the new recording to
  `.cache/healing-staging.json`, and posts a PR comment. Promote with
  `/accept-agentic-healing <flow>`.

## When to use it (and when not)

**Use this skill for:**

- Browser-driven UI tests of features in `oxy-hq/oxygen-internal`.
- Regression tests for UI bugs you just fixed.
- Triaging a failing agentic CI job.
- Promoting a staged Tier-2 healing recording.

**Do NOT use this skill for:**

- Unit tests — Vitest in the web app, `cargo nextest` in the Rust crates.
- Oxy agent / workflow eval tests (`*.agent.test.yml` / `*.aw.test.yml`)
  — the `oxy-test-drafter` skill handles those.
- Backend Rust integration tests — those live next to the crate code.

## What you get

A new file at `web-app/tests/agentic/flows/<bucket>-<descriptive-name>.flow.test.yml`
that:

- Uses the right `target:` and `backend_mode:` for the surface.
- Has a `setup:` block from the 3 documented fixtures only.
- Pairs each `act:` with a `wait_for:` so steps don't race.
- Uses explicit `[data-testid=…]` selectors when they exist (the skill
  greps `web-app/src/**/*.tsx` for testids before authoring).
- Opts into `cache_scope: shared` only when the step is verbatim from
  `canonical-prompts.md`.
- Substitutes secrets via `${VAR}` placeholders against the
  `SECRET_ENV_VARS` allowlist.
- Ends with one `judge:` covering the success claim, plus zero-or-more
  `assert:` checks for structural claims.
- Lands cleanly in a CI bucket — the skill tells you which.
- Has a matching entry in `_budgets.yml`.

## Reference docs

- [`SKILL.md`](SKILL.md) — full instructions Claude follows when handling
  any of the four modes.
- [`EXAMPLES.md`](EXAMPLES.md) — five worked examples covering the
  surfaces / patterns that ship in CI today (chat-ask shared prelude,
  ide-save Monaco quirk, builder-edits-app compound act, threads-list shared prelude, regression
  pattern).
- [`CHEATSHEET.md`](CHEATSHEET.md) — for devs authoring flows by hand:
  schema reference, full CLI surface, action-cache contract,
  Tier-1/Tier-2 healing narrative, common LLM failure modes.

For the full runtime mechanics — what tools the LLM gets, how the action
cache invalidates, what the judge does, how `agentic_only` cuts CI
feedback — read
[`web-app/tests/agentic/README.md`](https://github.com/oxy-hq/oxygen-internal/blob/main/web-app/tests/agentic/README.md)
in `oxy-hq/oxygen-internal` on `main`. That file is the source of
truth; this skill is the abstraction over it.
