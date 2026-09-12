# Claude Code Assistant Guidelines

> **Injected into every API call**, together with `product-context.md` and
> `internal-docs/backend-architecture.md` (both `@`-imported below). State each rule
> **once**, in the section that owns it. A recap section is how three copies of the
> `--local` rule accumulated here; if a rule feels worth repeating, the first
> statement is the one to sharpen. Anything retrievable by reading the code in a
> minute belongs in the code or a crate-local `CLAUDE.md`, not here.

Oxy (brand: **Oxygen**) is a Rust workspace + web frontend. The `oxy` CLI/server
binary lives in the `server` crate (`oxy-server`, the default workspace member) — a
thin composition root that mounts API surfaces over the `app` crate (`oxy-app`), the
CLI + HTTP-server library where almost all the code lives.

- **Rust** · **async** Tokio · **ORM** Sea-ORM (PostgreSQL) · **HTTP** Axum
- **Frontend** Vite + React + TypeScript, **pnpm** (never npm/yarn)

## Workspace Layout

```
crates/
  server/                   # (oxy-server) the `oxy` BINARY — composition root (main.rs + router
                            #   assembly) that mounts the API surfaces. Default member.
  app/                      # (oxy-app) CLI + HTTP server LIBRARY (the bulk of the code; lib-only)
  api-github/               # (oxy-api-github) GitHub OAuth + git-namespace HTTP surface — a sibling
                            #   crate oxy-server mounts; oxy-app does NOT depend on it
  app-dylib/                # (oxy-app-dylib) dev-only dynamic-linking shim, EXCLUDED from the
                            #   workspace; built only by `--features dev-dynamic` (just dev-backend-dyn)
  app-core/                 # (oxy-app-core) Shared app-layer seam: audit, serve_mode,
                            #   org/custom-app subdomain dispatch, member authz
  core/                     # (oxy) Core platform library, published as "oxy"
  auth/                     # (oxy-auth) Authentication — who you are
  authz/                    # (oxy-authz) Authorization — what you may do. THE authority model
  entity/  migration/       # Sea-ORM entities / migrations
  semantic/                 # (oxy-semantic) Semantic query layer (airlayer)
  shared/                   # (oxy-shared) Shared types, errors, infra
  project/                  # (oxy-project) Project/model config domain
  thread/                   # (oxy-thread) Thread/conversation domain (thin)
  oxy-compile/              # (oxy-compile) Compile boundary: workspace FS → Postgres rows
  workspace-fs/             # (oxy-workspace-fs) Workspace filesystem helpers (thin)
  git/                      # (oxy-git) Git client / worktree ops
  platform/                 # (oxy-platform) Platform services
  billing/                  # (oxy-billing) Stripe billing
  metric-monitoring/        # (oxy-metric-monitoring) Anomaly monitors / metric tree
  observability/            # (oxy-observability) Customer-facing observability backend
  telemetry/                # (oxy-telemetry) Platform telemetry: OTLP traces+logs → cluster OTel
                            #   collector (ClickStack/HyperDX), stderr JSON format, HTTP request spans
  airform/                  # (oxy-airform) dbt-style modeling
  airhouse/                 # (airhouse) Warehouse + connector
  cameras/                  # (oxy-cameras) Camera fleet domain
  test-utils/               # (oxy-test-utils) Fixtures & mocks
  agentic/
    core/ runtime/ pipeline/ analytics/ builder/ automation/ airway/
    connector/ http/ llm/ semantic/    # see crates/agentic/CLAUDE.md for layering
  infrastructure/llm/{anthropic,gemini,ollama,openai,oxy-llm}
  infrastructure/semantic/  # (oxy-airlayer-compat) airlayer compatibility shim
  integration/{looker,unifi,omni}
web-app/                    # Frontend (see web-app/CLAUDE.md)
sdk/cli/                    # (@oxy-hq/cli) the `oxyc` CLI — the gh-api-style client for
                            #   the HTTP API, plus the customer-account tooling. TypeScript,
                            #   NOT Rust: it replaced `oxy api`, which was deleted from
                            #   crates/app. See internal-docs/oxy-api-cli.md
```

Many crates carry their own `CLAUDE.md` — all `agentic/*`, `authz`, `cameras`,
`integration/unifi`, plus the two largest (`core`, `app`). Read the local one first.

## Build & Test

- **Never `--release`** locally or in CI checks — debug only.
- **`cargo nextest run`**, never `cargo test`.
- **Check every affected package.** A change usually lands in two: `cargo check -p oxy`
  *and* `-p oxy-app`, or `--workspace`.
- After CLI changes, exercise the binary: `cargo build && ./target/debug/oxy <command>`.

**Never verify with a bare `cargo nextest run`.** That builds and links every test
target in 84 crates. The cost of a test run here is *linking*, not asserting: each
integration-test target is a separate binary statically linking DuckDB + DataFusion +
Arrow + AWS SDK, so it costs a multi-hundred-MB link before the first assertion. Unit
tests share one binary per crate. So scope down, always:

| Verifying | Run |
| --------- | --- |
| Logic in `src/**` (the common case) | `just unit oxy-app` → `cargo nextest run -p oxy-app --lib` |
| One crate end to end | `just test-crate oxy-app` |
| A named area | `just test-filter 'test(authz)'` |
| It still *compiles* | `cargo check --tests -p oxy-app`, or `just test-build` |
| Everything | `just test` — CI's job, rarely yours |

Integration tests live in **grouped** binaries, and this is workspace-wide, not an
`oxy-app` quirk: `oxy-app` has six by domain (`tests/authz/`, `tests/slack/`,
`tests/custom_apps/`, `tests/airhouse/`, `tests/platform/`, `tests/routing/`), and **every other crate with
integration tests runs a single `tests/integration/` group**. Add a new case as a `mod`
inside the matching group — **a new top-level `tests/*.rs` adds a whole new link to every
full run.** A module is not a target, so run one with
`--test <group> -E 'test(<module>)'`.

### Browser tests (UI features)

UI changes under `web-app/` default to a regression flow in `web-app/tests/agentic/flows/`
(Playwright + LLM action selection, ~$0.002/run after first record). The
[`agentic-browser-test`](.claude/skills/agentic-browser-test/SKILL.md) skill owns
authoring/maintenance; drive it via slash commands (`/test-feature`,
`/agentic-test-add-case`, `/run-agentic-tests`, `/fix-agentic-test`,
`/accept-agentic-healing`). Mechanics: `web-app/tests/agentic/README.md`.

### Driving the running app (Playwright MCP)

The `playwright` MCP server (declared in `.mcp.json`) is for **looking at** a change —
clicking through it, screenshotting it, reproducing a reported bug. Different job from
the flows above, which are the committed regression suite: don't hand-drive a flow you
should be writing, don't write a flow to see a page once.

The loop is `just up` (build, start, seed, Vite — idempotent), then read `.oxy-dev/state.json`,
then one `browser_navigate("http://127.0.0.1:5173/dev-login?as=<persona>&next=<path>")` — that
navigation is the whole sign-in, no OAuth. Personas, routes, and what a 403/409/503 means:
the `oxy-run-and-verify` skill.

## `oxyc` — talking to a running deployment

`npm i -g @oxy-hq/cli` (published; `npx @oxy-hq/cli` also works). It is the
`gh api`-shaped client for the Oxy HTTP API — **use it instead of hand-rolling
curl** when you need real data out of dev/staging/prod, and instead of guessing
a route.

```bash
oxyc login --env dev              # browser; same credentials file the old `oxy login` wrote
oxyc routes threads --env dev     # discovery, SERVED by GET /api/_catalog — not a baked table
oxyc schema /api/orgs --env dev   # request/response shape for one endpoint
oxyc api /api/orgs --env dev -q '.[].name'   # -f/-F/--input/-q/--paginate, like gh api
```

Two things that mislead if you don't know them: an expired token makes
`GET /api/user` answer **200 with a null body**, which `oxyc whoami` reports as
"no longer resolves to a user" — not a 401; and exit codes are a contract
(`oxyc exit-codes`), so an agent branches on 4=auth / 5=not-found / 6=request
rather than parsing stderr. Full reference: `sdk/cli/README.md` (maintainer's
notes: `internal-docs/oxy-api-cli.md`).

## Committing

Conventional Commits, types `feat|fix|refactor|docs|test|build|chore|perf|style|ci`.
Name the **area** after the colon (`fix: web-app chart rendering bug`) — this repo uses
free-text areas, not scoped parens.

## Code Style

**Rust** — errors via `thiserror`/`OxyError` (`oxy_shared::errors`); logging via `tracing`,
**never `println!` in library crates**; CLI output via the `StyledText` trait from
`oxy::theme`.

**Frontend** — **pnpm only**, and `pnpm exec <tool>` never `npx`; format with Biome
(`pnpm exec biome check --write <file>`), not Prettier. Full conventions in
`web-app/CLAUDE.md`.

## Database & Runtime

- **DB**: there is no embedded Postgres — `oxy serve` refuses to boot without `OXY_DATABASE_URL`.
  `oxy start` runs one in Docker (via `bollard`, not docker-compose): container `oxy-postgres` on
  `localhost:15432` (postgres/postgres, db `oxy`), volume `oxy-postgres-data`, and sets the URL
  for its own process only; `--clean` for a fresh slate. Migrations run on startup
  (`cargo run --bin migration` to force). Entities in `entity`, migrations in `migration`.
- **Local development runs the production path** — `oxy serve --enterprise`, or `oxy start`
  for a Docker-Postgres dev box. Use it for tests, demos, role-split, S3/worker-fleet, and
  anything production-shaped. (`--local` is the legacy no-auth mode; see product-context.md.)
- **A dev box is cloud mode with non-prod secrets**, so it is indistinguishable from prod by
  `ServeMode` — never gate dev behavior on `ServeMode::Local` or on error heuristics.
  Concretely: `ctx.email.send` hits real SES, so set `OXY_APP_EMAIL_LOCAL_TEST=1` (and
  `MAGIC_LINK_LOCAL_TEST=true`) to preview the rendered email in the browser instead of
  sending. Both are in `.env.example` — which is the only `.env*` file that may be committed.

## Authorization

Every authorization decision goes through **`oxy-authz`** (`crates/authz`) — one exhaustive
`match` in `allows()` that states every authority ring. Note the neighbouring crate
`oxy-auth` is *authentication*; they are not interchangeable. Full guide:
`crates/authz/CLAUDE.md`.

- **Never decide access by hand.** No `matches!(role, Owner | Admin)` in a handler, no
  reading `OXY_OWNER` / the `app_admins` table. Take a `role_guards::*` extractor (there
  are 6), or call `server::authz::enforce_guard(..)`. `crates/app/tests/authz/authz_boundaries.rs`
  **fails the build** otherwise — its allowlist is a backlog, not an exemption list.
- **Platform standing** (`is_owner` / `is_app_admin`) is read **only** by
  `server::authz::globals`: one door for a decision, one for a flag to display.
- **The crate owns the model; the app owns fact-loading** (`server::authz::loader`) —
  loading needs DB primitives, so it can't live in the crate. Keep `oxy-authz` on
  `uuid` + `tracing`: that's what lets the model be tested without a database.
- **Decisions are `existing_allow && allows(..)`**, so the model can only ever *subtract*
  and a mis-modeled ring cannot open a hole. Pass the **real** shipped check as
  `existing_allow` — it's the oracle the differential tests use, not ceremony.
- A new `Action` needs a `Ring` (the compiler enforces it) **and** a differential case.
  Reuse a ring rather than inventing a synonym.

Two engines were evaluated and rejected (Cedar — adopted then removed; Casbin). The
reasoning is in `crates/authz/src/lib.rs` and the design doc — read it before proposing a
third. Policy-as-data is an explicit non-goal; that's the requirement that pays for an engine.

## Product Context (Web UI)

@product-context.md

## Backend Architecture

@internal-docs/backend-architecture.md

## Project Skills (invoke, don't rederive)

These encode decisions already made and argued. When the work matches, **invoke the
skill** rather than reasoning it out again — its `SKILL.md` carries the full contract.

| Skill | Governs — invoke when… | Non-negotiable |
| ----- | ---------------------- | -------------- |
| `oxy-scaling-design` | multi-instance / worker fleet / lease table / `OXY_ROLE` / stateless serve | code-first is sacred; git is source of truth; task-claim ≠ workspace-ownership (two leases) |
| `oxy-task-spec-default` | background work in `crates/app/src/server/` (`tokio::spawn`, periodic loops, LLM calls, clones >5s) | new long-running work is a `TaskSpec` in `agentic_task_queue`, **not** a spawn in an HTTP handler |
| `oxy-compile-boundary` | new `.foo.yml` file type, or any per-request read that walks the workspace FS | every workspace artifact is a `*_definitions` Postgres row keyed by `revision_id`, not an FS read |
| `oxy-route-classification` | add/move a route under `server/router/`, or a handler touching disk/`.git`/state dir | FS-touching routes MUST be `IdeOnly` in `role_manifest.rs`; persisted-data reads MUST stay `FleetOk` |
| `oxy-customer-apps-perf` | add/move a `/customer-apps/**` route or custom-app data endpoint; any per-request read on that hot path | serving routes need Cache-Control + SSE-safe compression; result caches keyed `project_id`-first, read after auth gates, honor `?refresh` |
| `oxy-run-and-verify` | verify a change in the running app, seed data, sign in as a persona, screenshot a feature or reproduce a UI bug | never `--local`; `just up`, not hand-started servers; sign in by persona (`/dev-login?as=`), never by editing env |

PRs that violate the right-hand column should be challenged through the matching skill.

## Docs

**`internal-docs/`** holds the living implementation references and operator runbooks —
the "how a subsystem works and why" you can't grep from code. Consult the matching doc
before working on a subsystem, and fold non-obvious durable facts back in when you ship.
[`internal-docs/README.md`](internal-docs/README.md) is the index; its **Maintenance
policy** is the full contract.

- **Design docs and specs go here**, never `docs/superpowers/specs/`. Prefer an existing
  living doc to a new file.
- **Undated filenames are durable; dated `YYYY-MM-DD-*.md` are ephemeral** snapshots that a
  biweekly workflow distills and prunes — never the lasting home for a fact.

**Brand copy:** never regenerate homepage/positioning/tagline copy from scratch — port it
verbatim or flag marketing. Canonical source is `docs/snippets/positioning.mdx` (mirrored
by `README.md`), gated by `.github/CODEOWNERS`. A docs PR that deletes a landing page or
touches >~50 files must confirm "positioning carried over verbatim".

## Search: four layers, one question each

Four search systems are installed and they overlap. Route by **what kind of question
you're asking**, not by which tool you remember:

| The question | Use | Why not the others |
| ------------ | --- | ------------------ |
| **How does this code relate?** — callers, callees, imports, tests-for, blast radius, execution flows, module structure | **code-review-graph** MCP (`query_graph_tool`, `get_impact_radius_tool`, `get_architecture_overview_tool`) | Precomputed edges. Nothing else here knows "which flows break if I change this". |
| **Review this diff** | **code-review-graph** `detect_changes_tool` + `get_review_context_tool` | Risk-scored and token-bounded; reading the files is the expensive way to the same answer. |
| **What does this code look like?** — call shapes, JSX props, `impl` blocks, "every handler that skips X" | **`ast-grep`** (see below) | The graph indexes symbols and edges, not syntax. A shape query is exactly its blind spot. |
| **Map a big file before reading it** | **`ast-grep outline <f> --view signatures`** | Zero setup, and works on files the graph hasn't indexed yet — a brand-new file, a worktree, `node_modules`. |
| **Where does this exact symbol resolve?** — one symbol, ground truth, generics and traits included | **serena** `find_referencing_symbols` / `find_symbol` | LSP, not an index. Use when the graph's answer looks incomplete or the code is uncommitted. |
| **Where does this string appear?** — literals, config keys, log lines, comments, TODOs | **Grep / Glob** | Text is text. The graph has no node for a string in a YAML file. |

**Grep IS ripgrep and Glob IS fd** — use the tools, not Bash, so results stay capped
and structured (`output_mode`, `head_limit`, `-A/-B/-C`). Shelling out to `rg`/`fd`
buys nothing and loses those. `fzf` is a TTY selector and is useless here.

**Never run `ast-grep` without `--json=compact | jq`:**

```bash
ast-grep -p '<pat>' -l <lang> --json=compact | jq -r '.[] | "\(.file):\(.range.start.line)"'
```

A match is a whole AST node, so `tokio::spawn($$$)` returns every spawned block in
full: 36 call sites in `crates/app/src/server/` = **258 KB** raw, vs 4.5 KB for
`rg -n` and 2.5 KB filtered. Filtered it's the cheapest of the three; raw it's a
context bomb. Same shape for `outline`: `world_model_graph/handlers.rs` is 117 KB,
its outline 624 bytes.

**Graph freshness.** `.code-review-graph/` (gitignored, ~370 MB) is built at a
commit; a `PostToolUse` hook runs `code-review-graph update --skip-flows` after
edits, so symbols stay current but **flow and community data lag until a full
`update`**. If a graph answer contradicts the code in front of you, the code wins —
re-check with serena or ast-grep and run `code-review-graph update`.

Skills: `explore-codebase`, `review-changes`, `debug-issue`, `refactor-safely` drive
the graph; `ast-grep` and `ast-grep-outline` carry pattern syntax. Invoke them rather
than guessing tool names or metavariable syntax.

> A generated block below may say "**ALWAYS** use the graph BEFORE Grep/Glob/Read".
> It is written by `code-review-graph install` and doesn't know the other three tools
> exist. **The table above supersedes it** — the graph is first for *relationship*
> questions, not for all of them.


<!-- code-review-graph MCP tools -->
## MCP Tools: code-review-graph

Routing for these tools lives in **"Search: four layers, one question each"** above,
alongside the three other search systems this repo has — read that, not this stub.

Everything below the marker comment is regenerated by `code-review-graph install`.
If a rerun replaces this stub with the stock "ALWAYS use the graph first" text,
that text is wrong here (it can't know serena and ast-grep are installed); restore
this pointer. The tool table it ships is accurate and worth keeping — it's the
*ordering* claim that conflicts.

| Tool | Use when |
| ------ | ---------- |
| `detect_changes_tool` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context_tool` | Need source snippets for review — token-efficient |
| `get_impact_radius_tool` | Understanding blast radius of a change |
| `get_affected_flows_tool` | Finding which execution paths are impacted |
| `query_graph_tool` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes_tool` | Finding functions/classes by name or keyword |
| `get_architecture_overview_tool` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

Start with `get_minimal_context(task=...)` and `detail_level="minimal"` — the
generated skills set a budget of ≤5 calls and ≤800 output tokens per task.
