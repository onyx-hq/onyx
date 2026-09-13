# {{APP_DISPLAY_NAME}}

Customer-app bundle scaffolded by `pnpm dlx create-oxy-app`. Vite + React +
`@oxy-hq/sdk`, pre-wired with `@oxy-hq/vite-plugin`.

## Quick start

```bash
pnpm install
pnpm dev        # local dev on :5173, proxies /api → oxy on :3000
```

`oxyApp()` (in `vite.config.ts`) resolves the base path, validates + copies
`oxy-app.json`, proxies `/api`, and injects `window.__OXY_APP__` in dev. If the
app isn't registered in your local oxy yet, register it (admin UI) or set
`OXY_PROJECT=<uuid>`; data calls need `OXY_TOKEN` in cross-origin dev. See
`internal-docs/customer-apps.md` §3 for the full local loop.

## Editing

- **`oxy-app.json`** (project root, next to `vite.config.ts` — **not** under
  `public/`) — the bundle's identity + launcher-card metadata: `slug`, `name`,
  optional `description`, `art`, `icon`, `status`, `ask`. The plugin copies it
  into `out/` at build.
- **`src/App.tsx`** — your UI. Replace the scaffold with whatever you want to
  render.

## Launcher-card image

The Oxy HQ home shows each app as a card. The image is the manifest `art`
field — a **relative** path served from the bundle root. To capture it:

```bash
pnpm run screenshot     # boots dev, screenshots → public/card.jpg (1280×640)
```

Then set `"art": "card.jpg"` in `oxy-app.json` and republish. `public/card.jpg`
is copied to the bundle root, so it serves at
`/customer-apps/<org>/<slug>/card.jpg` — keep `art` relative; never hardcode the
base path. Keep the image under 200 KB (`oxyc publish` warns above it): the home
page downloads it on every visit. `pnpm run screenshot -- --help` lists flags (`--wait`, `--selector`,
`--url`, `--settle`). Playwright is installed on demand (the script prints the
one-liner); it is not a default dependency.

## Deploying

```bash
npm install -g @oxy-hq/cli       # once; installs oxyc
oxyc login   --env production    # once per env; caches a token
oxyc publish --env production    # build + ship to the draft channel
oxyc publish --env production --promote   # …straight to live
```

`oxyc publish` reads `oxy-app.json`, runs the build (`pnpm install` → `pnpm
build` → `out/` by default), resolves the target + project, and uploads the
bundle. No CI, no project id in git. See `internal-docs/customer-apps.md` §5.
