---
name: agentic-browser-test
description: "Use when an Oxy dev or coding agent working in oxy-hq/oxygen-internal needs to create, update, maintain, or run/debug an agentic browser flow under web-app/tests/agentic/. Triggers include 'add a test for…', 'write a flow that…', 'this flow is failing', 'accept the healing recording', 'why did the agentic CI job fail?', 'regression test for the bug I just fixed'. Produces or repairs .flow.test.yml files and routes the dev through triage / healing / cache hygiene. Skip for unit tests (Vitest/cargo nextest), Oxy agent eval tests (.agent.test.yml / .aw.test.yml — the oxy-test-drafter skill handles those), or backend Rust integration tests."
---

# Agentic Browser Test Skill

You are the expert covering **all four reasons** a developer or coding agent interacts with the agentic browser testing system in `oxy-hq/oxygen-internal`:

1. **Creating** a new flow for a new feature or regression. (`/test-feature`)
2. **Updating** an existing flow when the product surface changes. (`/agentic-test-add-case`)
3. **Maintaining** the suite — accepting healing recordings, fixing broken flows, managing cache hygiene, calibrating budgets. (`/fix-agentic-test`, `/accept-agentic-healing`)
4. **Running / debugging** a failing flow locally or in CI. (`/run-agentic-tests`, `/fix-agentic-test`)

The dev should never have to learn the YAML schema, what `act:` / `wait_for:` / `expect:` is, how the action cache works, what Tier-1 vs Tier-2 healing means, or which bucket their new flow lands in. **You are the abstraction.**

---

## Step 0 — Always re-read source of truth (every invocation)

The runtime, schema, and authoring conventions evolve fast. **Before writing or repairing any YAML, read these source-of-truth files (this skill is colocated in the same repo, so paths below are repo-relative). Do not cache them across invocations.**

Canonical sources (PR #2275 shipped the suite; subsequent commits on `main` may have evolved the surface):

1. `web-app/tests/agentic/README.md` — runner mechanics, step kinds, `wait_for:` primitives, setup commands, action-cache contract, output format.
2. `json-schemas/flow-test.json` — schema for `.flow.test.yml`. Settings keys, enum values, defaults, validation rules.
3. `web-app/tests/agentic/canonical-prompts.md` — verbatim copy-pasteable step text for cross-flow `cache_scope: shared` reuse.
4. `web-app/tests/agentic/flows/*.flow.test.yml` — every existing flow. Canonical authoring examples.
5. `web-app/tests/agentic/flows/_budgets.yml` — per-flow cost ceilings. When generating a new flow, add an entry here.
6. `web-app/tests/agentic/runner/` — runtime source. The authoritative answer to "is this tool / primitive / fixture available?" is in `tool-registry.ts`, `fixtures/reset.ts` (note: `fixtures/` is a sibling of `runner/`), `cli.ts`, `action-cache.ts`, `selectors.ts`, `healing.ts`, `lint.ts`, `secrets.ts`.
7. `.github/workflows/agentic-tests.yaml` — reusable agentic-tests matrix (bucket layout, cache key formula, `flow_bucket` dispatch input). `ci.yaml` calls this via `workflow_call` and exposes the `agentic_only` fast-path.
8. `internal-docs/agentic-browser-testing-spec.md` — implementation deep-dive: runtime A/B verdict, post-A.x optimisations, v2 durability mechanism, cache invalidation taxonomy, steady-state cost model, incident retrospective, dated change log.
9. `internal-docs/agentic-browser-testing-team-overview.md` — team-facing CI mechanics + cost.

If anything in this SKILL conflicts with those files, **the files win**.

---

## Step 1 — Identify the user's mode

The same dev request can map to any of the four modes. Map intent to skill route:

| Phrase | Mode | Skill route |
|---|---|---|
| "add a test for…", "write a flow that…", "I just built X, test it" | Create | `/test-feature` |
| "add a case to chat-ask that covers…", "extend builder-edits-app to also…" | Update | `/agentic-test-add-case` |
| "this flow is failing in CI", "the trace shows…", "agentic/builder is red on my PR" | Run/debug | `/run-agentic-tests` for repro, then `/fix-agentic-test` for triage |
| "the runtime staged a healing recording — what do I do?", "PR comment says healing-staging.json is populated" | Maintain (Tier-2) | `/accept-agentic-healing <flow>` |
| "selector_drift_events went up but tests still passed" | Maintain (Tier-1) | No action needed — the cache silently re-ranked. Surface for awareness. |
| "bump CACHE_VERSION", "suite-wide cold rerun" | Maintain (cache hygiene) | `/fix-agentic-test` (cache health branch) |

Ambiguous: ask via `AskUserQuestion`. Don't guess across modes — creating a new flow when the dev wanted a case-add is wasted work.

### Trigger phrases that do NOT activate this skill (delegate)

- "write a unit test for …" → cargo nextest or Vitest.
- "write an oxy agent test" / "draft expecteds for my agent" → `oxy-test-drafter` skill handles `*.agent.test.yml` / `*.aw.test.yml`.
- "test the API endpoint …" → Rust integration tests.
- "test the workflow YAML" → likely `oxy-test-drafter`.

---

## Runtime primitives — the current surface

The runtime is the **bespoke** runtime (`@anthropic-ai/sdk` + a custom tool registry driving vanilla Playwright). It exposes exactly the tools / wait primitives / setup commands below. **Do not invent any others** — the loader and judge throw on unknown values.

### Tools (10) — authoritative: `runner/tool-registry.ts:TOOLS`

| Tool | State-changing? | Notes |
|---|---|---|
| `browser_snapshot` | no | Compact a11y-tree text (≤12kB). Optional `region: "main"` or `region: "<css>"`. |
| `browser_click` | yes | 5s timeout. |
| `browser_type` | yes | Fills (or appends with `append: true`). |
| `browser_press_key` | yes | Single key or chord (`Enter`, `Meta+s`, `Control+Enter`). |
| `browser_keyboard_type` | yes | Raw keyboard into the focused element. **Required for Monaco** — `browser_type`'s selector-based fill picks the hidden textarea wrong. |
| `browser_file_upload` | yes | Playwright `setInputFiles`. Selector points at `<input type="file">` (often hidden behind a styled drop zone — the onboarding wizard's id is `credential-<key>`, so `#credential-<key>` works). Paths array is repo-relative; absolute paths and `..` traversal refused by `runner/files.ts:resolveRepoFile()`. |
| `browser_navigate` | yes | Absolute URL or relative path. |
| `browser_screenshot` | no | PNG base64. Expensive; the judge already screenshots. |
| `browser_wait_for_selector` | no | 10s visibility wait. |
| `browser_get_page_text` | no | Truncated `body.innerText` fallback when snapshot is noisy. |

Only the six state-changing tools persist into the action cache (`tests/agentic/.cache/bespoke-actions.json`). Snapshots and screenshots are observation-only.

### `wait_for:` primitives (4) — authoritative: `runWaitFor` in `tool-registry.ts`

| Primitive | What it waits for |
|---|---|
| `streaming_complete` | Chat stream finishes (loading state appears, then stop button hides). 20s appear-window, 240s disappear-window. |
| `network_idle` | Playwright `networkidle` (no requests for 500ms). |
| `selector:<sel>[;timeout_ms=<n>]` | Element matching `<sel>` becomes visible. Default 30s; override via the suffix for legitimately-long waits (build phases, agentic runs). |
| `selector_hidden:<sel>[;timeout_ms=<n>]` | Element matching `<sel>` disappears. Use when the act/wait sequence finishes faster than the UI it triggers — the classic case is warm-replay screenshots before a `[data-testid=app-preview-loading]` spinner clears. |

### Setup commands (3) — authoritative: `fixtures/reset.ts:SetupCommand`

| Command | Mode | Notes |
|---|---|---|
| `reset_test_file` | local | Empties `demo_project/test.sql`. Refuses symlinks / paths escaping repo. |
| `restore_demo_file:<rel>` | local | Reverts `demo_project/<rel>` to `git show HEAD:demo_project/<rel>`. Used by flows that mutate a demo file (e.g. `builder-edits-app` editing `insights.app.yml`) so reruns start from the same canonical state. Refuses paths that escape the repo, contain `..`, or resolve through a symlink. Reads from HEAD without touching the index. |
| `goto:<path>` | both | Navigate to `<OXY_BASE_URL><path>` before the case starts. |

### Flow settings — authoritative: `json-schemas/flow-test.json`

```yaml
settings:
  runs: 1                                  # int, min 1
  model: claude-sonnet-4-6                 # default; haiku is too flaky as a pickup model
  judge_model: claude-haiku-4-5-20251001
  trace: on-failure                        # on-failure | always | never
  cache_actions: true                      # disables the whole cache when false
  max_steps: 30                            # upper bound on LLM tool-pick iterations PER step
  backend_mode: local                      # local | cloud — picks which oxy boot the runner auto-spawns
```

- `backend_mode: local` → `oxy start --local --enterprise` from `demo_project/`, runner targets `http://localhost:3000`.
- `backend_mode: cloud` → `oxy start --enterprise --clean` from repo root, runner targets the auth-disabled internal port `http://localhost:3001`. `--clean` wipes the Postgres volume, so it comes up with no org (and no UI creates one — orgs are staff/partner-made); a flow that needs org data runs against a backend you started and `oxy seed`ed first, as `scripts/verify-all.sh` phase 4 does. If a backend is already healthy at the resolved URL, the runner uses it as-is and does not respawn (no `--clean` side effect).
- **All flows in a single invocation must agree on `backend_mode`.** The runner errors loudly on mixed-mode loads.

### Step `cache_scope`

```yaml
steps:
  - act: |
      …canonical step text…
    cache_scope: shared      # 'shared' | 'flow' (default 'flow')
```

- `cache_scope: flow` (default): key = `sha256(flowFile|caseName|stepIndex|stepText)`. Recording is private to this flow/case/step.
- `cache_scope: shared`: key = `sha256("shared|" + stepText)`. Two flows with byte-identical step text share one recording. Used today for (see `canonical-prompts.md` for the verbatim text):
  - Chat-prelude ("Submit … to the default agent…") shared by `chat-ask` and `threads-list`.
  - Builder dialog open (Cmd+I + auto-approve toggle + submit) shared by builder flows.
  - Sidebar navigation: `sidebar-app-link-<app-name>` and `sidebar-thread-link-<thread-id>` for flows that click into an app or thread from the sidebar.

**Rule:** when copying a step **verbatim** from `canonical-prompts.md`, opt into `cache_scope: shared`. When the step body has flow-specific variation, leave at default `flow`.

### Expectations

- `assert: <claim>` — deterministic, $0. Supported forms (read `runner/judge.ts`):
  - `selector <sel> is visible` / `is not visible`
  - `selector <sel> has attribute <attr>=<value>`
  - `selector <sel> has a non-empty value` (via `inputValue()` — a controlled
    React input sets the value *property*, so `has attribute value=` misses it)
  - `selector <sel> does not contain text "<text>"` (via `textContent()`, for
    non-inputs such as a `SelectTrigger` button showing a placeholder)
  - `text "<text>" is visible`
  - `<description> is enabled`
  - `save button is not visible` (IDE-specific 5s waitFor helper)
- `judge: <claim>` — LLM-as-judge against current screenshot + DOM text. ~$0.002 per call with `claude-haiku-4-5-20251001`.

Reserve `judge:` for soft semantic claims. Never `judge:` something you can `assert:`.

---

## Authoring rules — these matter for cost and reliability

### Selectors: hierarchy + multi-strategy durability

Selector preference order (the runtime ranks fallbacks the same way in `selectors.ts:materializeStrategies`):

1. `[data-testid=foo]` — most stable. Survives copy edits, i18n, ARIA refactors.
2. `role=button[name='Save']` — stable across CSS/Tailwind refactors.
3. `text=Save` — fragile against copy edits.
4. CSS class / structure (`.foo button`) — fragile against component refactors.

**The runtime auto-records 2–3 fallback strategies per state-changing action.** Even a suboptimal primary selector gets durability fallbacks materialized from the resolved DOM at record time. Tier-1 healing silently re-ranks when a fallback wins. So a `text=` primary isn't fatal — but a recording that never lands on a testid primary defeats the durability story.

Grep `web-app/src/**/*.tsx` for `data-testid="…"` before authoring. When a button you need lacks a testid, **add one to the source component** as a one-line follow-up — it's the most stable long-term option.

### Prompt verbosity tradeoffs

Verbose `act:` prompts with explicit selectors converge faster on cold runs (1–2 LLM iterations vs 5–10), so per-step cost is **lower** despite more input tokens.

**Concrete patterns that earn their tokens:**
- Quote the testid verbatim: `Click [data-testid=agent-selector-button]`.
- Number sub-steps for tightly-coupled actions.
- Disambiguate when the page has duplicates: *"the file editor's Monaco surface — the TOP one, NOT the SQL results pane below"*.
- Tool hint when the right primitive is non-obvious: *"Use browser_keyboard_type (NOT browser_type — Monaco's hidden textarea makes selector-based fill unreliable)"*.

**Tokens that don't pay rent (the 2026-05-11 tightening pass dropped these):**
- Rationale paragraphs explaining why a verb was chosen. If the testid is correct, the rationale is dead weight.
- "Edit @insights" verb prefixes (the Cmd+I dialog auto-prepends `@insights`).
- Over-cautious disambiguators ("not the Ask mode toggle") when the testid is already unambiguous.
- Radix `DropdownMenu` prose — the `role=menuitem[name='…']` selector carries the same information.

Generate concise prompts. Rationale belongs in YAML comments above the step, not in the step body.

### Atomic vs compound `act:` steps

- **Default: atomic.** One logical user action per `act:` step. Each step is its own cache entry — editing one doesn't invalidate the others.
- **Compound only when actions are causally coupled.** The classic case is the Cmd+I builder dialog: pressing Meta+i twice closes it, so the open + auto-approve-toggle + type + submit sequence has to be one `act:`. **Cap compound steps at 5 actions max.**
- Measured: atomic cold $0.34 vs compound cold $0.42 for `ide-save`; warm tied at ~$0.002. Atomic usually wins on cold cost too.

### `wait_for:` placement

After every `act:` whose post-condition is non-trivial. Pair each user action with a wait that names the gate proving the action worked:
- chat submit → `wait_for: streaming_complete`
- navigation → `wait_for: "selector:<thing-on-the-new-page>"`
- loading spinner clear → `wait_for: "selector_hidden:[data-testid=app-preview-loading]"`
- network mutation → `wait_for: network_idle`

Without these gates, the next `act:` runs before the page is ready and either flakes or burns LLM iterations as the model figures out the page is mid-transition.

### Secrets

`${VAR}` placeholders in `act:` text. Validated at YAML load time (missing var throws). Substituted only at egress (Anthropic API send + Playwright dispatch). The action cache and result artifact always store the placeholder, never the plaintext.

Current `SECRET_ENV_VARS` allowlist (in `runner/secrets.ts`):
- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`
- `GEMINI_API_KEY`
- `CLICKHOUSE_PASSWORD`
- `OXY_DATABASE_URL`

Adding a new secret requires extending `SECRET_ENV_VARS`. `redactArgs()` is a defense-in-depth check — it throws if a plaintext value slips through. Values shorter than 8 chars (e.g. `GEMINI_API_KEY=empty`) are treated as test stubs and not substituted.

**`cache_actions: false` is no longer required for secret correctness** — egress-substitution covers it. Flip to false only for operational concerns like forcing every step cold for a benchmark.

### Budgets

Per-flow cost ceilings live in `web-app/tests/agentic/flows/_budgets.yml`. The reporter compares observed cost against the ceiling and surfaces a `⚠️` in the markdown summary on overage (advisory, never fails CI). **When generating a new flow, add a budget entry.**

Reasonable defaults:

| Shape | `cold_usd` | `warm_usd` |
|---|---|---|
| Simple (1–3 act steps, no builder / onboarding) | 0.30 | 0.005 |
| Builder / multi-step | 0.50 | 0.01 |
| Cloud-mode onboarding-class | 1.00 | 0.01 |

### `--scaffold` integration

When the dev has a component path in mind, prefer scaffolding from it:

```bash
pnpm test:agentic --scaffold <feature-name> --from <component-path>
```

`runner/scaffold.ts` extracts existing testids from the source and pre-populates an `act:` template with them. Less authoring work + better default selectors out of the gate.

---

## Hard rules — red lines

These must show up in every authoring + run command. Encoded in `web-app/tests/agentic/README.md`'s top-level policy section.

1. **Read-only against external systems.** Never seed/drop/mutate any database, warehouse, port-forward, or shared service from a fixture or flow. The setup-command surface in `fixtures/reset.ts` is intentionally limited to `goto:`, `reset_test_file`, and `restore_demo_file:` — none of which can make a network call. Data a flow reads (an org, a compiled workspace) is stood up from the shell beforehand with `oxy seed` / `scripts/seed-fixtures.sh`, never by the flow — and no UI creates orgs any more. Any proposed setup command that wants to call out is rejected at code-review.
2. **Never type secrets as plaintext.** Use `${VAR}` placeholders for any value in `SECRET_ENV_VARS`. Adding a new secret env var requires extending the allowlist.
3. **Never auto-promote Tier-2 healing recordings.** They stage to `.cache/healing-staging.json`, not `bespoke-actions.json`. Promotion requires `--accept-healing <flow>` so a developer reviews the new selectors before they become ground truth.

These three rules trace back to the 2026-05-06 ClickHouse incident retrospective in `internal-docs/agentic-browser-testing-spec.md`. Don't paper over them.

---

## CI mechanics — what hits the matrix

The agentic-tests job is a reusable workflow at `.github/workflows/agentic-tests.yaml` (called from `ci.yaml` via `workflow_call`, also triggerable standalone via `workflow_dispatch` with optional `flow_bucket` and `oxy_binary_run_id` inputs). A small `resolve-matrix` setup job emits the bucket matrix as JSON. Flows are grouped into **6 domain buckets** (not one job per flow):

| Bucket | Flows | Mode | Port |
|---|---|---|---|
| `builder` | `builder-edits-app`, `builder-rejected-suggestion` | local | 3000 |
| `semantic` | `semantic-builder-ask` | local | 3000 |
| `ask-agent` | `chat-ask`, `chat-panel-agent-switch` | local | 3000 |
| `threads` | `threads-list` | local | 3000 |
| `ide` | `ide-save` | local | 3000 |
| `metric-tree` | `metric-tree`, `metric-tree-scenario` | local | 3000 |

No bucket is cloud-mode: the one cloud bucket (`onboarding`) went with self-serve org creation.

Filename → bucket mapping for new flows:
- `builder-*` → `builder`
- `semantic-*` → `semantic`
- `chat-*` → `ask-agent`
- `threads-*` → `threads`
- `ide-*` → `ide`
- `metric-tree-*` → `metric-tree`

If a new flow doesn't match any prefix, surface to the dev: "This needs a new bucket entry in the `resolve-matrix` job's inline JSON in `.github/workflows/agentic-tests.yaml`. Buckets share `backend_mode`, so don't add a cloud-mode flow to a local-mode bucket — split the bucket by mode first. Also add the bucket name to the `flow_bucket` choice list in the workflow_dispatch trigger so dispatch UIs can target it."

**`ide-compile-error`** is in `flows/` but **NOT in any bucket** — gated on Monaco SQL diagnostic service shipping. Don't reference it as a CI-live example.

**Fast-iteration dispatch:** `agentic_only=true` on `workflow_dispatch` cuts CI feedback from ~45 min to ~15 min by skipping typos / fmt-web / build-web / smoke / E2E / cargo clippy / cargo nextest:

```bash
gh workflow run "CI check" --repo oxy-hq/oxygen-internal \
  --ref <branch> --field agentic_only=true
```

Bucket cache key:

```
agentic-actions-${runner.os}-${matrix.flow.name}-${hashFiles(
  'web-app/tests/agentic/flows/*.flow.test.yml',
  'web-app/tests/agentic/runner/runtimes/bespoke.ts',
  'web-app/tests/agentic/runner/runtimes/replay.ts',
  'web-app/tests/agentic/runner/selectors.ts',
  'web-app/tests/agentic/runner/tool-registry.ts',
  'web-app/tests/agentic/runner/action-cache.ts'
)}
```

Bucket-prefix restore-keys fall back to the most recent cache file for the bucket; per-step entries still warm-replay even when a different flow's YAML moved.

`continue-on-error: true` is on while calibrating. Flip-off criterion: ≥3 consecutive green main-branch runs.

After each run, if `.results/healing.json` is non-empty (Tier-2 heal happened) the job posts a PR comment summarizing drift events with the `--accept-healing` command. `/fix-agentic-test` walks the dev through what to do with that comment.

---

## Action cache + healing model

Cache file: `web-app/tests/agentic/.cache/bespoke-actions.json`. Schema version: `CACHE_VERSION = 3` (defined in `runner/action-cache.ts`).

### Three layers of invalidation

| Layer | Trigger | Blast radius |
|---|---|---|
| A | `CACHE_VERSION` bump | Entire cache. Used for shape changes (e.g. multi-strategy selectors in v3). |
| B | `act:` / `expect:` text edited; flow rename; case rename; earlier step inserted/removed. (With `cache_scope: shared`, only step text matters.) | That step redrives **and every subsequent step in the same case**. Other cases / flows unaffected. |
| C1 | A recorded fallback selector resolves (primary failed) | **No invalidation.** Strategies silently re-rank. $0. |
| C2 | All recorded strategies for one action fail | That step redrives cold via intent-aware redrive. New recording staged to `.cache/healing-staging.json` (NOT main cache). Same downstream blast as B until `--accept-healing` promotes it. |

### What does NOT invalidate

- Single testid rename when other strategies resolve.
- Label / copy / i18n edits.
- Tailwind / CSS refactors.
- Wrapper `<div>` added/removed.
- Prop changes that don't surface in DOM.
- Backend / non-UI changes.
- New flow that doesn't share a `cache_scope: shared` key.

### Tier-1 vs Tier-2

- **Tier-1 silent heal** (W1.3): a fallback resolved. The cache entry's strategies are re-ranked (winning strategy → rank 0). $0 LLM cost. Logged as `selector_drift_events` for telemetry. No developer action required — surface it for awareness only.
- **Tier-2 staged heal**: every recorded strategy failed. The runtime does an intent-aware redrive starting from the partially-replayed page state, stages the new recording to `.cache/healing-staging.json`, and writes a CI PR-comment summary. Promotion requires `pnpm test:agentic --accept-healing <flow>`.

The `/fix-agentic-test` command routes the dev through these tiers correctly. The `/accept-agentic-healing` command is the thin wrapper for Tier-2 promotion.

---

## File shape — what you produce

```yaml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json
#
# <one or two lines describing what this flow tests and why>

name: <human-readable summary>
target: <chat | ide | threads | onboarding | any>

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  max_steps: <pick an upper bound; default 30 is usually fine>
  backend_mode: <local | cloud>      # default 'local'; only set explicitly for cloud flows

setup:
  - <documented setup command>       # see the 3-item table above
  - "goto:/<path>"                    # land on the start page

cases:
  - name: <descriptive case name>
    tags: [<surface>, critical?, regression?]
    steps:
      - act: |
          <one logical user action, with explicit selectors>
        cache_scope: shared          # only if the step text is verbatim from canonical-prompts.md
      - wait_for: <documented primitive>
    expect:
      - assert: <deterministic claim>
      - judge:  <one sentence covering the success criterion>
```

Filename: `web-app/tests/agentic/flows/<descriptive-kebab-name>.flow.test.yml`. Lower-kebab. Bucket-prefix the stem (`builder-…`, `chat-…`, `threads-…`, `ide-…`) so CI bucketing is unambiguous.

---

## Self-check — before handoff

### 1. Parse-check via the runner's YAML loader

```bash
cd web-app
ANTHROPIC_API_KEY=fake-test-key \
  npx tsx -e "import('./tests/agentic/runner/yaml-loader.ts').then(m => m.loadFlow('tests/agentic/flows/<your-file>.flow.test.yml')).then(f => console.log('parsed:', f.name)).catch(e => { console.error(e); process.exit(1) })"
```

### 2. Schema-check against `json-schemas/flow-test.json`

```bash
npx --yes js-yaml web-app/tests/agentic/flows/<your-file>.flow.test.yml > /tmp/flow.json
npx --yes ajv-cli@latest validate -s json-schemas/flow-test.json -d /tmp/flow.json
```

### 3. `--dry-run` lint

Run the durability lint and surface findings to the dev:

```bash
cd web-app && pnpm test:agentic --dry-run <flow-stem>
```

The lint warns about `text=` only, bare CSS structure selectors, and steps with no selector hint at all. Two ignored warnings is fine for the current canonical flows (file-input id selector + `oxymart` text table-picker are the legitimate ones); more than two means the generated flow has room to improve — add testids / role+name selectors where the lint flagged.

### 4. Budget entry

When generating a new flow, append an entry to `web-app/tests/agentic/flows/_budgets.yml` using the defaults table above.

### 5. CI bucket assignment

Tell the dev which bucket the new flow lands in based on its filename prefix. If none matches, surface that they need to add a new bucket entry.

If any check fails, iterate. Don't hand off broken YAML.

---

## Handoff message

After the file is written and self-checks pass:

1. **Path to the new/updated file** + the bucket it lands in.
2. **First-run command**:
   ```bash
   HEADED=1 pnpm test:agentic <flow-stem>
   ```
   Or `/run-agentic-tests <flow-stem>` for the wrapped version. The runner auto-spawns the right backend based on `backend_mode`.
3. **Cost expectation** vs the budget you wrote into `_budgets.yml`.
4. **Where artefacts land**:
   - Markdown report: `web-app/tests/agentic/.results/<ts>.md`
   - JSON: `web-app/tests/agentic/.results/<ts>.json`
   - Trace on failure: `web-app/tests/agentic/.traces/<flow>-<case>.zip` — `pnpm exec playwright show-trace <path>`
5. **Slash commands the dev should know:**
   - `/test-feature <description>` — create a new flow.
   - `/agentic-test-add-case <flow> <description>` — extend an existing flow.
   - `/run-agentic-tests <pattern>` — run with HEADED=1 DEBUG=1.
   - `/fix-agentic-test <flow-or-bucket>` — triage a failing flow.
   - `/accept-agentic-healing <flow>` — promote staged Tier-2 healings.

---

## Future-proofing — design assumptions that may shift

The bespoke runtime is in active development. Re-read the README each invocation. Likely evolutions to watch:

- **Bucket layout per (domain, mode)** when cloud-mode coverage lands in `builder` / `ask-agent` / `threads` / `ide`. Today no CI bucket is cloud-mode; the cloud flows (`admin-*`, `airway-pipeline-run`) run only from `scripts/verify-all.sh`.
- **`cache_scope` collapsing to `shared: true` boolean** since only two values exist today.
- **Structured shorthand DSL** (`click:` / `type:` / `press:` step kinds that compile to Playwright tool calls without an LLM round-trip) — discussed for ~40–50× cold cost reduction per pure-mechanical step. Not committed.

When the dev asks for something the current README doesn't support, **say so explicitly** and offer a workaround using what's documented.

---

## Guardrails

- Never invent setup commands, `wait_for:` primitives, `assert:` forms, or CLI flags the README / source doesn't document. The loader and judge throw on unknowns or silently ignore them — either way the flow breaks.
- Never put real secrets in the YAML. Use `${VAR}` placeholders; ensure the var is in `SECRET_ENV_VARS`.
- Never edit a flow whose name suggests it's actively in use (`chat-ask`, `ide-save`, `threads-list`, etc.) when the dev is asking for a *new* flow. Create a new file. Use `/agentic-test-add-case` only when the dev explicitly says so.
- Never use `cache_actions: false` as a "make it work" hack. Flaky warm-replay is a runtime bug — file it against the runner.
- Never hand off YAML that hasn't passed parse + schema + dry-run lint.
- Never auto-promote Tier-2 healing recordings; route the dev to `/accept-agentic-healing` instead.

---

*Last verified against `oxy-hq/oxygen-internal` `main` after PR #2275 merged at `80023e576` (2026-05-19).*
