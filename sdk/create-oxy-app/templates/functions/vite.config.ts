import oxyApp from "@oxy-hq/vite-plugin";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// `oxyApp()` handles everything Oxy-specific: it validates `oxy-app.json`,
// copies it into the build output, proxies `/api` to your local oxy server in
// dev, and resolves the base path the bundle is served under.
//
// The base path resolves from (in order):
//   1. `OXY_APP_BASE_PATH` — `oxyc publish` sets this from the org it resolves
//      for the target environment, so the org is never hardcoded here.
//   2. `/` — the dev fallback, fine for `pnpm dev` and a standalone build.
//
// For a standalone build against a specific org:
//   OXY_APP_BASE_PATH=/customer-apps/<org>/{{APP_SLUG}}/ pnpm build
export default defineConfig({
  plugins: [react(), tailwindcss(), oxyApp()],
  build: { outDir: "out", emptyOutDir: true }
});
