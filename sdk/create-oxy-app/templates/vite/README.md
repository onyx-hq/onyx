# {{APP_DISPLAY_NAME}}

An Oxy **custom app** — a Vite + React bundle that Oxy serves inside your
workspace and that queries your project through `@oxy-hq/sdk`. Scaffolded by
`create-oxy-app`.

## Quick start

```bash
pnpm install
pnpm dev        # http://localhost:5173 — proxies /api → your local oxy server
```

It renders on first run with nothing else set up: the starter query is a
literal `SELECT`, so you get a working app before you point it at real data.

## Layout

| Path | What it is |
| --- | --- |
| `oxy-app.json` | The app's identity + launcher-card metadata: `slug`, `name`, `icon`, `art`. Lives at the project root (**not** under `public/`). |
| `src/App.tsx` | Your UI, and the two data surfaces (`useQuery`, `useSemanticQuery`). Start here. |
| `src/index.css` | Design tokens. Change the hex values to re-skin the whole app. |
| `src/chrome/` | Small presentational primitives — `Panel`, `KpiTile`, `Pill`, `Topbar`. |
| `vite.config.ts` | Wires `oxyApp()`, which validates + copies the manifest, proxies `/api`, and resolves the served base path. |
| `public/` | Static assets copied to the bundle root — `icon.svg` and `card.svg`. |

## Querying your data

`useQuery` runs raw SQL against the project's warehouse:

```tsx
const { rows, loading, error } = useQuery({ sql: "SELECT * FROM orders LIMIT 10" });
```

`useSemanticQuery` reads measures + dimensions from a semantic topic, so the
SQL stays in your `.view.yml` / `.topic.yml` files instead of this bundle:

```tsx
const { rows } = useSemanticQuery({
  topic: "store_performance",
  dimensions: ["store_performance.store_id"],
  measures: ["store_performance.total_sales"],
  limit: 5
});
```

Auth rides the session cookie — Oxy only serves this bundle after a membership
check, so there is no token to manage in the frontend.

Parameters interpolate through Jinja, quoted for you:

```tsx
useQuery({ sql: "SELECT * FROM orders WHERE region = {{ params.region | sqlquote }}" },
         { params: { region } });
```

## Launcher card

The HQ home shows each app as a card: `icon` is the small mark, `art` is the
wide preview. Both are **relative** paths in `oxy-app.json`, served from the
bundle root — never hardcode the base path. Replace `public/icon.svg` and
`public/card.svg` with your own (a real screenshot makes a good `art`).

## Deploying

```bash
npm install -g @oxy-hq/cli       # once; installs oxyc
oxyc login   --env production    # once per env; caches a token
oxyc publish --env production    # build + ship to the draft channel
oxyc publish --env production --promote   # …straight to live
```

`oxyc publish` reads `oxy-app.json`, runs the build into `out/`, resolves the
target org + project, and uploads the bundle. No CI required and no project id
in git.

## Server-side functions

Need to run code on the server — hit an external API, send email, keep a secret
out of the browser? That's an **Oxy Function**. Scaffold a copy of this app with
one already wired up:

```bash
pnpm dlx create-oxy-app my-app --template functions
```
