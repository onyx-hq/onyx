---
name: oxy-customer-apps-perf
description: Use when adding or modifying a custom-app serving route (crates/app/src/server/api/custom_apps_serve.rs, the /customer-apps/{*path} handler in serve.rs) or a custom-app data endpoint (projects/query.rs, projects/semantic_query.rs). Encodes the serve-plane + data-plane performance guardrails so every custom app stays fast. Triggers on "custom-app cache", "Cache-Control", "ETag", "compression on customer-apps", "result cache", "project-scoped cache", or a new per-request read on the custom-app hot path.
---

# Customer-apps performance (serve + data plane)

The perf baseline every custom-app request must keep, established by PR #2634
(full rationale, threat model, and future options: `internal-docs/customer-apps-performance.md`).
When you touch the serve or data path, preserve these — they are cheap to keep and
expensive to retrofit under load.

## Serve plane — `custom_apps_serve.rs`, `cli/commands/serve.rs`

- **Content-hashed assets → immutable Cache-Control.** URLs under `assets/`
  (Vite / Astro / Rsbuild / SvelteKit) and `_next/static/` (Next) change only when
  their bytes change → `public, max-age=31536000, immutable`. HTML →
  `private, no-cache`; unfingerprinted root files → `public, max-age=300`, unless
  their `?v=` names the build being served (the launcher's icon/art URLs) → immutable.
  This is `cache_control_for`; a new hashed-asset dir means adding its prefix there. Without
  it, every chunk re-runs the full auth + membership walk in `serve_inner`.
- **HTML stays `private`.** It carries a per-visitor tracking `Set-Cookie`, and
  `no-cache` alone still lets a *shared* cache store the response — which would hand
  every later visitor one session id. Do not "simplify" this to `no-cache`.
- **The resolution chain must stay fully cached.** Warm steady state is **zero** DB
  round-trips per asset. Adding an uncached lookup to `serve_pretty` /
  `serve_from_s3_build` costs ~100 queries per page load on its own. New app-row
  state means a new `invalidate_app_resolution_cache()` call site.
- **Assets are pre-compressed at publish, not per request** (`custom_apps_precompress`).
  A new compressible output type belongs in `is_precompressible_extension`; HTML must
  stay excluded (it is rewritten at serve time). Any response that ships
  `Content-Encoding` MUST also ship `Vary: accept-encoding`.
- **HTML → weak ETag + `If-None-Match` 304.** Injected HTML is a transform, so use a
  weak `W/"…"` ETag over the *final* bytes (`etag_for` / `if_none_match` in
  `serve_from_s3_build`); a matching `If-None-Match` returns 304 and skips the
  re-inject.
- **Compression is SSE-safe.** `CompressionLayer` on `/customer-apps/{*path}` relies
  on `DefaultPredicate`, which skips `text/event-stream` and already-encoded bodies —
  so the `/fn` SSE stream and the v0 proxy are untouched. If you add a streaming
  response under this route, confirm it stays `text/event-stream` (or keep it off the
  layer); never compress a stream.

## Data plane — `projects/query.rs`, `projects/semantic_query.rs`, `projects/result_cache.rs`

- **Short-TTL result cache, keyed by `project_id` FIRST.** The cache key is
  `(project_id, namespace, database, sql)`. `project_id` is the multi-tenant
  isolation boundary — a shared process-global cache without it **leaks results
  across tenants**. `result_cache.rs` has a `miss_on_different_project_or_sql_or_db`
  test asserting exactly this; keep it. Namespace by endpoint (`query` vs `semantic`).
- **Read the cache AFTER auth.** The `result_cache::get` must sit *below*
  `check_custom_app_gates` in the handler, or a cached hit bypasses authorization.
  Cache only successful responses (never error bodies).
- **Honor `?refresh`.** Every cached endpoint parses `?refresh` (or `refresh=`) and
  bypasses the cache so callers can force-run.

## Client plane — `custom_apps_asset_manifest`, `custom_apps_client`, `custom_apps_beacon`

- **Every build ships an asset manifest** at `__oxy/asset-manifest.json`, written
  by publish from the file list + a scan of the bundle's `index.html`. It is a
  build artifact, not a column — it reads through the same LRU, absence cache and
  pre-compression as any asset, so a promote/rollback needs no invalidation. A
  build without one must degrade to "no hints", never to an error.
- **`__oxy/` is a reserved namespace.** Publish strips author files under it
  (`asset_manifest::install_into`) and `serve_pretty` classifies every request
  under it *after* the auth gate, before the source dispatch. A new platform
  endpoint for apps goes **here**, not in `server/router/` — it inherits the
  app's exact gate, works on both the subpath and subdomain surfaces, and needs
  no CORS or route classification. `classify_reserved` is the whole routing
  table: a name nobody claims is `Unknown` → 404, never a fall-through to the
  bundle, and a wrong method is `Unknown` too rather than a 405. Adding a
  platform path means adding a `Reserved` variant — the asset manifest's
  `BuildObject` is the one that deliberately falls through, because it is a real
  object inside the build.
- **HTML carries `Link` preload hints, `x-oxy-build`, and `Cache-Tag`** — on the
  **304** as well as the 200. A 304 has no body to carry `<link>` tags, and
  `x-oxy-build` is how a running service worker notices a publish without being
  replaced. Dropping any of them from the revalidation path is the easy
  regression here.
- **Three surfaces, two bases.** Subpath and custom-app subdomain both address
  assets under `/customer-apps/<org>/<app>/`; the **org subdomain** serves the
  document at `/a/<slug>/` while keeping that asset base, so the worker cannot
  register there and deliberately fails closed (a `/` scope would claim the org
  product origin). Anything computing a scope, a base, or a containment check
  must say which of the three it means — see `customer-apps-performance.md`
  § *The client plane*.
- **`currentBuild` in the worker is a cache, not a fact.** A worker's global
  scope is torn down when it goes idle, so it is `null` on most events;
  `ensureBuildKnown()` recovers it from a **durable marker** that `precacheBuild`
  writes (the `oxy-meta::` cache), falling back to a lone generation and then to
  *unknown*. Any new reader that branches on it must go through that, and any
  destructive branch must treat "unknown" as **do nothing** rather than *do
  everything* — `dropStalePrecaches` swept the live precache exactly once for
  want of that distinction.

  **Do not re-derive it from cache-name order.** That was tried and is wrong:
  two generations is the *normal* state for the whole waiting period (install
  precaches the new build; the old one lives until activate), and
  `CacheStorage.keys()` yields creation order — so "the first name" is reliably
  the *superseded* build, and the sweep keeps the old precache and deletes the
  one install just built. The failure is invisible because it self-heals into
  "the first load after every update is near-cold".
- **The service worker must never `skipWaiting` automatically** — but not for
  the reason that used to be written here. It is *not* about precache deletion
  stranding a tab: lazy chunks live in the build-agnostic `runtime` cache that no
  sweep touches, and `syncBuild` already drops the previous precache with
  old-build tabs open. The reason is that `skipWaiting` + `clients.claim` swaps
  the fetch handler under an already-loaded page, and waiting costs nothing —
  publishes are picked up through the network-first shell and the `x-oxy-build`
  re-precache, never through worker replacement.
- **The worker's cache-first prefix list must equal the origin's `immutable`
  list.** `is_immutable_asset_path` and `cache_control_for` state the same rule
  twice; a test asserts they agree. A new bundler convention goes in both.
- **The HTML transform is memoized per build** (`custom_apps_html_cache`), keyed
  by `(app_id, build_id, object_key, org_slug, app_slug)`. A new field on
  `AppRuntimeConfig` that varies **by viewer** breaks that key — and breaks the
  `private`-because-of-the-cookie reasoning at the same time. Add one only after
  reading both.
- **`oxy-*` event names belong to the platform.** Only `__oxy/beacon` writes
  them, only from `PLATFORM_EVENTS`, and the SDK's `/events` route refuses the
  prefix. Widening the beacon from the allowlist to a prefix check turns it into
  an open-ended write channel into the events table — the allowlist is what
  bounds the blast radius of a route that, by design, makes no second
  authorization decision. Note what this does **not** buy: an app's own JS can
  post allowlisted names to its own beacon, so `oxy-*` means "the platform's
  shape", not "the app could not have written it". Don't document it as
  tamper-proof.
- **Speculative requests are not views.** `is_speculative_request` gates view
  recording *and* the tracking `Set-Cookie`; the launcher prefetches on hover, so
  without it a hover records an open and starts the session early.

## Litmus test before merging

A new serving route under `/customer-apps/**` with no `Cache-Control`/compression, an
uncached per-asset DB lookup, a `Content-Encoding` without `Vary`, HTML that isn't
`private`, or a new cached data endpoint whose key does **not** start with
`project_id`, is read **before** the auth gate, or ignores `?refresh` — should be
challenged through this skill. So should a platform endpoint for apps mounted in
`server/router/` instead of under `__oxy/`, a per-viewer field on
`AppRuntimeConfig`, an automatic `skipWaiting`, or a `Link`/`x-oxy-build` dropped
from the 304 path. This complements `oxy-route-classification` (which
decides *where* the route runs); this skill governs *how fast* it answers.

CDN work (CloudFront in front of the customer-apps hosts) is analysed in
`internal-docs/customer-apps-performance.md` § *CDN* — read the two-step ordering
there before proposing edge caching; the auth gate is the whole difficulty.
