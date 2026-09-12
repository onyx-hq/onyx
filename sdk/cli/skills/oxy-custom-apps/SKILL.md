---
name: oxy-custom-apps
description: Use when building, debugging, reviewing, or shipping an Oxy custom-app bundle (a code-first React/Vite app served by oxy), including writing Oxy Functions and driving them from the UI. Encodes the contract rules, pitfalls, performance guardrails, and the self-serve `oxyc publish` ship flow.
---

# Customer-app development

Customer-app bundles are arbitrary JS apps served by oxy at
`https://<env>/customer-apps/<org>/<slug>/`. Oxy is a data gateway:
serves static assets, gates cookie auth, and proxies queries. The
bundle is opaque above the data plane — no per-app server config.

## The Oxy App Kit (use it)

Two small packages remove the footguns — default to them rather than wiring a
bundle by hand:

- **`@oxy-hq/sdk`** — the data layer (`useQuery`, `useSemanticQuery`,
  `useAgentRun`, `<OxyChat>`); see rule 2 and `oxy-hq/customer-apps:
  docs/sdk-flow.md`.
- **`@oxy-hq/vite-plugin`** (`oxyApp()` in `vite.config.ts`) — bakes the asset
  **base path** from the manifest, copies `oxy-app.json` into `out/`, proxies
  `/api` in dev, and injects the `window.__OXY_APP__` shim. Dropping it
  re-introduces the #1 bug — asset 404s → blank dashboard (rule 3).

Scaffold both with `@oxy-hq/create-oxy-app` or by copying `oxy-hq/customer-apps:
examples/hello-oxy/`. The
platform is kit-agnostic (any bundle with a correct base path works), but
hand-rolling means owning the base-path + dev-proxy wiring yourself.

(**`oxy-hq/customer-apps:` prefixes a path in that repo**, not in whatever repo
you are sitting in. This skill loads globally, so every such pointer is
qualified.)

## Three rules

1. **`oxy-app.json` (project root, not `public/`) is identity only.**
   Schema:

   ```json
   {
     "schemaVersion": 2,
     "slug": "url-slug",
     "orgSlug": "your-org",
     "name": "Display Name"
   }
   ```

   Required: `schemaVersion` (must be `2`), `slug` (non-empty).
   Strongly recommended: `orgSlug` (lets `@oxy-hq/vite-plugin` derive
   the asset base path automatically, so plain `pnpm build` produces
   a bundle ready to drop into the linked org with no env var).
   Optional: `name` (display), `projectId` (dev-time hint).

   At serve time, oxy injects the authoritative org + project from
   the admin row via `window.__OXY_APP__` — the manifest's `orgSlug`
   and `projectId` are advisory hints only, never enforcement.

   Do not invent `products`, `writers`, or `from:`-blocks. Those
   don't exist in v2 and the probe rejects them.

   File location: **`<root>/oxy-app.json`**, sibling to
   `vite.config.ts`. The vite-plugin reads it from there; legacy
   bundles that kept it under `public/` won't see the plugin's
   base-path derivation kick in.

2. **All data flows through SDK hooks → oxy HTTP endpoints.** The
   surface is intentionally small:

   | Hook / component | Endpoint | When |
   | ---- | -------- | ---- |
   | `useQuery({sql})` | `POST /api/projects/:id/query` | Raw SQL, 10k row cap |
   | `useSemanticQuery({topic, …})` | `POST /api/projects/:id/semantic-query` | Reference topics/measures by name |
   | `useAgentRun({agentId})` | `POST /api/projects/:id/agents/:id/asks` + SSE | Streaming chat / Q&A |
   | `useProcedureRun({procedureId})` | `POST /api/projects/:id/procedures/:id/runs` ⚠️ beta | Long-running batch |
   | `useFunction(name)` | `POST /customer-apps/<org>/<slug>/fn/<name>` + SSE | Invoke a server-side **Oxy Function** (see below) |
   | `<OxyChat agentId="…"/>` | wraps `useAgentRun` | Drop-in chat UI |
   | `<OxyAnswer {...run}/>` | renders any run | Drop-in answer renderer |

   ```tsx
   import { OxyChat, useQuery } from "@oxy-hq/sdk";
   const { rows } = useQuery({
     sql: "SELECT col FROM t WHERE x = {{ params.x | sqlquote }}",
   }, { params: { x: someState } });
   // ...
   <OxyChat agentId="analytics" />
   ```

   See `oxy-hq/customer-apps: docs/sdk-flow.md` for the full reference,
   including `OxyApiError` parsing and beta-surface caveats.

   **Server-side work → Oxy Functions, not the browser.** `useQuery` &
   friends run in the client; for work the browser shouldn't do
   (warehouse writes, ELT kick-offs, external API calls, secret-bearing
   logic) ship a **TypeScript handler under `functions/`** and invoke it
   with `useFunction(name).invoke(body?)`. Declare it in `oxy-app.json`:

   ```json
   { "schemaVersion": 2, "slug": "my-app",
     "functions": { "daily-rollup": { "route": true, "timeoutSeconds": 30 } } }
   ```

   ```ts
   // functions/daily-rollup.ts — runs on Oxy's isolate runtime
   export default async (req, ctx) => {
     const { rows } = await ctx.query("SELECT … FROM …");   // data-plane ctx
     return Response.json({ rows });
   };
   ```

   `ctx` bridges to the data plane: `ctx.query` / `ctx.queryStream`,
   `ctx.semantic.query`, `ctx.warehouse.{insert,exec,upsert}`,
   `ctx.airway.run`, `ctx.fetch` (SSRF-allowlisted), `ctx.env`,
   `ctx.user`, `ctx.log`. `oxyc publish` bundles `functions/*.ts` with
   esbuild — no separate backend to stand up. Worked example +
   author-facing `ctx` types: `oxy-hq/customer-apps:
   examples/hello-oxy/functions/`.

3. **`OXY_APP_BASE_PATH` must match the path the bundle is linked
   under.** `@oxy-hq/vite-plugin` resolves it from (in order):
   `OXY_APP_BASE_PATH` env → `orgSlug + slug` in `oxy-app.json` →
   `/`. `oxyc publish` sets that env to `/customer-apps/<org>/<slug>/`
   for the build automatically, and the vite-plugin derives the same
   from the manifest for `pnpm dev` / `pnpm build`. Mismatch → asset
   404s → blank dashboard.

## Four pitfalls

- **CORS errors are usually wrong-URL errors in disguise.** The SDK's
  base URL must be `${OXY_URL}/api`, not `OXY_URL` bare. The API
  router (which carries CORS headers) lives under `/api/*`. Hitting
  `/<projectId>/...` lands in the static fallback and the missing
  headers look like a CORS failure.

- **Cookie auth requires `credentials: "include"`.** The SDK's
  `useQuery` sets this. If you fetch directly, set it yourself or the
  `oxy_session` cookie won't be forwarded.

- **`projectId` in the manifest is advisory, not enforcement.** The
  SDK uses it to construct the query URL. Authorization is the user's
  cookie → user → org_member of the project's org, decided
  server-side. A bundle that ships a stolen `projectId` can't read
  data the user can't already read.

- **Cloud mode is the target.** Don't branch on local vs. cloud; don't
  rely on the nil UUID `00000000-...` as a default project; don't
  assume `OXY_STATE_DIR` is writable.

## Performance guardrails

Any app that ships an Oxy Function or drives a batch of them should keep these —
cheap up front, hard to retrofit. (Established by the pokehouse `post-je` function.)

**In an Oxy Function — count the round-trips.** Each isolate run is stateless and
pays a fresh network hop per `ctx.query` / `ctx.warehouse.*`, so serial *dependent*
calls dominate latency.
- Run independent data-plane calls with `Promise.all`, not sequential `await`
  (e.g. resolve reference data **and** provision tables concurrently — only the
  final step needs both).
- Fold a check into a query that already runs: a duplicate/existence guard is a
  scalar subquery on the main `SELECT`, not a second round-trip.
- `Promise.all` idempotent DDL (`CREATE TABLE IF NOT EXISTS`); better, keep
  provisioning off the hot path if a migration can own it.
- A "cheap" extra `ctx.query`/`ctx.log` is N× the cost across N invocations.

**Driving many function calls from the UI (bulk actions) — don't go serial.**
- Independent calls run with **bounded concurrency** (~3–4) via a worker-pool over a
  queue, not one-at-a-time — turns an O(N) wall-clock batch into a few waves.
- If the calls share mutable server state that races under concurrency (a rotating
  token, a lazily-created table), the best fix is to **remove the shared mutation**
  (e.g. a single-writer scheduled refresher owns the token; requests only read).
  When you can't, warm the state with one call first, then fan out.
- Don't refetch derived state (a status ledger) after *every* item — the progress UI
  is the live feedback; **throttle** the refetch (~1/s) and flush once at the end.
- Guard `setState` after the component may have unmounted mid-batch (`mounted` ref).

**Trust the server, not the client body.** Environment / tenant / which-secret
selection is derived server-side (a function secret, a server-side mapping table,
the authed identity), never from the request body — validate or override anything
the client sends that picks a target.

**Reuse the SDK's data-plane caching.** `useQuery` already dedups concurrent identical
queries and short-caches results — don't hand-roll a fetch that bypasses it. Mark
read-only functions `cache: { ttlSeconds }` in the manifest; **never** cache a
side-effecting one (it silently drops writes). `idempotencyKey` = a fresh UUID
per ATTEMPT (click), never a stable per-subject key: the server stores each
key's response indefinitely and replays it WITHOUT running the function, so a
`store:period` key silently breaks any post → compensate (delete) → post-again
cycle — the second post "succeeds" with the stale response and writes nothing.
The key shields one request from transport duplication; cross-attempt dedup
belongs in the function itself (a server-side guard over real state, e.g. a
posted-ledger row the compensating action clears).

**Watch-outs when optimizing (each surfaced in review):**
- **Clear UI running-state in `finally`.** An async batch driver must reset its
  `running`/loading flag (and do its final refetch) in a `finally`, not after the
  `await` — a stray throw otherwise leaves the bar stuck at "…" with the button
  disabled.
- **Parallelizing can move a side effect ahead of a gate.** Running an independent
  side-effecting call concurrently with a validation query means it can now fire
  even when validation would reject. Fine if the side effect is benign — just know
  you traded the ordering for the latency.
- **Additive migration for a scoped guard → backfill legacy rows to their TRUE value,
  not a constant.** When a dedup/uniqueness guard starts keying on a new column,
  `ADD COLUMN … DEFAULT <constant>` silently defeats the guard for pre-existing rows.
  Derive the real value from an existing column (e.g. a DocNumber prefix) so old rows
  stay correctly scoped.

## Minimal starter

Easiest path: `pnpm dlx @oxy-hq/create-oxy-app@latest my-app --template vite`.
The scaffolded layout:

```
my-app/
├── oxy-app.json            ← project root, not public/
├── src/
│   ├── App.tsx
│   └── main.tsx
├── package.json
├── vite.config.ts
└── tsconfig.json
```

`oxy-app.json`:
```json
{ "schemaVersion": 2, "slug": "my-app", "orgSlug": "test", "name": "My App" }
```

`src/main.tsx`:
```tsx
import { OxyAppProvider } from "@oxy-hq/sdk";
import { createRoot } from "react-dom/client";
import { App } from "./App";

createRoot(document.getElementById("root")!).render(
  <OxyAppProvider><App /></OxyAppProvider>
);
```

`src/App.tsx`:
```tsx
import { useQuery } from "@oxy-hq/sdk";

export function App() {
  const { rows, loading, error } = useQuery({
    sql: "SELECT 1 AS hello",
  });
  if (loading) return <p>loading…</p>;
  if (error) return <p style={{ color: "red" }}>{error.message}</p>;
  return <pre>{JSON.stringify(rows, null, 2)}</pre>;
}
```

`vite.config.ts`:
```ts
import oxyApp from "@oxy-hq/vite-plugin";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react(), oxyApp()],
});
```

The plugin handles `outDir = "out"`, base-path resolution from the
manifest, dev-server proxy to `/api`, and the `window.__OXY_APP__`
dev shim. Plain `pnpm build` produces a bundle with the correct
asset base baked in.

## Publishing

Ship with `oxyc publish` — **no CI, no S3 sync, no folder picker**. From
the app directory (`oxy-hq/customer-apps: examples/hello-oxy/` is the worked
shell; note its manifest deliberately carries no `orgSlug`, leaving the org to
be resolved at publish time):

```bash
oxyc login --env production    # once; browser flow, caches a token,
                               # and prints whether you're an app-admin
oxyc publish --env production           # build → tar → upload (draft channel)
oxyc publish --env production --promote # …straight to live
```

- **Identity** is the manifest's `slug` + `orgSlug` (override with
  `--app` / `--org`). The server upserts the app row — there is no
  separate `oxy apps create` step.
- **First publish of a new app** needs `--project <workspace-uuid>`: the
  build-config lookup can't resolve a project until the app exists. Drop
  the flag after the first publish — it's auto-resolved from then on.
- **`--env` targets:** `local` → the oxy web-app Vite dev server `:5173`
  (run `pnpm dev` there; it serves `/cli-auth` and proxies `/api` to
  oxy on `:3000`), `dev`/`staging`/`production` →
  `app{-dev,-staging,}.oxygen-hq.com`.
- `--dir out` skips the build and uploads a prebuilt directory.
- Promote / roll back / unpublish from the oxy admin app-detail console;
  every publish is audited (who shipped each build, when).

Full hands-on guide: `oxy-hq/customer-apps: docs/local-development.md`.

## Reading data while vibe-coding

You don't need an `X-API-Key` to poke at the data API by hand — your
`oxyc login` token authenticates as a bearer. From an app directory:

```bash
oxyc login --env local       # once; caches a token per host

# gh-api-style client (resolves the target + bearer for you, no headers):
oxyc api user --env local                                   # who am I?
oxyc api projects/<projectId>/query --env local -f sql='select 1'

# raw curl with your token (e.g. to script or debug):
curl -H "Authorization: Bearer $(oxyc token --env local)" \
     -H 'content-type: application/json' \
     localhost:5173/api/projects/<projectId>/query -d '{"sql":"select 1"}'
```

`oxyc api <path>` takes the path relative to `/api/`; `-X` sets the method
(POST when a body is given), `-f`/`-F` set the body (string / JSON-typed),
`--input` sends a raw one, `-i` includes response headers. `--env` resolves the
same targets as `oxyc publish` (`local` → the Vite dev server `:5173`, which
proxies `/api` → oxy `:3000`).

Do not guess a path: `oxyc routes <filter>` lists what exists and
`oxyc schema <path>` gives the body it expects.

Access is your **normal org membership** — fine for your own test
workspace, not a cross-customer backdoor. The `projectId` is the one the
vite-plugin already resolved for the app (it's in `window.__OXY_APP__`).
