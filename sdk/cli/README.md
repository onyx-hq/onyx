# `@oxy-hq/cli`

`oxyc` — a `gh api`-shaped client for the Oxy HTTP API, plus the tooling that
manages customer workspace repos.

- **API client** — `api`, `routes`, `schema`, `openapi`, `login`, `whoami`, `assume`
- **Customer workspaces** — `list`, `new`, `import`, `doctor`, `update`, `adopt`, `launch`
- **Custom apps** — `publish`, `init-ci`, `proxy`
- **Development** — `validate`, `mcp`, `guide`, `skills`

## Install

```bash
npm install -g @oxy-hq/cli      # then: oxyc
npx @oxy-hq/cli routes          # zero-install; works for everything but `skills install`
```

From a checkout:

```bash
cd sdk/cli && pnpm install && pnpm build
node dist/main.mjs --help
pnpm link --global              # or, for `oxyc` on PATH
```

Requires `node >= 20`. `gh` (authenticated) is required for the customer
commands, `jq` for `--jq`, `git` for the repo commands. Each is reported by
name with its install line the first time a command needs it.

## Quick start

```bash
oxyc login --env dev                        # browser flow; stores a token
oxyc whoami --env dev                       # verify it against the deployment

oxyc routes threads                         # find the endpoint
oxyc schema {workspace}/threads -X POST     # find the body it takes
oxyc api {workspace}/threads --md           # call it
```

## Global flags

Accepted by every command except `validate`, `guide`, `exit-codes`, `skills`
and `cache`.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--env <name\|url>` | `production` | deployment to target |
| `--target <url>` | — | explicit base URL; overrides `--env` |
| `--token-env <VAR>` | `OXY_TOKEN` | env var holding the bearer token |
| `--api-key-env <VAR>` | `OXY_API_KEY` | env var holding the key for `/external/api` |
| `--org <slug>` | — | value for the `{org}` placeholder |
| `--workspace <id>` | — | value for the `{workspace}` placeholder |
| `--project <id>` | — | value for the `{project}` placeholder |
| `--customer <name>` | — | act as though run inside this customer's repo |
| `--quiet` | — | suppress progress messages on stderr |

### Environments

`--env` takes a name or any URL. Anything not in this table is used as a URL.

| `--env` | URL |
| --- | --- |
| `local` | `http://localhost:5173` |
| `dev`, `development` | `https://aip.dev.oxy.tech` |
| `staging` | `https://aip.staging.oxy.tech` |
| `production`, `prod` (default) | `https://app.oxygen-hq.com` |

Credentials are stored **per deployment**, and the default is production. A
401 against dev usually means you are logged into prod — pass `--env` to
`login` and to the call.

## `oxyc api`

```
oxyc api <path> [flags]
```

`<path>` is relative to `/api`; a leading `/` or `api/` is accepted.

| Flag | Meaning |
| --- | --- |
| `-X, --method <verb>` | HTTP method. Default GET, or POST when a body is present. |
| `-f, --raw-field <k=v>` | string parameter |
| `-F, --field <k=v>` | typed parameter — `true`, `3`, `["a"]`, `@file`, `@-` |
| `-H, --header <name:value>` | extra header (repeatable) |
| `--input <file\|->` | raw request body from a file, or `-` for stdin |
| `-q, --jq <expr>` | filter the response through `jq` |
| `--md` | render the result as a markdown table |
| `--paginate` | follow every page and return one document |
| `--paginate-key <field>` | the field holding the rows, when the guess is wrong |
| `--max-pages <n>` | stop after `n` pages (default 100) |
| `--slurp` | with `--paginate`, emit an array of pages instead of merging |
| `--cache <duration>` | reuse a recent successful GET (`30s`, `5m`, `2h`) |
| `-i, --include` | print the status line and response headers |
| `--silent` | make the request, print nothing |
| `--verbose` | log the request before making it |
| `--timeout <duration>` | request timeout (default `2m`) |

**Bodies.** `-F 'ids[]=a' -F 'ids[]=b'` accumulates into an array. On GET, HEAD
and DELETE, fields become query parameters instead of a body.

**Placeholders.** `{org}`, `{workspace}`, `{project}`, `{customer}` and `{me}`
are resolved from the customer repo you are standing in, from an org URL passed
to `--env`, or from `--org` / `--workspace` / `--project`. An unresolvable
placeholder is an error naming the flag that fills it — never a literal sent to
the server.

**Surfaces.** The path selects the credential: `/api/**` sends the bearer from
`oxyc login`; `/external/api/**` sends `X-API-Key` from `$OXY_API_KEY`.

**`--paginate` is a heuristic.** Oxy sends `Link: rel="next"` on some endpoints
and nothing on others; where there is no header, `oxyc` reads
`pagination.has_next`, then `has_more`, then `page < total_pages`. Nothing
recognised means one page, and it warns when the server said nothing at all.

```bash
oxyc api {org}/workspaces --md
oxyc api {workspace}/threads -q '.threads[].title'
oxyc api {workspace}/sql/query -X POST -f 'sql=select 1' --md
oxyc api /admin/users --paginate --md
```

## Discovery

```bash
oxyc routes [filter] [--json] [--all] [--refresh]   # endpoints this deployment mounts
oxyc schema <path> [-X <verb>]                      # request/response shape for one
oxyc openapi                                        # the whole OpenAPI document
```

`routes` is served by `GET /api/_catalog`, so it describes the deployment you
are talking to rather than a baked table. It is cached per host; `--refresh`
re-asks. `--all` includes ide-only and worker-only mounts. `--json` adds each
route's surface and fleet role.

## Authentication

```bash
oxyc login [--env <e>] [--login-env <e...>] [--assume <org> -r <why>]
oxyc whoami [--json]
oxyc token                  # print the bearer, for a raw curl
oxyc logout
```

`--login-env` is repeatable and comma-separated (`--login-env dev,staging`);
the browser opens once per environment, in sequence.

Credentials live in the OS config directory under **`oxy`** — the same file the
Rust `oxy login` wrote before it was removed, so an existing login still works:

- macOS: `~/Library/Application Support/oxy/credentials.json`
- Linux: `$XDG_CONFIG_HOME/oxy/credentials.json`

`OXY_CREDENTIALS_PATH` overrides the path. Caches are separate, under `oxyc`
(`~/.cache/oxyc`). In CI set `OXY_TOKEN` instead of logging in.

## Acting as an organization

```bash
oxyc assume start --org <slug|uuid|url> -r "<reason>"
oxyc assume status [--json]
oxyc assume end [--org <slug>] [--all]
```

Sessions last **60 minutes and are not renewable** — re-running `start` returns
the existing session rather than extending it. `--org` is optional when `--env`
is itself an org URL. The reason is recorded in the audit log.

The session belongs to your account, not this terminal, so your browser shares
it and `end` ends it in both. A staff 403 on a tenant surface is usually no
active session rather than a role problem; `assume status` answers "none"
without failing.

## Customer workspaces

A customer **is** a GitHub repo carrying the `oxy-customer` topic. Tagging the
repo is the whole of registration; there is no separate registry file. These
commands need `gh` authenticated.

```bash
oxyc list [--json] [--refresh]        # the customers
oxyc path <customer>                  # where their repo is on this machine
oxyc new <customer> [--display <name>]        # create, tag and scaffold a repo
oxyc import <org/repo> [--clone]              # tag an existing repo
oxyc remove|rm <customer> [--purge --yes]     # untag (never deletes the repo)
oxyc doctor [<customer>] [--all]              # report state, change nothing
oxyc update <customer> [--apply] [--diff-all] # drift from the workspace template
oxyc adopt <customer> [--apply]               # install managed files an import lacks
oxyc activity <customer> [--since <date>] [--repo <org/name>] [--write] [--json]
oxyc launch <customer> [claude-args...] [--here] [--dry-run]
oxyc repos [--refresh]                # where OUR repos are checked out
```

`update` and `adopt` **report by default and write only with `--apply`**.
Neither commits, branches or pushes. What they may rewrite is decided by
`template/.oxyc-managed`: an unclassified file belongs to the customer and is
never touched.

`launch` starts a Claude Code session scoped to one customer; `--here` runs in
the current directory while granting access to the customer's repo.

## Custom apps

```bash
oxyc publish [--env <e>] [--dir <path>] [--promote] [--build-only | --prebuilt] [--json]
oxyc init-ci [--app <org>/<app>] [--environment <name>] [--force]
oxyc proxy [--port <n>] [--allow-writes] [--allow-events] [--yes]
```

**`publish`**, from an app directory, runs `oxy-app.json`'s build (default
`pnpm install`, `pnpm build`, output `out/`), bundles each declared Oxy Function
with `pnpm exec esbuild` into `functions/<name>.js`, resolves the project from
the target's public `build-config`, and uploads the bundle — to the **draft**
channel unless `--promote`. `--env` defaults to **production**; name it.

- **Identity**: `--org` / `--app`, then `OXY_ORG` / `OXY_APP`, then the manifest's
  `orgSlug` / `slug`, then an `apps/<org>/<app>/` working directory. `--org`
  takes a slug or a UUID. `--project` pins the workspace (and implies its org).
- **`--dir`** publishes a pre-built directory instead of building; functions are
  still bundled into it. **`--build-only`** stops after building and bundling —
  no credential, no network unless the org must come from `--project`.
  **`--prebuilt`** (with `--dir`) skips esbuild and refuses if a declared
  function's `functions/<name>.js` is missing. The pair splits CI so the job that
  runs package scripts never holds the credential.
- **Auth**: the `--token-env` variable (`OXY_TOKEN`), then the login cache — or,
  in a GitHub Actions job with `id-token: write` and neither set, **trusted
  publishing**: the job's OIDC token is exchanged for a credential scoped to this
  one app. That needs the org **slug** and a publisher registered for the
  workflow (see `init-ci`).
- **Provenance**: the checkout's `origin`, `HEAD` and branch (else `GITHUB_SHA` /
  `GITHUB_REF_NAME`) are recorded; a missing half or a dirty app directory is a
  warning, never a failure. The build id defaults to
  `$GITHUB_SHA-$GITHUB_RUN_ID.$GITHUB_RUN_ATTEMPT`, else random — a reused one is
  a `409`.
- `.env.local`, then `.env`, are read from the directory and its parents without
  overriding the environment. The server's warnings print on stderr; `--json`
  prints its result on stdout.

**`init-ci`** writes `.github/workflows/oxy-publish.yml` at the repo root: a
`build` job (no id-token) that runs `publish --build-only`, and an
environment-gated `publish` job whose only work is `publish --prebuilt` with
OIDC. It prints the `oxyc api …/publishers` call that registers the workflow.

## Development commands

```bash
oxyc validate [-f <file>] [--json]
oxyc mcp
oxyc guide
oxyc skills install | list
oxyc cache clear
oxyc exit-codes
```

**`validate`** checks workspace YAML against `json-schemas/*.json`, which are
generated from the Rust config types. No network, no token.

| File | Schema |
| --- | --- |
| `config.yml` / `config.yaml` | `config.json` |
| `*.automation.yml`, `*.procedure.yml`, `*.workflow.yml` | `workflow.json` |
| `*.agentic.yml` | `agentic.json` |
| `*.app.yml` | `app.json` |
| `*.agent.test.yml` | `agent-test.json` |

Structural checks only. `oxy validate` additionally resolves `databases:` and
`llm.ref` against the loaded workspace, and wins where the two disagree.

**`proxy`** forwards a local dev server's Oxy calls to a cloud target with your
login token attached. Defaults: side-effecting calls are **held**, tracking
events are **dropped**, auth endpoints reach the backend unauthenticated so
sign-in works, and the cached token never overrides a real browser session. A
production target (any host under `oxygen-hq.com`) is refused without `--yes`.
Each Oxy Function call carries a W3C trace — the SDK's, or one minted here — and
prints `↳ <status> fn <name>  request_id=…  trace_id=…`.

**`mcp`** serves the API over stdio as four tools — `oxy_routes`, `oxy_schema`,
`oxy_request`, `oxy_whoami` — rather than one per endpoint, so the tool schemas
cost ~2 KB per turn and reach endpoints added after this package shipped.

```bash
claude mcp add oxyc -- npx -y @oxy-hq/cli mcp --env production
```

**`guide`** prints a page to paste into `AGENTS.md` / `CLAUDE.md`.
**`skills install`** symlinks the six bundled Claude skills into
`~/.claude/skills`; it refuses to run from an `npx` cache, whose symlinks dangle
once npm reclaims it.

## Exit codes

```
0  success
1  failure with nothing more specific to say
2  usage error — a bad flag, a missing argument, a malformed value
4  not authenticated, or the token was rejected (401/403)
5  not found (404), or an unknown customer
6  the request was malformed (4xx other than 401/403/404)
7  unavailable — 5xx, a timeout, or the network failed. Retryable.
8  refused — the operation would have destroyed or overwritten something
```

`4` almost always means the wrong `--env`. `7` is worth retrying; `6` never is.

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `OXY_TOKEN` | — | bearer token, overriding the login cache (the CI path) |
| `OXY_API_KEY` | — | key for the `/external/api` surface |
| `OXY_CREDENTIALS_PATH` | OS config dir | the shared `credentials.json` |
| `OXYC_ORG` | `oxy-hq` | GitHub org the customer repos live in |
| `OXYC_CUSTOMER_TOPIC` | `oxy-customer` | topic that registers a customer repo |
| `OXYC_DOSSIER_ROOT` | `~/.oxyc/dossiers` | where customer clones go |
| `OXYC_REPO_ROOTS` | scanned | where to look for our own checkouts |
| `OXYC_TEMPLATE_DIR` | shipped `template/` | use a working copy instead |
| `OXYC_SCHEMAS_DIR` | shipped `json-schemas/` | use a working copy instead |
| `OXYC_SKILLS_DIR` | shipped `skills/` | use a working copy instead |
| `OXYC_SKILLS_TARGET` | `~/.claude/skills` | where `skills install` links |
| `OXYC_CACHE_DIR` | `<cache>/oxyc` | cache root |
| `OXYC_CACHE_TTL` | `3600` | seconds the customer listing is cached |
| `OXYC_LIST_LIMIT` | `1000` | max repos listed from GitHub |
| `OXYC_SEARCH_LIMIT` | `1000` | max pull requests searched by `activity` |
| `OXYC_DIFF_LINES` | — | cap on diff lines printed by `update` |
| `OXYC_DEBUG` | — | `1` keeps the stack on an unexpected throw |
| `OXYC_QUIET` | — | `1` is `--quiet` |
| `OXYC_DRY_RUN` | — | `1` makes `launch` print the command instead of running it |
| `NO_COLOR` / `FORCE_COLOR` | — | force plain / coloured output |

## Output contract

**stdout carries the response body and nothing else.** Progress, warnings,
errors and hints go to stderr, and a failure never exits `0`.

Attached to a TTY you get colour, aligned tables and a searchable picker.
Piped, you get markdown tables and no colour. Raw `api` response bodies are
byte-identical in both, so `| jq` always works.

## Development

```bash
pnpm install
pnpm build          # tsdown; `prebuild` runs codegen
pnpm test           # vitest; `pretest` builds
pnpm typecheck
pnpm lint           # biome
pnpm build:binary   # standalone executables (needs bun)
```

The package ships four directories — `dist`, `json-schemas`, `skills`,
`template` — and the last three are resolved at runtime relative to
`package.json`. `scripts/ci/verify-cli-package.mjs` checks the packed tarball
on every PR.

Maintainer's notes — release process, catalog generation, OpenAPI curation, CI
gates: [`internal-docs/oxy-api-cli.md`](../../internal-docs/oxy-api-cli.md).
