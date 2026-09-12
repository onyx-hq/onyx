# Oxy Starter — the example custom app

The smallest complete [Custom App](../../../internal-docs/customer-apps.md): a hand-written
`index.html` plus one hashed stylesheet and one hashed module under `assets/` — no build
step, no dependencies, no framework. `oxy seed` deploys it through the real publish path
(`put_build` → `app_builds` → channel pointers), so it exercises the same serve code an
`oxyc publish` bundle does — and a fresh clone lands on a launcher with a working app on it.

```sh
cargo run -p oxy-server -- seed
cargo run -p oxy-server -- compile --workspace-path ./examples
OXY_ROLE=ide cargo run -p oxy-server -- serve --enterprise
# → /customer-apps/local/oxy-starter/            overview
#   /customer-apps/local/oxy-starter/stores?limit=10
#   /customer-apps/local/oxy-starter/stores/20
#   /customer-apps/local/oxy-starter/identity
```

## The five patterns it shows

1. **Scalar aggregate** — `POST /api/projects/<projectId>/query` for one row of KPIs.
2. **List query** — the same endpoint for a leaderboard, sized by `?limit=`.
   2b. **Path parameter** — `stores/<n>` for one store. The store number reaches SQL, so it
   is parsed as an integer and refused if it is anything else. A URL is user input on every
   screen; only here is it user input a database will read.
3. **Runtime identity** — `window.__OXY_APP__`, spliced in before `</head>` at serve time.
   A bundle never hardcodes its org or project; it reads them, addresses the data plane
   with `projectId`, and routes off the `basePath` Oxy mounts it under. The session cookie
   that authorized the bundle also authorizes the query, so there's no second auth step.
4. **Viewer identity** — `GET /api/projects/<projectId>/shell-context` for who is looking:
   name, email, org, workspace. This is **display** identity, and it deliberately carries
   **no role** — shipping one to the browser invites gating on it, and a bundle is
   JavaScript the viewer can edit.
5. **History** — four screens, each a real URL, navigated with `history.pushState`. Back
   and Forward walk them, a reload lands on the screen you were on, and a link to one opens
   where you meant. See *Two halves of a URL* below.

   The **verified** half — `appRole`, `orgRole`, `teams`, `kind` — lives on `ctx.user`
   inside an Oxy Function, assembled server-side from the authenticated session where the
   caller cannot reach it. Only `appRole` is a gate; `orgRole` and `teams` are facts to
   explain or lay out with. That needs an Oxy Function (server-side, esbuild-bundled), so it
   is the one thing this function-less app can't demonstrate — `examples/hello-oxy` in the
   customer-apps repo shows both halves side by side.

Both queries hit `oxymart` in the `local` DuckDB database from `examples/config.yml` — the
same demo table the example dashboards use. No credentials, so it works offline.

## What Oxy adds for free (no bundle code)

Beyond serving the files and injecting identity, the serve path gives every custom app the
fast-load path — automatic, on by default, and something this app is now split to actually
demonstrate:

- **Preload hints.** At publish, Oxy parses this `index.html`, sees the two `assets/` tags,
  and stamps `Link: rel=preload; as=style` + `rel=modulepreload` on the shell — so the
  browser pulls `app-*.css` and `app-*.js` while the HTML is still in flight. Open DevTools
  → Network on a cold load and they are already in flight before the parser reaches the tags.
- **A service worker** (`__oxy/sw.js`) that precaches those two files and serves anything
  under `assets/` cache-first, so a repeat load is near-instant and works offline.
- **Usage instrumentation** — SPA pageviews, Core Web Vitals, engagement time, and
  uncaught-error counts, posted to `__oxy/beacon` by the injected runtime and shown on the
  app's Activity tab. No `useTrackEvent` call required.

Both the worker and the instrumentation are opt-out per build in `oxy-app.json`
(`performance.serviceWorker: false`, `analytics: false`); this app opts out of neither. The
full serve/client-plane design is `internal-docs/customer-apps-performance.md` →
*The client plane*.

### Why the filenames carry a hash

`assets/app-<hash>.css` / `.js` — the hash is the content digest, and it is what lets the
serve path cache these `immutable` (one year) safely: a changed file is a changed URL, so a
returning visitor can never be pinned to stale bytes. **If you edit either asset, recompute
its hash and update the reference in `index.html`** — here it is by hand, the one chore the
inline version didn't have. Leave the hash stale and returning visitors keep the old file
for up to a year.

This chore is unique to this seeded, build-step-free example. A real app publishes with
`oxyc publish`, which runs the build command declared in its `oxy-app.json`
(`build.command`, default `pnpm build`) and uploads the result — so the re-hashing is a
property of **the bundler you configure**, not of `oxyc publish` itself. Any bundler that
content-addresses its output filenames regenerates the hash on every build, and the
`create-oxy-app` Vite template does so by default. That is the property that keeps a
publisher safe, and nothing on the serve side enforces it: `cache_control_for` applies the
year-long `immutable` to everything under `assets/` by *prefix*, so a build emitting an
un-hashed `assets/app.js` gets it too, with no warning. Stay on a content-hashing bundler
(the default) and there is no chore; swap `build.command` for one that doesn't hash and the
footgun is back. (A `--dir <prebuilt>` publish skips the build entirely; the hashing then
happened wherever that directory was produced.)

## Two halves of a URL, and why they carry different things

Both halves hold state, and the split is not cosmetic:

- **The path picks the screen** (`/stores`, `/stores/20`). The injected client runtime
  patches `pushState`/`replaceState` and reports an **SPA pageview per path**, so these
  show up individually on the app's Activity tab.
- **The query carries state within a screen** (`?limit=10`). Query strings are
  deliberately never recorded — `currentPath()` in `custom_apps_client/runtime.js` reads
  `location.pathname` alone — so this half is linkable but private. That is exactly why an
  app puts "which month, which vendor" here and not in the path.

A consequence worth knowing before you go looking for it: changing only the query fires no
pageview, by design. Two screens that differ only by `?limit=` are one row on the Activity
tab, and that is the privacy property working, not instrumentation missing.

Nothing is stored in a variable the URL does not also say. Every screen rebuilds itself
from `location` on `popstate`, which is what makes Back correct rather than approximately
correct — and it is why the app can be driven from the address bar in a test.

`basePath` is the prefix the app is mounted under, and it is the **same string on the
subpath and subdomain surfaces**, so routing off it needs no per-surface branch. Deep links
survive a cold load because the serve path falls back to the bundle's `index.html` for any
navigation request that misses (`allow_spa_fallback`, scoped to `Accept: text/html`), and
the service worker's navigation handler is network-first with that same shell cached.

## Why it's hand-written

A real app uses `create-oxy-app` (`sdk/create-oxy-app/templates/`) and ships a Vite bundle,
which content-hashes assets and stamps the base path for you. This one stays dependency-free
and does the same two things by hand — a split, hashed bundle is the shape the fast-load path
rewards, shown with nothing but a text editor.
It stays toolchain-free on purpose: the seed must work on a fresh clone with no Node
installed, and an example you can read top-to-bottom in one sitting teaches the contract
better than a build pipeline does. Splitting the assets by hand keeps that property.

For the full worked example — semantic queries, Jinja params, Oxy Functions, scheduled
jobs — see `examples/hello-oxy` in the [customer-apps](https://github.com/oxy-hq/customer-apps)
repo. This is the floor; that's the ceiling.

## Editing it

Edit and re-run `oxy seed`. The build id is a content hash, so changed bytes become a new
build and the bundle cache can't serve a stale page. **If you touch `assets/app-*.css` or
`assets/app-*.js`, rename it to its new content hash and update the `<link>`/`<script>` in
`index.html`** — see *Why the filenames carry a hash* above; a stale hash pins returning
visitors to old bytes under the year-long `immutable` cache. `cargo nextest run -p oxy-app
seed_apps` holds the bundle to the contracts the serve path enforces.
