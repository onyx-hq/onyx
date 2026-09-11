# Development Guide

This guide will help you set up your development environment for contributing to Oxy.

## Prerequisites

- Rust **1.92.0 or newer** (the workspace MSRV; edition 2024)
- Node.js and pnpm (pnpm only — never npm or yarn)
- Git
- [`just`](https://github.com/casey/just) — the command runner the whole dev
  workflow is built around (`cargo install just`, or `brew install just` on macOS)

## Clone the repository

```bash
git clone https://github.com/oxy-hq/oxygen.git
cd oxy
```

## Setup

The paved path is `just`, which bootstraps the dev toolchain (including
[`cargo-nextest`](https://nexte.st/), the test runner used everywhere) and
installs frontend dependencies in one step:

```bash
just install   # installs nextest + other dev tools and runs `pnpm install`
just           # list every available recipe
```

If you'd rather install the pieces by hand:

```bash
cargo build      # build the Rust workspace (debug — never use --release locally)
pnpm install     # install frontend dependencies, and point git at .githooks
```

Either route installs the git hooks: `pnpm install` runs a `prepare` script that
sets `core.hooksPath` to the tracked `.githooks` dir, which forwards commit-msg /
pre-commit / pre-push to `.husky/` and seeds a new checkout's `target/`.
`just install-hooks` does the same thing explicitly.

**That first `cargo build` is ~7 minutes from cold, and you can skip nearly all
of it** if you already have another checkout of this repo built:
`just seed-target` hardlinks its artifacts across in seconds. See
[internal-docs/rust-build-performance.md](internal-docs/rust-build-performance.md).

## Environment Variables

You typically do not need to manually set environment variables for local development. Oxy manages environment configuration inside the app by default.

Set environment variables only when integrating external services or overriding defaults (for example, custom database URLs or provider credentials). Use `.env.example` as the reference template.

## Running Tests

Tests run under [`cargo-nextest`](https://nexte.st/), **not** `cargo test`. The
simplest entry point is:

```bash
just test
```

To run nextest directly (e.g. to scope to a crate or a single test):

```bash
cargo nextest run                 # whole workspace
cargo nextest run -p oxy-app      # a single crate
cargo nextest run <test_name>     # a single test by name
```

nextest shows failing-test output by default; add `--no-capture` to stream
stdout from passing tests too.

## Running the app locally

Local development runs the production path — cloud/enterprise mode with real auth. Never
`--local`: that is the unmaintained single-workspace, no-auth mode.

### One command: `just up`

```bash
just up        # build, start, seed, Vite — detached and idempotent; re-run after any change
just status    # what is running and whether it answers (exit 1 when it is not serving)
just down      # stop the backend + Vite; `just down --db` also stops the Postgres container
```

`just up` (`scripts/dev-up.sh`) does, in order:

1. `cargo build -p oxy-server` — an incremental no-op when nothing changed.
2. `oxy start --enterprise` — Docker Postgres on `localhost:15432`, the API on `:3000`, and
   the no-auth internal API on `127.0.0.1:3001`. Reused while it is healthy and the binary is
   unchanged; restarted when the build produced a new binary.
3. `oxy seed --workspace-path ./examples` — the `local` org with the Demo workspace and the
   `oxy-starter` custom app, plus the tenant orgs (`acme`, `northwind`, `globex`, …), every
   workspace compiled and promoted. Idempotent.
4. Vite on `http://127.0.0.1:5173`, proxying `/api` and `/customer-apps` to `:3000`. Web-app
   edits hot-reload; nothing restarts it unless you pass `--restart`.

| Flag | Effect |
| ---- | ------ |
| `--no-build` | use the existing binary (`$OXY_BIN`, else `target/debug/oxy`) |
| `--no-seed` | skip `oxy seed` |
| `--no-frontend` | API only |
| `--restart` | restart the backend and Vite even when healthy (e.g. after editing `.env`) |
| `--clean` | `oxy start --clean`: wipe the database volumes first |

It needs a running Docker daemon and uses fixed ports, so it is one stack per machine (for
side-by-side checkouts, see below). It refuses to start when a port it needs is held by a
process it did not start, and prints that process. Every run writes fresh log files under
`.oxy-dev/logs/`, and `.oxy-dev/state.json` records the URLs, pids, seeded workspaces and
sign-in personas. To land signed in, open
`http://127.0.0.1:5173/dev-login?as=member` (or `owner`, `staff`, `operator`, `partner`) —
see **Dev sign-in** below. Coding agents: the `oxy-run-and-verify` skill
(`.claude/skills/oxy-run-and-verify/SKILL.md`) is the full guide.

### By hand

There is no embedded database: `oxy serve` refuses to boot without `OXY_DATABASE_URL`.
`oxy start` brings up Docker Postgres and then serves:

```bash
cargo run -p oxy-server -- start --enterprise     # Docker Postgres + API on :3000
pnpm --dir web-app dev                            # Vite on http://127.0.0.1:5173

# seed that database (idempotent)
OXY_DATABASE_URL=postgresql://postgres:postgres@localhost:15432/oxy \
  ./target/debug/oxy seed --workspace-path ./examples
```

Against a Postgres you run yourself, set `OXY_DATABASE_URL` and use
`cargo run -p oxy-server -- serve --enterprise` instead.

## Running multiple instances side by side

You can run several local Oxy instances at once — typically one per git
checkout or [worktree](https://git-scm.com/docs/git-worktree) — to test
multi-instance behavior or work on two branches without stopping each other.

All ports are configurable from the repo-root `.env`, which **both** the backend
(`cargo run serve`, via `dotenv`) and the Vite dev server (`pnpm run dev`, via
`loadEnv`) load. The flags still win when passed explicitly; otherwise the env
var is used, then the default.

| Variable                 | Used by  | Default                 | Purpose                                             |
| ------------------------ | -------- | ----------------------- | --------------------------------------------------- |
| `OXY_HTTP_PORT`          | backend  | `3000`                  | API server port (same as `serve --port`)            |
| `OXY_HTTP_INTERNAL_PORT` | backend  | `3001`                  | internal port (same as `serve --internal-port`)     |
| `OXY_DEV_PORT`           | frontend | `5173`                  | Vite dev server port                                |
| `OXY_DEV_PROXY_TARGET`   | frontend | `http://localhost:3000` | backend the Vite dev server proxies API requests to |

> ### `OXY_DEV_PROXY_TARGET` must stay on this machine
>
> Point it at a **remote** backend and custom-app data calls answer 403
> `origin not allowed`. The origin gate
> (`is_local_dev_pair`, `crates/app/src/server/router/mod.rs`) allows a
> loopback origin only when the request also *arrived* at a loopback host, so a
> dev browser talking to `app-dev.oxygen-hq.com` is — correctly — indistinguishable
> from any other page on your machine posting there with your cookie.
>
> Both ends loopback is the supported shape, and every spelling of it works:
> `localhost`, `127.0.0.1`, `[::1]`, `<slug>.localhost`, any port. (Before
> 2026-08 only eight literal `http://localhost:<port>` origins passed, so
> `127.0.0.1:5173` and any custom `OXY_DEV_PORT` failed with this same 403.)

> ### Kubernetes env-var collision
>
> These vars are named `OXY_HTTP_*` — **not** `OXY_PORT` / `OXY_INTERNAL_PORT`,
> and **not** `OXY_SERVE_*`. Kubernetes injects Docker-link service-discovery
> vars of the form `<SVCNAME>_PORT=tcp://<clusterIP>:<port>` into every pod, for
> every Service in the namespace. So an env var `OXY_<X>_PORT` is silently
> overwritten with `tcp://...` whenever a Service named `oxy-<x>` exists — and
> clap then can't parse `tcp://...` as a port, crashlooping the pod (exit 2,
> `invalid digit found in string`).
>
> - `OXY_PORT` collided with the prod Service `oxy`. It shipped in release
>   **0.5.90** and took prod down on **2026-06-29**. Staging was unaffected only
>   because its Service is named `oxy-staging` (→ `OXY_STAGING_PORT`), so the bug
>   passed pre-prod undetected.
> - `OXY_SERVE_PORT` is **also** unsafe: the split-fleet design
>   (`internal-docs/multi-instance-fleet.md`) adds `serve`/`ide`/`worker` roles,
>   and a Service fronting the serve replicas (plausibly `oxy-serve`) would
>   inject `OXY_SERVE_PORT` — the identical crashloop. `http` is not a fleet
>   role, so `oxy-http` is not a plausible Service name.
>
> Rule of thumb: never name an env var `OXY_PORT` or `OXY_<role>_PORT` where
> `oxy`/`oxy-<role>` could be a Service name. **The durable, name-independent fix
> is `enableServiceLinks: false` on the pod spec** (in the `oxy-app` Helm chart,
> `ghcr.io/oxy-hq/helm-charts`), which disables the injection mechanism entirely;
> the `OXY_HTTP_*` naming is just defense-in-depth for deployments that don't set
> it.

Give each checkout its own `.env` with a non-overlapping set of ports, and point
that checkout's frontend at its own backend. For example:

**Checkout A** — `.env`:

```bash
OXY_HTTP_PORT=3000
OXY_HTTP_INTERNAL_PORT=3001
OXY_DEV_PORT=5173
OXY_DEV_PROXY_TARGET=http://localhost:3000
```

**Checkout B** — `.env`:

```bash
OXY_HTTP_PORT=3100
OXY_HTTP_INTERNAL_PORT=3101
OXY_DEV_PORT=5273
OXY_DEV_PROXY_TARGET=http://localhost:3100
```

In each checkout, run `cargo run serve` and `pnpm run dev` as usual. Checkout A
is then at `http://localhost:5173` (API `3000`) and checkout B at
`http://localhost:5273` (API `3100`), fully isolated.

## Dev sign-in (browser automation without OAuth or email)

Local development runs the production path — cloud mode with magic-link auth —
so signing in normally means completing Google/Okta OAuth or opening the email
preview `MAGIC_LINK_LOCAL_TEST` writes to disk. Neither is scriptable, which
makes Playwright MCP, a scratch Playwright script, or any headless probe
useless against a local server.

There is a bypass that mints exactly the session the real login flows mint
(JWT + `oxy_session` cookie), for identities the operator pre-declared.

**On a debug build — `cargo run serve`, anything `just` builds — you configure
nothing.** The allow-list falls back to `OXY_GLOBAL_ADMINS`, which a dev box
already sets, so a fresh clone can drive the app immediately. (The pre-rename
`OXY_APP_ADMINS` is no longer read by anything; rename it if your `.env` predates
the change — the server logs an error at startup when only the old name is set.)

That inferred list is served to **loopback callers only**. `serve` binds
`0.0.0.0`, so without that limit a plain `cargo run serve` on office or café
wifi would hand every device on the network an unauthenticated session for a
real staff address — with nothing configured and nothing announcing it. Your own
browser and Playwright are on the same machine, so the zero-config path is
unaffected; a phone on the same wifi, a container, or a colleague's laptop gets
a 404. To serve those, name the identities explicitly — that is a deliberate act
and is honored for any peer:

```bash
# .env — comma-separated. The first entry is the default identity.
OXY_DEV_LOGIN_EMAILS=dev@oxy.local,member@oxy.local
OXY_DEV_LOGIN_EMAILS=            # set-but-empty: bypass off, admins untouched
```

Restart the server; the startup banner says dev sign-in is on and names the
variable that enabled it. Then:

| Want | Do |
| ---- | -- |
| A signed-in browser | navigate to `http://localhost:5173/dev-login` |
| …as a specific identity | `/dev-login?email=member@oxy.local` |
| …landing on a specific page | `/dev-login?next=/ide` |
| A token for `curl`/scripts | `curl 'http://localhost:3000/api/auth/dev-login?email=dev@oxy.local'` |
| A button to click | the **Dev sign-in** button on `/login` (only rendered when enabled) |

`GET` returns the token in the body and sets **no** session cookie; only the
`POST` the page uses does. That way a page you happen to have open can't
navigate your browser into a session behind your back — the session cookie is
`SameSite=Lax`, which would otherwise ride along on a top-level GET.

The identity is an ordinary user row, created on first use — so a first
sign-in against a fresh database has no orgs and lands on onboarding, exactly
like a real new user. List the same address in `OXY_OWNER` / `OXY_GLOBAL_ADMINS`
when you need staff reach.

For Playwright MCP that is the whole setup: one `browser_navigate` to
`/dev-login` and the browser holds a real session for the rest of the run. The
`playwright` server is declared in the repo's `.mcp.json` and pinned as a
`web-app` devDependency, so `pnpm install` is all it takes for Claude Code to
offer it (the package's binary is `playwright-mcp`).

If the first navigation complains about a missing browser, install it **through
the MCP package**, not from `web-app`:

```bash
pnpm --dir web-app/node_modules/@playwright/mcp exec playwright install chromium
```

The obvious `pnpm --dir web-app exec playwright install` is the wrong one and
will leave you in a loop. Browser revisions are pinned per Playwright version:
`@playwright/mcp` depends on its own `playwright` + `playwright-core` pair
(`1.63.0-alpha-2026-08-05` → chromium **1237**), while `web-app`'s direct dep is
`@playwright/test@1.62.1` → chromium **1234**, and pnpm only links direct
dependencies' binaries. Running it from inside the MCP package resolves the
right one without naming a version, so it survives a pin bump. Confirm either
way with `--dry-run`, which prints the install path.

Artifacts land in `.playwright-mcp/` relative to the **server's working
directory**, which is whatever cwd Claude Code was started in — the repo root in
the usual case, observed there rather than under `web-app/`. `pnpm --dir web-app`
resolves the *package* from `web-app` without changing the child process's cwd.
Either way it's gitignored: `.gitignore`'s `.playwright-mcp/` has no leading
slash, so it matches that directory at any depth.

**The guard rails, so this can't become a production hole:**

- **The roster fallback is debug-build only.** Production and every Docker image
  are release builds; they honor `OXY_DEV_LOGIN_EMAILS` and nothing else.
  `OXY_GLOBAL_ADMINS` *is* set on real deployments, so a release binary that
  honored it would hand anyone who can reach the server an unauthenticated
  **Global Admin** session.
- **…and loopback-only on top of that.** The two guards cover different axes:
  `debug_assertions` separates shipped from local, loopback separates this
  machine from the network. Only an explicit `OXY_DEV_LOGIN_EMAILS` — which
  somebody deliberately typed — is served off-box.
- No allow-list resolves ⇒ `/api/auth/dev-login` **404s**. A deployment that
  never sets the var is unaffected. (The `/dev-login` *page* is always routable
  — it exists to explain the refusal, and the server is the gate.)
- The endpoint only ever issues a session for an address **already listed** — an
  unlisted email is a 403, so a caller cannot name their way into someone
  else's account.
- Enabling it prints a warning at startup and logs one per issued session.

A dev box is cloud mode with non-prod secrets, so the server cannot tell itself
apart from production by mode — the build profile and this env var are the only
gates. Never set `OXY_DEV_LOGIN_EMAILS` on a deployment other people can reach.

## OAuth bounce proxy (Google / GitHub sign-in across instances)

OAuth providers validate the `redirect_uri` against a fixed allow-list, so every
dev port would otherwise need its own redirect URI registered with Google and
GitHub. The bounce proxy ([`scripts/oauth-bounce.mjs`](scripts/oauth-bounce.mjs))
solves this: you register **one** redirect URI per provider — the proxy's
origin — and it forwards each callback to the instance that started the flow
(identified by the instance origin appended to the OAuth `state`).

It covers Google sign-in and all three GitHub flows (login, account-connect, and
App-install).

### One-time provider setup

Register the proxy's callback URLs (default port `8429`):

- **Google** OAuth client → `http://localhost:8429/auth/google/callback`
- **GitHub OAuth app** → `http://localhost:8429/github/callback`
- **GitHub App** → add `http://localhost:8429/github/callback` as a callback URL
  (GitHub Apps accept multiple callback URLs)

### Per-instance config

Point every instance at the proxy by adding these to each checkout's `.env`:

```bash
OXY_OAUTH_PROXY_ORIGIN=http://localhost:8429    # frontend: redirect_uri target
OXY_OAUTH_REDIRECT_ORIGIN=http://localhost:8429 # backend: token-exchange + GitHub URL building
```

Both must be the same origin (the registered one). Leaving them unset preserves
the normal per-origin flow exactly.

### Run the proxy

Start it once (it is shared by every instance):

```bash
just oauth-proxy            # or: node scripts/oauth-bounce.mjs
```

The listen port can be overridden with `OXY_OAUTH_PROXY_PORT` (default `8429`);
keep it in sync with the origins above and the registered callback URLs. The
proxy only ever forwards to loopback origins, so it is safe to leave running.

## Contributing

Please read [CONTRIBUTING.md](CONTRIBUTING.md) for details on our code of conduct and the process for submitting pull requests.

## Database

Oxy uses PostgreSQL for data storage.

### Development Environment

There is no embedded PostgreSQL. `oxy start` (and so `just up`) runs one in Docker, managed
through `bollard` rather than docker-compose:

| | |
| - | - |
| Container | `oxy-postgres` (image `postgres:18-alpine`) |
| Address | `localhost:15432`, user `postgres`, password `postgres`, database `oxy` |
| Data | the `oxy-postgres-data` volume — `oxy start --clean` / `just up --clean` wipes it |

`oxy start` removes and recreates the container on every start (the volume survives), and
sets `OXY_DATABASE_URL` for its own process only — a separate `oxy seed` or `oxy compile`
needs it passed explicitly. Open a shell with
`docker exec -it oxy-postgres psql -U postgres -d oxy`.

### Production/Custom PostgreSQL

To use an external PostgreSQL database, set the `OXY_DATABASE_URL` environment variable:

```bash
export OXY_DATABASE_URL=postgresql://user:password@localhost:5432/oxy
```

### Running Migrations

Migrations are run automatically on startup. To run manually:

```bash
cargo run --bin migration
```

## HTTPS (Optional)

HTTPS is optional for day-to-day local development. Oxy can run locally without requiring TLS setup.

Use local HTTPS only when you need to test HTTPS-only behavior (for example, HTTP/2-only scenarios).

To enable HTTPS locally (backend and frontend), you need TLS certificates. We recommend using [mkcert](https://github.com/FiloSottile/mkcert):

### Install mkcert

**macOS:**

```sh
brew install mkcert
brew install nss # if you use Firefox
```

**Linux:**
Please check for instruction on [mkcert installation](https://github.com/FiloSottile/mkcert#linux).

Trust certificates from mkcert:

```sh
mkcert -install
```

We don't need to generate a self-signed cert for Oxy, as we already bundle a cert into the project.

If you want to run local development with HTTPS/HTTP2, use:

```bash
cargo run serve -- --http2-only
pnpm run dev
```
