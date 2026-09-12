## Overview

The **Oxy HTTP API** lets you query databases, manage threads, trigger
workflows, and configure workspaces programmatically.  All endpoints live
under `/api/` and are documented in the sections below.

---

## Authentication

Two schemes are accepted on every endpoint — supply one:

| Scheme | How to obtain | Header sent |
|--------|--------------|-------------|
| **API Key** | Generate in *Settings → API Keys* | `X-API-Key: <key>` |
| **Bearer (JWT)** | Issued by `oxyc login` or the magic-link flow | `Authorization: Bearer <token>` |

Use the **Authorize** button above to enter either credential; Swagger UI
will attach it to every "Try it out" request.

---

## CLI Quick-start

The terminal client is **`oxyc`** (`@oxy-hq/cli`, source in `sdk/cli`). It
replaced the old `oxy api`, `oxy login` and `oxy logout` subcommands, which
were removed from the `oxy` binary.

```bash
npm install -g @oxy-hq/cli
```

### `oxyc login` — authenticate the CLI

```
oxyc login [--env <env>] [--target <url>] [--login-env <env>...]
```

Opens your browser to the Oxy web app, captures the session JWT via a
loopback callback, and caches it in the credentials file, keyed by host so
dev / prod tokens are stored separately:
`~/Library/Application Support/oxy/credentials.json` on macOS,
`$XDG_CONFIG_HOME/oxy/credentials.json` on Linux (default `~/.config`). It is the same file the old
`oxy login` wrote, so an existing login keeps working.

```bash
oxyc login                        # authenticate against production
oxyc login --env local            # authenticate against a local oxy serve
oxyc login --env dev --login-env staging   # several deployments, one browser flow each
oxyc login --target https://my.oxy.example.com
```

After login, every `oxyc api` call for that target is automatically
authenticated — no manual token management needed.

```bash
oxyc logout                       # clear the cached token for the default env
oxyc logout --env local
```

---

### `oxyc` — call the HTTP API from the terminal

```
oxyc api <path> [-X METHOD] [-f k=v] [-F k=v] [-H 'Name: value']
oxyc routes [FILTER] [--json]      # every endpoint THIS deployment mounts
oxyc schema <path> [-X METHOD]     # request/response shape for one endpoint
oxyc openapi                       # this very document
```

An authenticated `gh api`-style client. The path is taken relative to the
target's `/api/` surface; a leading `/` or `api/` prefix is normalised, and a
`/external/api/...` path selects the API-key surface automatically.

Discovery is served by this deployment rather than baked into a binary, via
`GET /api/_catalog` — so the list describes the routes actually mounted here,
not the routes some build could have mounted. `oxyc` caches it per host.

```bash
# GET examples
oxyc api user                                   # GET /api/user
oxyc api projects/<id>/agents                   # GET /api/projects/<id>/agents
oxyc api threads --env local                    # hit a local oxy serve

# POST with JSON fields. `-f` is a string, `-F` keeps the JSON type.
oxyc api projects/<id>/runs -f workflow_path=my-workflow.yml
oxyc api admin/compiles/run -F promote=true -F 'ids[]=a' -F 'ids[]=b'

# POST with a raw body: inline, from a file, or from stdin
oxyc api projects/<id>/query --input '{"sql":"select 1"}'
oxyc api projects/<id>/query --input @payload.json
echo '{"sql":"select 1"}' | oxyc api projects/<id>/query --input -

# Custom method / headers
oxyc api projects/<id>/runs/<run_id> -X DELETE
oxyc api something -H 'X-My-Header: value'

# Shape the output: jq server-side, or a markdown table
oxyc api threads --jq '.threads[].id'
oxyc api <workspace_id>/sql/query -f 'sql=select 1' -f database=<name> --md

# Print the bearer token (useful for raw curl calls)
oxyc token --env local
```

The `--env` flag resolves the base URL from an `oxy-app.json`
`environments` map in the current directory, or from the built-in defaults
(`local` → `http://localhost:5173`, `production` → your cloud workspace).
Pass `--target <url>` to override it explicitly.

---

## Base URL

All paths in this document are relative to `/api`.  When using `oxyc api` the
prefix is added automatically.
