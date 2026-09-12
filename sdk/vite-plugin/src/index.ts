/**
 * @oxy-hq/vite-plugin — one plugin, six behaviors.
 *
 * Drop this into a custom-app's vite.config.ts and the ~50 lines of
 * base-path / outDir / dev-proxy / manifest-copy boilerplate that
 * every app re-implements collapses to:
 *
 *   plugins: [react(), oxyApp()]
 *
 * `pnpm dev` needs NO per-app config file. Identity (org/app) comes from
 * the committed `oxy-app.json`; the projectId is resolved from oxy at
 * startup via the public build-config endpoint. The only machine-level
 * settings are optional env vars (OXY_TARGET defaults to localhost:3000;
 * OXY_TOKEN only if the target oxy requires auth) — set once in your
 * shell, never per app.
 *
 * What it does, in priority order:
 *
 *   1. Forces build.outDir = "out" if unset (warns on override).
 *   2. Resolves Vite `base` automatically:
 *        OXY_APP_BASE_PATH > "/customer-apps/<orgSlug>/<slug>/" > "/"
 *      The middle form is derived from oxy-app.json's identity
 *      fields; missing fields cleanly fall through to "/".
 *   3. Validates oxy-app.json at build start. Fails hard on
 *      schemaVersion != 2, on v1 leakage (products/writers), and on
 *      missing/malformed slug. Error messages link the migration doc.
 *   4. Copies oxy-app.json into out/ at build close. The probe reads
 *      from the served bundle; this stops being a per-app "did you
 *      remember to put it in public/" foot-gun.
 *   5. In dev (buildStart) resolves the app's projectId from oxy's public
 *      build-config endpoint using the manifest's org/app, then injects
 *      `window.__OXY_APP__` into served HTML (transformIndexHtml) so the
 *      SDK has identity in `pnpm dev` with no config file. OXY_* env vars
 *      still override any field for advanced setups.
 *   6. Defaults `server.proxy["/api"]` to OXY_TARGET (or OXY_BASE_URL,
 *      default http://localhost:3000) and, when OXY_TOKEN is set,
 *      attaches it as a bearer on proxied calls (node-side only).
 *
 * Everything else (react plugin, tailwind plugin, custom Vite
 * options) coexists — we mergeConfig rather than overwrite.
 */

import { promises as fs } from "node:fs";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { loadEnv } from "vite";
import type { Plugin, ProxyOptions, ResolvedConfig, UserConfig } from "vite";

const PLUGIN_NAME = "oxy-app";
const DEFAULT_MANIFEST = "oxy-app.json";
const DEFAULT_OUT_DIR = "out";
const DEFAULT_DEV_PROXY_TARGET = "http://localhost:3000";
// The standalone v1 → v2 migration doc was folded into
// internal-docs/customer-apps.md; bundle authors who land here from
// a v1-rejection warning find current SDK + manifest guidance under §5.
const MIGRATION_DOC_URL =
  "https://github.com/oxy-hq/oxygen-internal/blob/main/internal-docs/customer-apps.md";
// Mirrors the server's `is_valid_slug` (admin/apps/ops.rs) and the SDK loader's
// SLUG_RE: lowercase alphanumerics and SINGLE hyphens, no leading/trailing/double
// hyphen, no underscore. The old pattern allowed a trailing/double hyphen and had
// no length bound, so `app-`, `a--b` and a 100-char slug built cleanly and then
// 422'd at `oxy publish` — the "author learns it in CI" gap this gate closes. The
// slug becomes the app's OLTP schema/role name, a repo path segment and the
// served base path.
const SLUG_PATTERN = /^[a-z0-9]+(-[a-z0-9]+)*$/;
const SLUG_MAX_LEN = 63;
// Mirrors `is_valid_function_name` (server/api/custom_apps_publish.rs) — a function key is
// interpolated into `functions/<name>.js`, so `../../x` must be rejected before
// esbuild. Checked here so a bad name fails at `oxy build` too, not only at
// publish.
const FUNCTION_NAME_PATTERN = /^[a-z][a-z0-9-]{0,63}$/;

export interface OxyAppPluginOptions {
  /**
   * Path to the manifest, relative to project root. Default: oxy-app.json.
   * Override if the manifest lives in a non-standard location for
   * some reason — vanishingly rare.
   */
  manifest?: string;

  /**
   * Build output directory. Default: "out". Aligned with the admin
   * probe and customer-apps repo conventions. Override only if you
   * have a strong reason; the plugin will warn loudly on mismatch.
   */
  outDir?: string;

  /**
   * Dev-server proxy target for `/api/*`. Default:
   * `process.env.OXY_BASE_URL || "http://localhost:3000"`. Use when
   * dev-targeting a stubbed backend or remote oxy instance.
   */
  devProxyTarget?: string;

  /**
   * Inject `window.__OXY_APP__` shim into served HTML in dev mode.
   * Default: true. Disable for tests that want to assert against the
   * raw bundle output.
   */
  injectDevShim?: boolean;
}

/**
 * Parsed manifest. We only consume the v2 identity-only shape; any
 * extra fields are ignored at parse time but flagged in
 * `validateManifest`.
 */
interface OxyManifest {
  schemaVersion?: number;
  slug?: string;
  name?: string;
  orgSlug?: string;
  projectId?: string;
  products?: unknown;
  writers?: unknown;
  functions?: unknown;
}

export default function oxyApp(opts: OxyAppPluginOptions = {}): Plugin {
  const manifestPath = opts.manifest ?? DEFAULT_MANIFEST;
  const outDir = opts.outDir ?? DEFAULT_OUT_DIR;
  const injectDevShim = opts.injectDevShim ?? true;

  // State carried between hooks. `manifest` is populated in `config`
  // and consumed by `transformIndexHtml` + `closeBundle`.
  let manifest: OxyManifest | null = null;
  let manifestAbs: string = "";
  let resolvedConfig: ResolvedConfig | null = null;
  // Dev identity for window.__OXY_APP__. org/app come from oxy-app.json
  // (committed) by default; projectId is resolved from oxy at startup
  // (see `buildStart`) so no per-app config file is needed. Any value can
  // still be overridden by an OXY_* env var for advanced/local setups.
  let devIdentity:
    | { orgSlug: string; appSlug: string; projectId: string; branch: string }
    | null = null;
  // Dev oxy URL — defaults to localhost:3000, override with OXY_TARGET.
  // Stored so `buildStart` can resolve the project from oxy.
  let devTarget = DEFAULT_DEV_PROXY_TARGET;

  return {
    name: PLUGIN_NAME,

    // `config` hook runs before everything. We read the manifest,
    // validate it, and contribute Vite config (base, outDir, proxy).
    config(userConfig, env): UserConfig {
      // Anchor manifest path on Vite's resolved project root, not
      // `process.cwd()`. When Vite is invoked from a monorepo root
      // (e.g. `vite -c apps/x/vite.config.ts`), `process.cwd()` is
      // the monorepo root and the manifest lookup would miss the
      // app's own `oxy-app.json`. `userConfig.root` is what `vite`
      // itself uses to resolve everything else in the config.
      const root = userConfig.root ?? process.cwd();
      manifestAbs = path.resolve(root, manifestPath);
      manifest = readManifest(manifestAbs);
      if (env.command === "build" && manifest) {
        const errors = validateManifest(manifest);
        if (errors.length > 0) {
          // Fail the build with a single grouped error message —
          // throwing here aborts Vite cleanly with a stack trace
          // that points at the plugin, not a 200-line YAML dump.
          throw new Error(
            `[${PLUGIN_NAME}] oxy-app.json validation failed:\n  - ${errors.join(
              "\n  - "
            )}\n\nMigration guide: ${MIGRATION_DOC_URL}`
          );
        }
      }

      // Load .env / .env.local (Vite-native) so local dev identity, target,
      // and token live in a gitignored .env.local instead of oxy-app.json.
      // Prefix "" pulls every key, not just VITE_*.
      const loaded = loadEnv(env.mode, root, "");
      const envVar = (k: string): string | undefined => {
        const v = loaded[k] ?? process.env[k];
        return v && v.trim().length > 0 ? v : undefined;
      };

      const base = resolveBase(manifest, envVar("OXY_APP_BASE_PATH"));
      devTarget =
        opts.devProxyTarget ??
        envVar("OXY_TARGET") ??
        envVar("OXY_BASE_URL") ??
        DEFAULT_DEV_PROXY_TARGET;
      const devToken = envVar("OXY_TOKEN");

      // Dev identity comes from oxy-app.json (committed). projectId is
      // optional here — `buildStart` resolves it from oxy when absent — so
      // no per-app .env.local is required. OXY_* env vars still override
      // for advanced/local-retarget setups.
      const orgSlug = envVar("OXY_ORG") ?? manifest?.orgSlug ?? "";
      const appSlug = envVar("OXY_APP") ?? manifest?.slug ?? "";
      const projectId = envVar("OXY_PROJECT") ?? manifest?.projectId ?? "";
      const branch = envVar("OXY_BRANCH") ?? "main";
      devIdentity = appSlug ? { orgSlug, appSlug, projectId, branch } : null;

      // `/api` dev proxy. When OXY_TOKEN is set, attach it as a bearer on
      // proxied requests so a remote dev oxy authenticates them (local dev
      // is cross-origin, so the session cookie isn't sent). The token lives
      // in the Vite node process only — never in browser JS.
      const apiProxy: ProxyOptions = {
        target: devTarget,
        changeOrigin: true
      };
      if (devToken) {
        apiProxy.configure = (proxy) => {
          proxy.on("proxyReq", (proxyReq) => {
            proxyReq.setHeader("authorization", `Bearer ${devToken}`);
          });
        };
      }

      // outDir guard: warn (don't error) if the user set their own
      // and it's not what we want. They might have a reason; we just
      // want them to know they've stepped off the paved road.
      const userOutDir = userConfig.build?.outDir;
      if (userOutDir && userOutDir !== outDir) {
        console.warn(
          `[${PLUGIN_NAME}] build.outDir is set to "${userOutDir}" but the admin probe expects "${outDir}". ` +
            `Bundles built into a non-standard dir need the operator to navigate the picker into that dir manually.`
        );
      }

      // We return a partial config; Vite mergeConfig with userConfig
      // and our return value, with user winning on conflict for keys
      // where we set defaults (so user's `base` overrides ours).
      // For `server.proxy["/api"]` we only set when absent — Vite's
      // merge does the right thing here.
      // Proxy table: the Oxy data plane (`/api`) plus this app's server-side
      // function calls (`/fn`). The `/fn` key is scoped to THIS app's base so it
      // never shadows the bundle's own assets served locally under
      // `/customer-apps/<org>/<slug>/`. Point the target at `oxyc proxy` (default
      // localhost:3000) to hit a cloud env's data with your `oxyc login` token.
      const proxy: Record<string, ProxyOptions> = { "/api": apiProxy };
      if (orgSlug && appSlug) {
        proxy[`/customer-apps/${orgSlug}/${appSlug}/fn`] = apiProxy;
      }

      const contribution: UserConfig = {
        base,
        build: {
          outDir: userOutDir ?? outDir,
          // emptyOutDir defaults to true when outDir is inside root,
          // and false otherwise. Force true so stale builds don't
          // leak past a slug rename.
          emptyOutDir: true
        },
        server: {
          proxy
        }
      };
      return contribution;
    },

    configResolved(config) {
      resolvedConfig = config;
    },

    // Dev-only: resolve the app's projectId from oxy when it isn't already
    // known, using the PUBLIC (no-auth) build-config endpoint keyed by the
    // org/app slugs from oxy-app.json. This is what lets dev run with NO
    // per-app config file — the committed manifest + this lookup supply
    // everything window.__OXY_APP__ needs. Best-effort: a failure just
    // leaves projectId empty and the shim is skipped (with a warning).
    async buildStart() {
      if (resolvedConfig?.command !== "serve" || !injectDevShim) return;
      if (!devIdentity || devIdentity.projectId || !devIdentity.appSlug) return;
      const url =
        `${devTarget.replace(/\/$/, "")}/api/apps/` +
        `${encodeURIComponent(devIdentity.orgSlug)}/${encodeURIComponent(devIdentity.appSlug)}/build-config`;
      try {
        const res = await fetch(url);
        if (res.ok) {
          const data = (await res.json()) as { project_id?: string; branch?: string };
          if (data.project_id) {
            devIdentity = {
              ...devIdentity,
              projectId: data.project_id,
              branch: data.branch || devIdentity.branch
            };
            console.log(
              `[${PLUGIN_NAME}] resolved project ${data.project_id} for ${devIdentity.orgSlug}/${devIdentity.appSlug}.`
            );
          }
        } else {
          console.warn(
            `[${PLUGIN_NAME}] couldn't resolve project for ${devIdentity.orgSlug}/${devIdentity.appSlug} ` +
              `from ${devTarget} (${res.status}). Register the app in oxy (or set OXY_PROJECT) so dev can query.`
          );
        }
      } catch (err) {
        console.warn(
          `[${PLUGIN_NAME}] project resolve failed (${url}): ${String(err)}. Is OXY_TARGET reachable?`
        );
      }
    },

    // Dev-only: inject window.__OXY_APP__ identity so the SDK can call
    // oxy in `pnpm dev`. Data calls are authorized by the dev token the
    // proxy attaches (above) or, when served by oxy, the session cookie.
    transformIndexHtml: {
      order: "pre",
      handler(html) {
        if (!injectDevShim || !resolvedConfig || resolvedConfig.command !== "serve") {
          return html;
        }
        // org/app from oxy-app.json; projectId resolved from oxy in
        // `buildStart`. Without a projectId the SDK can't make /query
        // calls, so skip injection entirely.
        if (!devIdentity || !devIdentity.projectId) return html;
        // Escape `</script>` inside the stringified payload before
        // splicing it into a `<script>` block. JSON.stringify is not
        // HTML-aware — a field containing `</script>` (or `</SCRIPT>`,
        // etc.) would close the inline script tag early and dump the
        // rest of the payload into the HTML stream. Low probability
        // (these are operator-controlled slugs) but the fix is one
        // regex; mirrors `custom_apps_serve::inject_app_config`.
        const payload = JSON.stringify({
          orgSlug: devIdentity.orgSlug,
          appSlug: devIdentity.appSlug,
          projectId: devIdentity.projectId,
          branch: devIdentity.branch
        }).replace(/<\/(script)/gi, "<\\/$1");
        const tag = `<script>window.__OXY_APP__ = ${payload};</script>`;
        // Splice into <head> before any other script so it's set
        // before the bundle's entry runs.
        return html.replace(/<head>/i, `<head>\n    ${tag}`);
      }
    },

    // After Vite finishes writing out/, copy oxy-app.json next to
    // index.html. The admin probe reads from the served bundle, not
    // from the source tree.
    async closeBundle() {
      if (!resolvedConfig || resolvedConfig.command !== "build") return;
      if (!existsSync(manifestAbs)) return;
      const dest = path.resolve(resolvedConfig.root, resolvedConfig.build.outDir, DEFAULT_MANIFEST);
      try {
        await fs.copyFile(manifestAbs, dest);
      } catch (err) {
        // Don't fail the build — log and continue. A missing
        // manifest in out/ produces a soft probe warning, not a 500.
        console.warn(`[${PLUGIN_NAME}] failed to copy ${manifestPath} into ${dest}: ${String(err)}`);
      }
    }
  };
}

/**
 * Read + parse the manifest. Returns null on any failure (missing
 * file, bad JSON) — the caller decides whether that's fatal.
 */
function readManifest(absPath: string): OxyManifest | null {
  try {
    const bytes = readFileSync(absPath, "utf-8");
    return JSON.parse(bytes) as OxyManifest;
  } catch {
    return null;
  }
}

/**
 * Validate a parsed manifest against the v2 identity-only contract.
 * Returns a list of human-readable error strings; empty list means
 * the manifest is valid.
 */
export function validateManifest(m: OxyManifest): string[] {
  const errors: string[] = [];
  if (m.schemaVersion !== 2) {
    errors.push(
      `schemaVersion is ${m.schemaVersion ?? "(missing)"}; v2 is required. ` +
        `Set "schemaVersion": 2.`
    );
  }
  if (m.products !== undefined || m.writers !== undefined) {
    errors.push(
      "manifest contains v1 fields (products and/or writers). " +
        "The MVP refactor requires identity-only manifests."
    );
  }
  if (!m.slug || typeof m.slug !== "string") {
    errors.push("slug is required.");
  } else if (m.slug.length > SLUG_MAX_LEN || !SLUG_PATTERN.test(m.slug)) {
    errors.push(
      `slug "${m.slug}" is malformed; must be 1–${SLUG_MAX_LEN} lowercase letters, ` +
        `digits and single hyphens (no leading/trailing/double hyphen, no underscore). ` +
        `It becomes the app's OLTP schema name and served path, which oxyc publish rejects otherwise.`
    );
  }
  // Function keys become `functions/<name>.js`, so a name like `../../x` is a
  // path traversal — reject at build, the same shape `oxyc publish` enforces.
  if (m.functions !== undefined) {
    // `typeof [] === "object"`, so an array would otherwise slip through with no
    // keys to check — reject it explicitly (a manifest `functions` is a map).
    if (
      typeof m.functions !== "object" ||
      m.functions === null ||
      Array.isArray(m.functions)
    ) {
      errors.push("functions must be an object keyed by function name.");
    } else {
      for (const name of Object.keys(m.functions as Record<string, unknown>)) {
        if (!FUNCTION_NAME_PATTERN.test(name)) {
          errors.push(
            `function name "${name}" is malformed; must match ${FUNCTION_NAME_PATTERN.source}.`
          );
        }
      }
    }
  }
  return errors;
}

/**
 * Resolve Vite's `base` option. Priority:
 *   1. OXY_APP_BASE_PATH env (CI always sets this — keep working).
 *   2. Derived from manifest's orgSlug + slug.
 *   3. "/" (dev default, root-mounted).
 *
 * Always returns a path with a trailing slash — Vite requires that.
 */
export function resolveBase(
  manifest: OxyManifest | null,
  envBaseOverride?: string
): string {
  const envBase = envBaseOverride ?? process.env.OXY_APP_BASE_PATH;
  if (envBase && envBase.trim().length > 0) {
    return envBase.endsWith("/") ? envBase : `${envBase}/`;
  }
  if (manifest?.orgSlug && manifest?.slug) {
    return `/customer-apps/${manifest.orgSlug}/${manifest.slug}/`;
  }
  return "/";
}
