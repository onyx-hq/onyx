# Agentic Browser Tests

Agent-driven browser tests written in YAML. The runner exposes only **generic browser tools** to an LLM (`browser_snapshot`, `browser_click`, `browser_type`, …) and lets the model read the page (accessibility tree) and act. There are no page-object wrappers in the registry, so UI churn (renamed test IDs, restructured menus) does not break the runner — the model just adapts at runtime.

The runtime lives in `runner/runtimes/bespoke.ts` (Anthropic SDK + Playwright with action-cache replay). The `Runtime` interface in `runtimes/interface.ts` keeps the seam open for swapping in a different runtime later (e.g. Stagehand v3) without touching the shared layers.

See [`internal-docs/agentic-browser-testing-spec.md`](../../../internal-docs/agentic-browser-testing-spec.md) for the deep dive: architecture, action-cache schema, selector materialization (v2 durability), CI integration, cost model + cache-invalidation taxonomy, change log, and the 2026-05-06 incident retrospective.

## Policy: read-only against external systems

**Flows and fixtures must never seed, drop, mutate, or otherwise destructively interact with any database, warehouse, port-forward, or shared service.** Only Oxy's own local state (its embedded Postgres in Docker, its workspace files on the local filesystem, the action-cache file) and committed-to-repo fixtures (DuckDB / Parquet / CSV under `demo_project/.db/`) may be written. Live warehouses, ClickHouse, Snowflake, BigQuery, Slack, GitHub — never.

This policy exists because of the [2026-05-06 incident](../../../internal-docs/agentic-browser-testing-spec.md#2026-05-06-incident) in which a `seed_clickhouse.sh` fixture, with defaults that exactly matched a `kubectl port-forward svc/clickhouse-pokehouse-chi 8123:8123` to the production cluster, ran during an unattended `--dangerously-skip-permissions` Claude Code session and dropped four tables (~3 years of menu line-item raw data, recovered slowly from Toast). The fixture and its consuming flow are gone; the structural fix is this policy plus the removal of fixture code that even *can* mutate an external system.

When authoring a new flow:

- Fixture data must be a file committed to this repo (DuckDB, Parquet, CSV) or generated deterministically into a temporary directory.
- The flow's `act:` prompts may type credentials only for local fixtures. If a step types a hostname, port, user, or password that could exist on a developer's `localhost`, do not use defaults that match a known production environment's port-forward.
- Do not add a `setup:` command that runs against an external HTTP endpoint, even one that "should be local."
- If a flow needs a warehouse, point it at DuckDB on a committed-to-repo file.

## Cold vs warm — what's actually being cached

There are **two independent caches** in the runtime. They serve very different purposes.

### 1. The action cache (the big lever)

A JSON file at `tests/agentic/.cache/bespoke-actions.json`. For every `act:` step that completes successfully, the runtime stores the recorded sequence of state-changing tool calls (`browser_click`, `browser_type`, `browser_press_key`, `browser_keyboard_type`, `browser_navigate`) — i.e. the actual deterministic actions the LLM ended up issuing.

- **Cold run** = action-cache miss for at least one step. The runtime opens an LLM loop (snapshot → tool-pick → dispatch → repeat) and re-derives the sequence. Typical cost per step: $0.05–0.20 input, depending on iterations.
- **Warm run** = action-cache hit. The runtime replays the recorded sequence directly against Playwright with **no LLM call**. Per-case cost floors at the judge call (~$0.002 with Haiku 4.5).
- **Invalidation** is drop-and-redrive: if a recorded selector no longer matches the page, the entry throws on replay, the runtime invalidates the entry, and re-derives from scratch. There is no partial-replay path.

The cache key is `sha256(flow_file | case_name | step_index | step_text)` by default — per-flow scope. Editing a step's text invalidates only that step's entry; adjacent steps in the same case still warm-replay. **Cross-flow reuse is opt-in via `cache_scope: shared`** on a per-step basis (key becomes `sha256("shared|" + step_text)`). Two flows with byte-identical step text that both opt into shared scope resolve to the same entry — record once, replay free across both. Canonical preludes live in [`canonical-prompts.md`](./canonical-prompts.md) — e.g. the chat prelude shared by `chat-ask` and `threads-list`.

`cache_actions: false` in flow settings disables the cache entirely (forces all steps cold).

### 2. Anthropic prompt cache (small lever, mostly invisible)

Anthropic's server-side prompt cache, reached via `cache_control: ephemeral` markers on the system prompt, the last tool definition, and the step prompt. 5-minute TTL, 4096-token minimum on Sonnet for an entry to actually materialise. Helps within a single multi-iteration step (iterations 2..N replay the prefix at 0.10× rate) and across consecutive same-session steps (the system+tools prefix stays warm).

You don't tune this — just be aware it's part of why cold cost numbers vary by 20–30% run-to-run.

### What this means for CI cost

Without cache persistence: every CI run on every PR pays cold cost for every step. ~$0.10–$0.40 per case on every push.

**With cache persistence** (already wired up, see the CI section below): the action cache is restored at the start of each CI run via `actions/cache` keyed on a hash of the flow files. Subsequent runs that don't touch flow text replay deterministically, dropping per-case cost to the judge floor (~$0.002).

For a **suite of 50 flows with overlapping UI traversal**, today there is no cross-flow reuse — every flow pays cold cost the first time even if it walks identical steps to another flow. The "shared cache key" mechanism in the followups doc would unlock that.

## Quick start

### 1. Install + configure

```bash
cd web-app
pnpm install
echo "ANTHROPIC_API_KEY=sk-ant-..." >> .env.local
```

### 2. Run

The runner picks the right backend boot for the loaded flows by reading each flow's `settings.backend_mode` (default `local`):

| `backend_mode` | spawn command (cwd) | runner targets |
|---|---|---|
| `local` (default) | `oxy start --local --enterprise` (`demo_project/`) | `http://localhost:3000` (auth-disabled in `--local`) |
| `cloud` | `oxy start --enterprise --clean` (repo root) | `http://localhost:3001` (auth-disabled internal port) |

Cloud mode passes `--clean` so the Postgres volume comes up empty and a flow that writes rows starts from the same state on a rerun. If a backend is already healthy at the resolved URL, the runner uses it as-is and does not respawn (no `--clean` side effect). All flows loaded in a single invocation must agree on `backend_mode` — the runner errors loudly if you mix.

Requires `oxy` on `PATH` and Docker Desktop running, since `oxy start` brings up Postgres in a container.

```bash
pnpm test:agentic                          # all flows in the default (local) mode set
pnpm test:agentic chat-ask                 # filename match
pnpm test:agentic --tag critical           # tag filter
pnpm test:agentic --output results.json    # write JSON (also auto-written under .results/)
HEADED=1 pnpm test:agentic chat-ask        # see browser
DEBUG=1 pnpm test:agentic chat-ask         # stream agent reasoning
pnpm test:agentic --no-auto-backend        # skip backend auto-start
pnpm test:agentic --no-auto-frontend       # skip Vite auto-start
```

Set `OXY_BIN=$PWD/target/debug/oxy` if your system `oxy` on PATH is older than your local debug build (a common cause of `--enterprise: unrecognized argument`).

Override the resolved URL with `OXY_BASE_URL` / `OXY_HEALTH_URL` if you want the runner to drive a backend you already have running on a non-default port.

## Authoring a flow

Files live in `flows/<name>.flow.test.yml`. Schema: [`json-schemas/flow-test.json`](../../../json-schemas/flow-test.json).

```yaml
name: chat ask returns SQL artifact
target: chat               # documentation hint only

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  max_steps: 15

setup:
  - "goto:/"

cases:
  - name: ask a question and get a SQL artifact
    tags: [chat, critical]
    steps:
      - act: |
          Submit a question to the analytics agent on the home page chat panel.
          The demo ships a single agent (`analytics`), selected by default:
          1. Type 'What were the total weekly sales by store?' into textarea[name=question].
          2. Click [data-testid=chat-panel-submit-button].
      - wait_for: streaming_complete
    expect:
      - judge: "the analytics agent answered with a coherent, data-backed result about weekly sales per store, not an error"
```

### Writing effective `act:` prompts

The model needs **enough specificity to pick selectors correctly first try.** Vague prompts cost 5–10× more because each wrong selector burns 5s before the model adjusts. Concrete patterns that work:

- **Explicit data-testid when stable**: `Click [data-testid=agent-selector-button]`. The model uses it verbatim.
- **Numbered sub-steps in one prompt** when the actions are tightly coupled — see the chat-ask example above. The whole sequence stays atomic from the cache's perspective but the model plans ahead.
- **Disambiguators when the page has duplicates**: `Click the file editor's Monaco surface (selector .monaco-editor) — the top one, NOT the SQL results pane below`.
- **State changes worth waiting for**: `After clicking, the URL should change to /ide/files/<base64>` — orients the model on what success looks like.
- **Tool hints when the right primitive is non-obvious**: for Monaco, `Use browser_keyboard_type (NOT browser_type — Monaco's hidden textarea makes selector-based fill unreliable)`. The model respects these hints.

What to avoid:

- ❌ Pure natural language without selectors when the page has multiple matching elements (the agent selector page has the word "default" in 3 places — without a testid, the model picks the wrong one).
- ❌ "Verify X" steps that don't actually act on the page — use `expect: assert:` or `expect: judge:` instead.
- ❌ `force: true` on `browser_snapshot` (the model can pass it but rarely should — the in-turn snapshot cache is automatically refreshed by state-changing tools).

### Atomic vs compound steps

The runtime always lets the LLM chain multiple tool calls within one step's turn — there's no `compound: true` flag. The choice between **atomic** (one short prompt per logical action) and **compound** (one long prompt that drives a full sub-sequence) is purely a YAML authoring decision.

- **Atomic** wins when individual actions are independently meaningful or when one part of the flow churns more than the rest (each step is its own cache entry, so the changed part re-derives but the rest still warm-replays).
- **Compound** wins when the sub-sequence is short and tightly coupled — fewer step boundaries means smaller cumulative LLM context per iteration.

Measured 2026-05-04 for the existing flows: **atomic wins** for `ide-save` (cold $0.34 atomic vs $0.42 compound; warm tied at ~$0.002). `chat-ask` is already minimal (one act + one wait_for) — no choice to make.

When authoring a new flow, write one variant first; if cold cost exceeds ~$0.30/case, try the other variant and commit the winner.

### Step types

- `act: <text>` — natural language. The LLM reads the page via `browser_snapshot` and chooses generic browser actions.
- `wait_for: <primitive>` — built-in waits:
  - `streaming_complete` — the chat stream finishes (loading state appears, then the stop button hides).
  - `network_idle` — Playwright's `networkidle` (no network activity for 500ms).
  - `selector:<sel>` — element matching `<sel>` becomes visible. Optional `;timeout_ms=<n>` suffix overrides the default 30s wait (use for legitimately-long waits like the agentic build phase).
  - `selector_hidden:<sel>` — element matching `<sel>` disappears. Counterpart to `selector:`; same `;timeout_ms=<n>` suffix. Use this when the act step finishes faster than the UI it triggers (e.g. warm-replay screenshots before a `[data-testid=app-preview-loading]` spinner clears).

## Generic browser tools

Defined in `runner/tool-registry.ts`. Available to the LLM in every step:

| Tool | Use |
|---|---|
| `browser_snapshot` | Compact a11y-tree text. Always call this first. ≤12kB. Optional `region: "main"` or `region: "<css selector>"` to scope. |
| `browser_click` | Click by Playwright selector (text=, role=, [data-testid=…]). 5s timeout — wrong selectors fail fast. |
| `browser_type` | Fill or append into an input/textarea. 5s timeout. |
| `browser_press_key` | Single key or chord (Enter, Meta+s, …). |
| `browser_keyboard_type` | Type via raw keyboard into the focused element. Use for Monaco. |
| `browser_file_upload` | Attach files to an `<input type="file">` via Playwright's `setInputFiles`. Built for the workspace setup wizard's DuckDB upload step (no committed flow uses it since `onboarding-blank-workspace` was deleted). Paths are repo-relative; absolute paths and `..` traversal are refused. |
| `browser_navigate` | Go to a URL. |
| `browser_screenshot` | PNG base64. Expensive; the judge already screenshots, so prefer `browser_snapshot`. |
| `browser_wait_for_selector` | Wait up to 10s for visibility. |
| `browser_get_page_text` | Truncated `body.innerText` fallback when snapshot is too noisy. |

Only the six state-changing tools (`click`, `type`, `press_key`, `keyboard_type`, `navigate`, `file_upload`) are recorded into the action cache. Snapshots and screenshots are observation-only.

## Expectation types

- `assert: <claim>` — deterministic, evaluated by `runner/judge.ts`. Supported forms:
  - `selector <sel> is visible` / `is not visible`
  - `selector <sel> has attribute <attr>=<value>`
  - `selector <sel> has a non-empty value` — reads `inputValue()`. Use this for
    pre-filled inputs: a controlled React input sets the value *property*, so
    `has attribute value=` cannot see it.
  - `selector <sel> does not contain text "<text>"` — reads `textContent()`.
    Use this for non-inputs, e.g. asserting a shadcn `SelectTrigger` (a button)
    is no longer showing its placeholder.
  - `text "<text>" is visible`
  - `<description> is enabled` (for follow-up input, etc.)
  - `save button is not visible` (waits up to 5s for the IDE save button to hide)
- `judge: <claim>` — LLM-as-judge against current screenshot + DOM text. Cheap with `claude-haiku-4-5-20251001`.

Asserts cost $0; judge calls cost ~$0.002 each. Use asserts wherever the claim is structural; reserve judge for soft semantic claims ("the response is coherent and not an error").

## Setup commands

- `reset_test_file` — empties `demo_project/test.sql` so the IDE save flow starts clean on rerun. Refuses (loud throw) if the resolved path is a symlink or escapes the repo root.
- `restore_demo_file:<rel>` — reverts `demo_project/<rel>` to its committed-in-HEAD content via `git show HEAD:demo_project/<rel>`. Used by flows that mutate a demo file (e.g. the builder agent editing `insights.app.yml`) so reruns start from the same canonical state. Refuses paths that escape the repo, contain `..`, or resolve through a symlink. Reads from HEAD without touching the index, so a developer's staged changes elsewhere are unaffected.
- `goto:/path` — navigate to a URL relative to `OXY_BASE_URL` (default `http://localhost:3000`).

The set is intentionally small. New setup commands are subject to the read-only-against-external-systems policy at the top of this file — propose any addition that needs network access on the followups doc first.

### Seeding the data a flow reads — `scripts/seed-fixtures.sh`

Because the list above cannot reach a database, **no flow seeds its own fixtures**. Anything a flow needs to exist — a workspace, a compiled + promoted revision, a published custom app, a partner org — is stood up from the shell before the runner starts:

```bash
scripts/seed-fixtures.sh fleet     # the docker fleet: seeds via the ide container, verifies at a replica
scripts/seed-fixtures.sh native    # a running `oxy serve`/`oxy start`: seeds via ./target/debug/oxy
scripts/seed-fixtures.sh check     # verify only, no writes
scripts/seed-observability.sh      # ClickHouse spans + metric usage for the four observability flows
```

`just verify` runs the right one for each phase, so you usually do not call these by hand.

Two things worth knowing before you debug a red flow:

- **It verifies over HTTP, not SQL.** Every check is a real request against the same routes the browser uses, because a row that exists but 403s, 404s, or hangs off an unpromoted revision is indistinguishable from no row at all from where Playwright sits. Exit 1 means "seeded, but a fixture is missing from the node the flows drive" — start there before blaming the product.
- **Identity is half the fixture.** The runner injects exactly one session per invocation. `flow@oxy.local` is an org owner with no platform standing and is what every workspace-scoped flow needs; the SPA bounces staff off a tenant workspace into `/admin`, so a workspace flow signed in as staff times out on a testid that was never going to render. The admin and partner surfaces need the opposite. `scripts/verify-all.sh --group b --dry-run` prints which identity each invocation uses, for free.

### Environment-variable substitution in `act:` text

`act:` step text supports `${VAR_NAME}` placeholders. The runner validates them at YAML load time (a missing variable throws), and substitutes the real values **only at the egress boundaries** — when the prompt is sent to the Anthropic API and when a state-changing tool dispatches into Playwright. Everywhere else (step text in result JSON, recorded actions in `.cache/bespoke-actions.json`, debug `tool_calls` args) the placeholder is what's stored.

```yaml
- act: "Type ${ANTHROPIC_API_KEY} into the API key input."
```

Concretely, on a flow that types `${ANTHROPIC_API_KEY}`:

- The Anthropic API receives the literal key (necessary so the LLM knows what to type).
- Playwright receives the literal key (so the input fills correctly).
- The action cache stores `browser_type({selector: "#secure-input", text: "${ANTHROPIC_API_KEY}"})`.
- The result artifact (`agentic-results-<flow>.json`) stores the same placeholder.

The allowlist of redacted env vars lives in `runner/secrets.ts` (`SECRET_ENV_VARS`). Add new entries when a flow needs them. The redactor errors loudly if an allowlisted plaintext value would still reach disk — defense in depth.

Older flows under `cache_actions: false` were written before egress substitution shipped. The flag now protects only against operational concerns (e.g. wanting to force every step cold for a benchmark); it is no longer required for secret-handling correctness.

### Cloud-mode flows

Flows that need the multi-tenant (org → workspace) shape declare `backend_mode: cloud` in their settings. The runner spawns `oxy start --enterprise --clean` and drives the auth-disabled internal port (3001). `--clean` wipes the local oxy Postgres volume, so any orgs/workspaces from previous runs are gone and every run starts from a fresh DB.

Driving the **authenticated** public port (3000) instead — worth it when you're debugging something auth-shaped by hand, or pointing Playwright MCP at a running server — no longer requires OAuth or the magic-link email preview: set `OXY_DEV_LOGIN_EMAILS` and navigate to `/dev-login` (see the "Dev sign-in" section in `DEVELOPMENT.md`). Committed flows still use 3001; nothing here changes.

No CI bucket runs a cloud-mode flow today. The last one, `onboarding-blank-workspace`, walked self-serve org creation (welcome → create org → skip invite → blank workspace → setup wizard), and that path no longer exists: orgs are provisioned by Oxygen staff or a partner, every new org gets a Default workspace, and a user with no org lands on a "not part of an organization yet" page with only invites, join-by-link and log out. A cloud-mode flow therefore needs an org to exist before it starts — there is no UI to create one outside the admin console. The committed cloud flows (`admin-*`, `airway-pipeline-run`) run from `scripts/verify-all.sh` phase 4, which starts `oxy start --enterprise` itself and runs `oxy seed` before the runner reuses that backend (a runner-spawned `--clean` one would come up empty). Whatever it uses, keep the warehouse file-based (DuckDB), so the flow stays structurally incapable of hitting a port-forward to production — the failure mode of the 2026-05-06 incident. `builder-edits-app` runs in local mode against `demo_project/insights.app.yml` directly, with a `restore_demo_file:insights.app.yml` setup command to revert the builder's edits between runs.

### Driving a fleet or remote deployment (bypassing `backend_mode`)

`backend_mode` only controls what the runner **auto-spawns** — `local` runs `oxy
start --local --enterprise`, `cloud` runs `oxy start --enterprise --clean`
(`runner/backend.ts:172`, `spawnArgs`). It is not a fixture selector and does not
declare which deployment topology a flow supports. Pass `--no-auto-backend` to skip
spawning entirely (`runner/cli.ts:170-172`) and set `OXY_BASE_URL` / `OXY_HEALTH_URL`
/ `OXY_SESSION_TOKEN` / `OXY_SESSION_USER` yourself to point the runner at an
already-running backend — including one replica of the Docker split fleet
([`docker-compose.fleet.yml`](../../../docker-compose.fleet.yml), see
[`internal-docs/local-split-fleet-testing.md`](../../../internal-docs/local-split-fleet-testing.md)).
`scripts/fleet-assert.sh --flows` does exactly this. The mixed-`backend_mode` refusal
still applies to whatever flows are loaded in one invocation, though —
`pickBackendMode` runs unconditionally, before `--no-auto-backend` is even checked
(`runner/cli.ts:159`, `:289-297`) — so `--no-auto-backend` only skips the spawn, not
the one-mode-per-invocation rule. This is also why the fleet-eligible flows stay on
the default `local` mode rather than flipping to `cloud`: `cloud` only changes which
command gets auto-spawned, so flipping it would break the plain `--local` path for
flows driven directly at a fleet, for no gain.

Two structural limits worth knowing before trying to widen fleet coverage:

- **One session per invocation.** `runtimes/bespoke.ts` reads `OXY_SESSION_TOKEN` /
  `OXY_SESSION_USER` once and injects them into a single Playwright browser context
  used for every case in the run (`runner/runtimes/bespoke.ts:122-156`). Admin flows
  need a staff identity; workspace-scoped flows need a non-staff one — staff reach is
  not standing, so a Global Owner gets bounced off every tenant workspace into
  `/admin` (see `product-context.md`, "Roles"). No single invocation can cover both
  today; that needs a per-flow `identity:` field in the schema, which doesn't exist
  yet.
- **`applyPathPrefix` and non-workspace-scoped routes.** `goto:` targets are prefixed
  with `OXY_PATH_PREFIX` so the same flow text resolves to a different URL under
  `--local` vs. a fleet/cloud deployment (`runner/backend.ts:79-86`). Routes that hang
  off the app root instead of a workspace — `TOP_LEVEL_SURFACES`: `/admin`,
  `/partners`, `/customer-apps`, `/login`, `/dev-login`, `/invite`, `/cli-auth`
  (`runner/backend.ts:101-109`) — must be listed there, or the prefix produces a URL
  that routes nowhere. Three admin flows failed exactly that way before the list
  existed, each looking like an unrelated broken page rather than one misconfigured
  harness.

## Output format

Every run produces two artifacts under `tests/agentic/.results/<iso-timestamp>.{json,md}`:

- A machine-readable JSON file with full per-step debug data, token usage, and USD cost.
- A markdown summary with one row per case run plus a per-step debug table per run. Designed to be readable in a GitHub Actions step summary (auto-appended to `$GITHUB_STEP_SUMMARY` in CI) or in a terminal.

If you pass `--output <path>.json`, the JSON is also written there.

JSON shape (see `runner/types.ts:RunResults`):

```json
{
  "runtime": "bespoke",
  "started_at": "2026-05-04T12:00:00.000Z",
  "duration_ms": 23456,
  "cost_usd": 0.0123,
  "pricing_as_of": "2026-05-04",
  "flows": [{
    "name": "...",
    "cases": [{
      "runs": [{
        "passed": true,
        "duration_ms": 23456,
        "tokens": { "input": 1200, "cached_input": 9500, "cache_creation": 0, "output": 80 },
        "cost_usd": 0.0098,
        "cache_hits": [true, false],
        "step_debug": [
          {
            "step_index": 0,
            "kind": "act",
            "text": "...",
            "duration_ms": 2400,
            "iterations": 1,
            "model": "claude-sonnet-4-6",
            "tokens": { "input": 12000, "cached_input": 9500, "cache_creation": 0, "output": 80 },
            "cost_usd": 0.0098,
            "from_cache": false,
            "tool_calls": [{ "name": "browser_click", "ms": 120, "args": { "selector": "..." } }],
            "snapshot_bytes": 11800,
            "snapshot_calls": 2,
            "snapshot_cache_hits": 0
          }
        ],
        "judge_usage": {
          "model": "claude-haiku-4-5-20251001",
          "calls": 1,
          "tokens": { "input": 1500, "cached_input": 0, "cache_creation": 0, "output": 50 },
          "cost_usd": 0.0017
        },
        "expect_results": [
          { "kind": "assert", "passed": true, "claim": "...", "evidence": "visibility(...)=true" }
        ],
        "trace_path": "tests/agentic/.traces/...zip"
      }]
    }]
  }]
}
```

`cost_usd` at the run level is the sum of all per-step costs **plus** that run's judge cost. The top-level `cost_usd` is the grand total across all runs in the invocation. `pricing_as_of` records when the rate table was last verified — see `runner/pricing.ts` to update rates.

### Step debug fields

Each entry in `step_debug` describes one step in `case.steps` (in order):

| Field | Meaning |
|---|---|
| `step_index` | 0-based index of the step in the case |
| `kind` | `act` (LLM-driven) or `wait_for` (no LLM) |
| `text` | The raw `act:` prompt or `wait_for:` primitive |
| `duration_ms` | Wall-clock time the step took |
| `iterations` | Number of LLM tool-pick iterations (`0` for cache hits and wait_for) |
| `model` | Model that handled this step. Undefined for cache hits. |
| `tokens` / `cost_usd` | Per-step usage and USD cost under `model` |
| `from_cache` | True if this step replayed from the action cache (no LLM call) |
| `tool_calls` | Ordered list of tools invoked during this step, with args (truncated where long) and ms |
| `snapshot_calls` / `snapshot_cache_hits` / `snapshot_bytes` | Snapshot stats for the step |
| `escalated` / `initial_model` | Reserved for future haiku→sonnet escalation; undefined today |
| `error` | Step error message if the step threw |

A failing run's `step_debug` is the first thing to read when triaging — `iterations`, `tool_calls` (with args), and `error` together usually pinpoint the failure.

## Debugging a flow

1. Run with `HEADED=1 DEBUG=1` to watch the browser and see per-iteration LLM decisions.
2. Read the latest `tests/agentic/.results/<ts>.md` for a per-step cost + tool-call summary.
3. For deeper diagnosis, open the `.json` from the same timestamp and inspect `step_debug[].tool_calls` — every tool call shows args (the selector the model tried) and any error.
4. If the case failed mid-stream, open the Playwright trace at the path printed in `step_debug[].trace_path`: `pnpm exec playwright show-trace tests/agentic/.traces/<flow>-<case>.zip`.
5. If a single step is consuming many iterations (>5), read the prompt — usually it's missing a disambiguating selector or a required pre-condition (e.g. clicking a button that's only enabled after a previous action).

Common failure modes and fixes:

| Symptom | Fix |
|---|---|
| Step burns 30s on a `browser_click` | Wrong selector — model is waiting on Playwright's old default. The runtime now uses 5s; if you still see 30s, you're on an outdated branch. |
| Step takes 12+ iterations | Vague `act:` prompt — add explicit selectors or numbered sub-steps. |
| `assert: "selector ... is visible"` flakes | The page renders the element late. Insert a `wait_for: selector:<sel>` step before the assertion, or use `judge:` (which captures a screenshot at evaluation time). |
| Save button assertion races (IDE flow) | `save button is not visible` already does a 5s waitFor — extend if your flow takes longer to flush. |
| Monaco appears empty after typing | Use `browser_keyboard_type`, not `browser_type`. Click `.monaco-editor` first to focus. The 25ms keystroke delay is built in. |
| `model: claude-sonnet-4-7` returns 404 | Not yet GA on the account. Use `claude-sonnet-4-6` until 4-7 ships (see `runner/yaml-loader.ts:DEFAULT_SETTINGS.model`). |

## Cost expectations

| Scenario | Per-case cost | Notes |
|---|---|---|
| Cold (cache miss) | $0.05–0.40 | Variance is mostly LLM iteration count. A well-written `act:` prompt with explicit selectors tends to converge in 1–4 iterations and lands ~$0.05; a vague prompt that has the model second-guessing burns 10+ iterations and lands at the high end. |
| Warm (cache hit, full flow) | $0.002–0.005 | Just judge calls. Replay is microseconds. |
| Cold first-ever run, full suite of 50 cases | $5–$20 estimated | One-time. Fund this against the plan to ship CI persistence. |
| Subsequent CI runs, same flows | $0.10–0.25 | Just judge calls × N cases. |
| CI run with 1 flow's text edited | $0.10 + (cold cost for the edited flow) | Other 49 cache-hit. |

The cost reporter writes `cost_usd` per step, per run, and per total. Trust those numbers for budgeting — they apply Anthropic's published rates (input × 1× / cache-read × 0.10× / cache-write × 1.25× / output × 1×) per the table in `runner/pricing.ts`.

## CI

CI integration lives in `.github/workflows/agentic-tests.yaml`, a reusable workflow that `.github/workflows/ci.yaml` calls via `workflow_call`. The same file can also be triggered standalone via `workflow_dispatch` (with a `flow_bucket` input to run a single bucket, and an `oxy_binary_run_id` input pointing at a prior CI run's binary artifact). It runs on every PR (and on pushes to main, plus manual `workflow_dispatch`) when the `web-app` or `oxy` change groups are non-empty.

A `resolve-matrix` setup job emits the 6-bucket matrix as JSON; the main `agentic-tests` job consumes it via `strategy.matrix.flow`. Each sub-job:

- Downloads the prebuilt CI binary via the `download-oxy-binary` composite action (`.github/actions/download-oxy-binary/`), which supports both same-run and cross-run downloads.
- Sets up pnpm + Node + Playwright (cached) via the `setup-web-app-test-env` composite action (`.github/actions/setup-web-app-test-env/`).
- Boots an ephemeral Postgres service container.
- Boots Oxy in the bucket's `backend_mode` and health-checks the appropriate port.
- **Restores the action cache via `actions/cache`** keyed on a hash of every flow YAML + the bespoke runtime files. On a cache hit, every step that's text-identical to a previously-recorded step replays without an LLM call. Falls back to a prefix-match restore-key on flow edits, so unchanged steps still warm-replay.
- Runs `pnpm test:agentic <flow1> <flow2> ... --no-auto-backend --no-auto-frontend --output ../agentic-results-<bucket>.json`.
- Uploads `agentic-results-<bucket>.json`, `web-app/tests/agentic/.results/`, `.traces/`, and `.logs/` as the `agentic-results-<bucket>` artifact.
- On `pull_request` events, reads `web-app/tests/agentic/.results/healing.json` and (if non-empty) posts a markdown drift-events table via `.github/scripts/agentic-healing-comment.mjs`.

The job is `continue-on-error: true` while we calibrate. Flip it to `false` once the suite is steady-state.

### What invalidates the CI cache

- Editing any `.flow.test.yml` file → exact-key miss, prefix restore from the prior cache. Unchanged step text still hits.
- Editing `runner/runtimes/bespoke.ts`, `runner/tool-registry.ts`, or `runner/action-cache.ts` → exact-key miss with prefix restore. The cache schema version (`CACHE_VERSION` in `action-cache.ts`) auto-invalidates entries with mismatched version, so a runtime change that breaks replay is already self-healing — the cache key bump just makes invalidation instant rather than per-step lazy.

If you intentionally want to nuke the cache (e.g. to remeasure cold cost), bump `CACHE_VERSION` in `runner/action-cache.ts` or just add a comment to the cache-key inputs to flip the hash.

## Troubleshooting

- **`backend did not become healthy`** — `oxy start --local --enterprise` (or `oxy start --enterprise --clean` for cloud-mode flows) failed. Tail `web-app/tests/agentic/.logs/backend.log`. Most often Docker Desktop isn't running, or the system `oxy` binary on PATH is older than the workspace build (set `$OXY_BIN` to the freshly built one).
- **`cannot run flows with mixed backend_mode`** — you loaded a glob that matched both local-mode and cloud-mode flows. Filter to one mode per invocation (e.g. `pnpm test:agentic builder-edits-app` or `pnpm test:agentic chat-ask ide-save`).
- **Stale local cache** — delete `tests/agentic/.cache/bespoke-actions.json` to force a full re-derive on the next run, or pass `cache_actions: false` in the flow's settings.
- **Snapshot too large** — the LLM can call `browser_get_page_text` as a fallback, or `browser_snapshot` with `region: "main"` to scope. If it consistently struggles, narrow the `act:` prompt or split the step.
- **Judge cost too high** — flip `expect: judge:` to `expect: assert:` where possible; judge is for soft claims only. Cheaper still: use a deterministic `selector ... has attribute ...` assert.
