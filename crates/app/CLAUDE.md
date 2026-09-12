# `oxy-app` — CLI + HTTP Server (`crates/app`)

The CLI + server **library** lives here (crate name **`oxy-app`**, path `crates/app`, ~111k
LOC — the largest crate; lib-only). The `oxy` **binary** is the thin `oxy-server` crate
(`crates/server`, the workspace default member), which mounts the API surfaces and calls into
this library. It wires everything together: the CLI
command surface, the Axum HTTP server (`server/`, ~80k LOC), the fleet/role machinery, the
worker runtime, and integration glue (Slack, emails). Business logic should live in `oxy` and
the domain crates — this crate is composition + transport.

## Layering & where logic belongs

`oxy-app` sits at the top of the platform stack and is the only crate allowed to depend on
nearly everything (`oxy`, agentic-http/pipeline, integrations). Keep it thin:

- **HTTP handlers are transport** — parse/validate → call `oxy` or `agentic-pipeline` →
  serialize. A handler past ~30 lines is leaking domain logic (see `backend-architecture.md`).
- **Enter the agentic subsystem only through `agentic-http`/`agentic-pipeline`**, never by
  importing `agentic-analytics`/`agentic-builder`/`agentic-connector`/`agentic-llm` directly.
- Long-running work (spawns, periodic loops, LLM calls, >5s clones) belongs in a `TaskSpec`
  on the durable queue, **not** a `tokio::spawn` in a handler — invoke the
  `oxy-task-spec-default` skill.

## Module map (`src/`, declared in `lib.rs`)

| Module | Owns |
| ------ | ---- |
| `cli/` | Command surface. `cli/commands/` has one file per command: `serve.rs`, `start.rs`, `run.rs`, `worker.rs`, `compile.rs`, `migrate*.rs`, `admin.rs`, `clean.rs`, … `mod.rs` builds the clap tree. HTTP-client commands (`publish`, `login`, `proxy`, `assume`, `api`) are not here — they are `oxyc` (`sdk/cli`). |
| `server/` | The Axum server (~80k LOC) — see breakdown below. |
| `agentic_wiring/` | Adapts `oxy` state into the shape `agentic-pipeline` needs (`project_ctx.rs` is a 1.8k-line god file — a decomposition target). Has its own `CLAUDE.md`. |
| `integrations/` | External integrations, notably the Slack bot (`integrations/slack/`). |
| `emails/`, `custom_app_template/` | Transactional email (SES) + custom-app scaffold templates. |
| `observability_boot.rs` / `observability_setup.rs` | Tracing/observability bootstrap for the binary. |

### `server/` internals

| Path | Owns |
| ---- | ---- |
| `server/api/**` | One file (or dir) per resource: `threads`, `apps`, `automation`, `projects/`, `admin/`, `billing/`, `custom_apps_*` (the serve + data hot path), `world_model_graph.rs` (4.8k — the single biggest file, a decomposition target), `workspaces.rs`, `onboarding.rs`, `auth.rs`. |
| `server/router/` | Route mounting: `public.rs`, `protected.rs`, `workspace.rs`, `global.rs`, `entry.rs`, `openapi.rs`. `ROUTES.md` documents the surface. |
| `server/role_manifest.rs` (~2k) | **Routing authority** for the split fleet — classifies every route `IdeOnly` vs `FleetOk`. FS/`.git`/state-dir routes MUST be `IdeOnly`; persisted-data reads MUST stay `FleetOk`. Invoke `oxy-route-classification` before touching routes. |
| `server/{worker_runtime,worker_health,worker_metrics}.rs`, `preagg_*`, `compile_*` | Durable-task worker fleet + pre-aggregation + compile-boundary workers. |
| `server/{serve_safety,admission,ide_proxy,role_middleware}.rs` | Serve-mode gating, self-routing reverse proxy (`IdeOnly` → ide upstream), admission control. `ServeMode` itself now lives in `oxy-app-core` — see below. |
| `server/service/` | Server-side services (api keys, app, eval, project, formatters). |

**Five modules now live in `oxy-app-core` (`crates/app-core`), not here** — `audit`,
`serve_mode`, `org_host_dispatch`, `custom_apps_host_dispatch`, `member_authz`. They moved
so the per-surface crates can share them without depending on each other. Import them as
`oxy_app_core::<module>`; no re-export was left behind, so a stale `crate::server::…` path
is a compile error, and an in-flight branch using one will merge clean and then fail to build.

## Key entry points

- `cli::commands::cli()` — clap dispatch (called from the binary's `crates/server/src/main.rs`); log format auto-detects local vs cloud (JSON).
- `oxy serve` (`cli/commands/serve.rs`) — production HTTP path; `--enterprise` is the default
  shape for tests/demos, `--local` is the single-user/no-auth laptop mode only.
- `server/router/entry.rs` — top-level router assembly.
- `server/role_manifest.rs` — the map every new route must be classified in.

## Conventions & pitfalls

- Deployment mode drives almost every behavior: **Local** (`oxy start`), **Remote**
  (`oxy serve --local`), **Cloud** (`oxy serve`). Bug triage starts with "which mode?"
- SSE streams (Builder/analytics/workflow) MUST emit a terminal event (`done`/`error`/
  `cancelled`) even on early failure, or the frontend hangs forever.
- Multi-tenant scoping is a correctness invariant — orchestrator/pre-agg endpoints filter by
  `workspace_id`; secret lookups filter by project.
- After CLI changes: `cargo build` then exercise `./target/debug/oxy <command>`.
- Check both packages after edits: `cargo check -p oxy-app` **and** `cargo check -p oxy`.
- The file/function size limits bite hardest here — this crate holds the workspace's worst
  god files (`world_model_graph.rs`, `role_manifest.rs`, `admin/apps/handlers.rs`,
  `project_ctx.rs`, `workspaces.rs`). Prefer a new module over growing one; see
  `internal-docs/domain-boundaries.md`.
