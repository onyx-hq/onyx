---
name: oxy-run-and-verify
description: Use when you need to run, start, or launch the Oxy / Oxygen app locally and look at a change in the real product — verify a UI or API change in the browser, confirm a fix works end to end, seed demo data, sign in as a member / org owner / staff / app operator / partner, screenshot a feature, or reproduce a reported UI bug with the Playwright MCP. Triggers on "run the app", "start the server", "start the stack", "just up", "check it in the browser", "see it working", "screenshot the page", "reproduce this bug", "sign in as", "log in as a member", "dev-login", "seed data", "state.json", "which persona", "503 needs_recompile", "x-oxy-assume-required". This is the project skill the built-in `run` skill looks for. Not for authoring committed regression flows (agentic-browser-test) or for unit/integration tests.
---

# Run the stack, sign in as a persona, verify

The loop is always the same, and it never involves `--local`, hand-started servers, or env edits:

1. **`just up`** — builds, starts, seeds, serves. Idempotent: re-run it after any change.
2. **Read `.oxy-dev/state.json`** — URLs, pids, log paths, personas, seeded workspace ids.
3. **`browser_navigate` to `http://127.0.0.1:5173/dev-login?as=<persona>&next=<path>`** — that one navigation is the whole sign-in.
4. Look, click, screenshot. Change code. Back to 1 (or nothing, for web-app edits — Vite hot-reloads).

**Here vs `agentic-browser-test`:** this skill is for *looking at* a change once. A regression you want kept belongs in a committed flow under `web-app/tests/agentic/flows/`; invoke that skill for it.

## Running it from Claude Code

- Run `just up` / `just status` / `just down` **with the Bash sandbox disabled**: they need the Docker socket, `ps`, and to bind ports. Sandboxed, the script exits at once naming the problem.
- A **cold** `just up` can outlast the 10-minute Bash timeout (a ~7 min first build, image pulls, then the seed compiling ~10 workspaces). Run it with `run_in_background: true` and wait for the completion notification. A warm run is mostly the seed step.
- The exit code is the contract. On failure the script prints the last 40 log lines and the log path, so read those before retrying anything.

## Commands

| Command | Does |
| ------- | ---- |
| `just up` | `cargo build -p oxy-server` → `oxy start --enterprise` (Docker Postgres :15432, API :3000, no-auth internal API 127.0.0.1:3001) → `oxy seed --workspace-path ./examples --llm-keys` (compiles + promotes every seeded workspace; copies env LLM keys into their secrets) → Vite on 127.0.0.1:5173 → writes state.json and prints three ready sign-in links |
| `just up --no-build` | reuse the existing binary (`$OXY_BIN`, else `target/debug/oxy`) |
| `just up --no-seed` | skip the seed (faster when data and compiled revisions are already there) |
| `just up --no-frontend` | API only, for `curl` checks |
| `just up --restart` | restart backend + Vite even when healthy (after editing `.env`) |
| `just up --clean` | wipe the database volumes first (`oxy start --clean`) |
| `just status` | pids, `/api/ready`, Vite + its `/api` proxy, containers, current log paths. Exits 1 when not serving |
| `just down` / `just down --db` | stop backend + Vite / also `docker stop` the Postgres + ClickHouse containers |

Reuse rules: the backend is kept when it is ours, ready, and the binary is unchanged; a build that produced a new binary restarts it. Vite is kept when it is ours and answering. A port held by anything else fails the run and prints the process holding it. That is usually a server you started by hand, so stop it rather than working around it.

## `.oxy-dev/state.json`

| Key | Use |
| --- | --- |
| `frontend_url`, `backend_url`, `internal_url`, `database_url` | where things are (always `127.0.0.1` for HTTP) |
| `dev_login_template` | `http://127.0.0.1:5173/dev-login?as={persona}&next={path}` |
| `token_template` | `http://127.0.0.1:3000/api/auth/dev-login?as={persona}` — GET returns `{token}` for `curl`, sets no cookie |
| `personas` | `{name: email}`; `persona_checks` holds what the server actually answered for each |
| `workspaces[]` | `org_slug`, `workspace_name`, `workspace_id`, `compiled`, `home_path` |
| `custom_apps[]` | `org_slug`, `app_slug`, `url` |
| `logs` | the current `build` / `backend` / `seed` / `frontend` log files |

Every run writes **new** timestamped logs under `.oxy-dev/logs/`. Take the path from `logs` (or `just status`), never a guessed or globbed name.

## Personas

| Persona | Signs in as | Standing | Verify with it |
| ------- | ----------- | -------- | -------------- |
| `member` | `amara.larsson@acme.test` | plain Acme member: no partner access, no team | the customer view: Home/launcher, custom apps, chat, data apps, automations as a non-admin; "you can't do that" paths |
| `owner` | `maya.nguyen@acme.test` | Owner of Acme | org + workspace settings (Members, Teams, Crew, Locations, Positions, Billing, Databases, Secrets, Apps), invitations |
| `staff` | first `OXY_GLOBAL_ADMINS` email in `.env` (debug builds only) | Global Admin + Owner of `local` | the admin console, and the IDE / chat / semantic model on the **Demo** workspace, which is the one with real seeded content |
| `operator` | `app-operator@oxy.local` | platform `app_operator` grant, **no** org membership | custom-apps admin (`/admin/apps`); out-of-scope admin pages answer *not found* by design |
| `partner` | `oliver.okafor@acme.test` | Acme member with partner-console access | `/partners` — Acme's clients (northwind, globex), apps, team, activity |

Pick the **least-privileged persona that should see the feature**, and use a second one to prove who shouldn't. Staff entering any org other than `local` needs an assume-role session, and that is a policy boundary, not a bug.

Seeded orgs: `local` (Demo `70787bb2-e11b-5488-b2c3-02e60d5fc7d3` from `./examples`, custom app `oxy-starter`), `acme` (partner; manages `northwind`, `globex`), `initech` (partner; manages `umbrella`), `vandelay` (unmanaged). Take workspace ids from `state.json`.

## Route map

`W` = `/<org_slug>/workspaces/<workspace_id>` (see `workspaces[].home_path`). All paths are on `http://127.0.0.1:5173`.

| Surface | Path |
| ------- | ---- |
| Post-login dispatcher / org root | `/` · `/<org>` (each picks a workspace and redirects) |
| Home (launcher) | `W` or `W/home` |
| Chat | `W/threads` · `W/threads/<id>` |
| Automations | `W/automations` · `W/automations/<pathb64>` (`/workflows` is an alias) |
| Airway pipeline | `W/pipelines/<pathb64>` |
| Data apps (`.app.yml`) | `W/apps` · `W/apps/<pathb64>` |
| Oxygen Factory (IDE) | `W/ide` → `W/ide/world-model` |
| IDE: files · SQL · semantic model · modeling · tests | `W/ide/files/<pathb64>` · `W/ide/database` · `W/ide/semantic` · `W/ide/modeling` · `W/ide/tests` |
| Orchestrator · observability | `W/ide/coordinator/overview` · `W/ide/observability/traces` |
| Context graph | `W/context-graph` |
| Settings dialog | any `W` URL + `?settings=<section>`: `organization.{general,members,teams,crew,locations,positions,billing,integration}`, `workspace.{members,databases,airhouse,oltp,repositories,secrets,connections,apps}`, `preferences.appearance` |
| No org yet (pending invites, join by link) | `/onboarding` — orgs are created only by staff (`/admin/tenants?type=orgs`) or partners, each with a Ready `Default` workspace. An org with no ready workspace shows a "being set up" card at `/<org>`; `/<org>/onboarding` just redirects there |
| Admin console | `/admin` → `/admin/apps`; `/admin/{orgs,users,workspaces,workspace-health,tenants,compiles,internal-jobs,feature-flags,audit,app-admins,publish-tokens,airhouse,oltp}`; Global-Owner only: `/admin/billing/queue`, `/admin/airway` |
| Partner console | `/partners` · `/partners/{apps,team,activity}` |
| Custom app | `/customer-apps/local/oxy-starter/` (served by the backend through Vite's proxy) |

`<pathb64>` is standard base64 of the workspace-relative file path (`printf %s business_metrics_dashboard.app.yml | base64`). URL-encode `?` and `&` inside `next`: `next=/local/workspaces/<id>/home%3Fsettings%3Dorganization.members`.

## Reading what you see

| You see | It means | Do |
| ------- | -------- | -- |
| 503 with `"needs_recompile": true` / "This workspace isn't ready yet" | no promoted compiled revision; a compile was just enqueued | wait ~2 min, or `just up` (the seed compiles + promotes); a compile error is in the seed log |
| 403 with `x-oxy-assume-required`, or the assume-role dialog | a staff/operator identity inside a tenant workspace without an assume session | use the tenant's own persona (`owner` / `member`) |
| plain 403 | this persona lacks the role | the persona table, not the code, is usually wrong |
| dev-login **409** | persona not seeded | `just up` without `--no-seed` |
| dev-login **400** | unknown persona, or both `email` and `as` given, or a binary older than personas | fix the URL; `just up` rebuilds |
| dev-login **404** | dev-login disabled: no `OXY_GLOBAL_ADMINS` in `.env`, or a release binary | add it to `.env`, `just up --restart` |
| bounced to `/login` | no session / expired | navigate to the dev-login URL again |
| staff lands on `/admin/billing/queue` | that email is also in `OXY_OWNER`; a Global Owner is sent to admin from user-facing routes | use another persona for tenant pages |
| chat run errors on the provider / API key | no LLM key in the env when the seed ran (`just up` seeds with `--llm-keys`, storing keys as workspace secrets; a bare `oxy seed` does not) | put `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` in `.env`, `just up` |
| custom-app data 403 `origin not allowed` | a non-loopback origin | browse `127.0.0.1:5173` |

## After a change

| Changed | Do |
| ------- | -- |
| `crates/**` | `just up` (rebuilds; restarts the backend only if the binary changed; re-seeds) |
| `web-app/**` | nothing — Vite hot-reloads; reload the page if state is stale |
| `examples/**` YAML | `just up` (the seed recompiles + promotes Demo) |
| a migration | `just up` (the restart runs it) |
| `.env` | `just up --restart` |
| `pnpm-lock.yaml` | `just up` (frozen `pnpm install`, then restarts Vite) |
| data got weird | `just up --clean` |

## Playwright MCP

- **Sign-in** is one `browser_navigate` to the dev-login URL; the page posts, sets the session, and lands on `next`. To switch persona, navigate to `/dev-login?as=<other>` again. If the old identity lingers, `browser_close` and start over.
- Find elements with `browser_snapshot` (accessibility tree + refs) and prefer `data-testid`s. Use `browser_take_screenshot` for evidence; files land in `.playwright-mcp/` (gitignored).
- Diagnose with `browser_network_requests` (look for the 403/409/503 shapes above) and `browser_console_messages`.
- Missing browser: `pnpm --dir web-app/node_modules/@playwright/mcp exec playwright install chromium`.
- API-only check: `TOKEN=$(curl -s --noproxy '*' 'http://127.0.0.1:3000/api/auth/dev-login?as=member' | jq -r .token)`, then `curl -s --noproxy '*' -H "Authorization: Bearer $TOKEN" http://127.0.0.1:3000/api/orgs`.

## Gotchas

- **`127.0.0.1`, not `localhost`.** Vite binds IPv4 loopback only, and macOS resolves `localhost` to `::1` first.
- **Never `--local`**: it's the unmaintained no-auth single-workspace mode, and nothing you see there says anything about the product.
- **`RUSTC_WRAPPER=sccache` in your shell breaks the build** ("could not execute process `sccache`"; `.cargo/config.toml` forbids it). `just up` drops it for its own build; a bare `cargo build` does not.
- **`invalid archive member … librusty_v8.a` / `could not find native static library rusty_v8`**: the V8 prebuilt (~39 MB `.gz`) download was cut short. The sandbox proxy does this, and so do two cargo processes racing on `target/debug/gn_out`. Delete `target/debug/gn_out/obj/librusty_v8.*`, run `cargo clean -p v8`, and rebuild once, unsandboxed and alone.
- **`pnpm exec` can run an implicit `pnpm install`** when `node_modules` is behind the lockfile. For one-off tools call the binary directly: `web-app/node_modules/.bin/{tsc,vitest}`, and `node_modules/.bin/biome` from the repo root.
- **Don't set `OXY_DEV_LOGIN_EMAILS`**: it replaces the persona roster. **Don't export `OXY_ROLE`**: `oxy start` is one all-roles process, and `just up` strips it.
- `oxy start` recreates the `oxy-postgres` container on every backend restart (the volume survives), and sets `OXY_DATABASE_URL` only for itself. A separate `oxy seed` / `oxy compile` needs `OXY_DATABASE_URL=postgresql://postgres:postgres@localhost:15432/oxy`.
- One stack per machine: the ports are fixed. A second checkout needs its own ports, as described in DEVELOPMENT.md "Running multiple instances side by side".
