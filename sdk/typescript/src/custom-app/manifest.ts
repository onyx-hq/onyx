// Manifest loader for custom-app bundles served by oxy at
// `app.oxygen-hq.com/customer-apps/<org_slug>/<app_slug>/`.
//
// The bundle commits a `public/oxy-app.json` declaring its identity
// (slug, orgSlug, projectId). This module:
//   1. Fetches that manifest at startup (cached after the first call).
//   2. Validates the schema with clear errors (v2 only — v1 is rejected).
//   3. Joins it with the runtime identity oxy injects via
//      `<script>window.__OXY_APP__=...</script>`.
//
// Bundles call `useQuery` directly for data access — there are no
// `products` or `writers` declarations in v2 manifests.

import { type OxyInjectedAppConfig, readInjectedAppConfig } from "./inject";
import { getOxyAppLogger } from "./logger";

// ── Manifest types ──────────────────────────────────────────────────────────

/**
 * Declaration of a single Oxy Function shipped in the bundle's
 * `functions/` dir. See `internal-docs/customer-apps-functions.md`.
 *
 * All fields optional except that at least one invocation surface
 * (`route`, `schedule`, or `airwayStep`) must be active. Absent =
 * `route: true` (HTTP-invocable via `useFunction`).
 */
export interface OxyAppFunctionManifest {
  /** Source entry, relative to the app dir. Default: `functions/<name>.ts`. */
  entry?: string;
  /** Cron expression. When set, the function fires on this schedule. */
  schedule?: string;
  /** IANA timezone for `schedule`. Default: `UTC`. */
  timezone?: string;
  /** Expose `POST .../fn/<name>` (called via `useFunction`). Default: true. */
  route?: boolean;
  /** Wire the function in as an Airway pipeline transform step. */
  airwayStep?: { pipeline: string; resource: string };
  /**
   * Receive an unauthenticated POST from a third party at
   * `POST /api/webhooks/apps/<org>/<app>/<name>`.
   *
   * The PLATFORM verifies every request — HMAC-SHA256 over the raw body, in
   * constant time — before the function is enqueued, so app code never sees an
   * unverified request. Omit the block and that endpoint answers 404, as if it
   * did not exist.
   */
  webhook?: {
    /**
     * App-secret key holding the signing key(s) — the same `apps/<app-id>/`
     * namespace `ctx.env` reads. The manifest names the secret, never holds it.
     *
     * **Comma-separated for rotation.** Providers that keep two live signing
     * keys (Uber's `BASIC_HMAC` does) sign with either during a rotation; any
     * match passes, so adopting a new key does not drop the events still
     * signed with the old one.
     */
    secretVar: string;
    /** Header carrying the signature, e.g. `x-uber-signature`. */
    signatureHeader: string;
    /** How the digest is encoded. Default `hex`. */
    encoding?: "hex" | "base64";
  };
  /** Wall-clock timeout. Default 30, max 300. */
  timeoutSeconds?: number;
  /**
   * Opt-in result caching for route invocations. Omit (the default) to never
   * cache — the safe choice for a side-effectful function (writes, external
   * POSTs, ELT). Set `ttlSeconds` ONLY for read-only / idempotent functions:
   * results are then cached per (build, function, user, request body) for that
   * window, and a repeat `useFunction().invoke(sameBody)` returns the cached
   * result without re-running. A `?refresh` query bypasses it.
   */
  cache?: { ttlSeconds?: number };
  /**
   * Databases this function's `ctx.warehouse.*` writes may target. Omit (or
   * leave empty) and the function may NOT write to any database — writes are
   * fail-closed and rejected before any connection is opened. Declare a
   * destination here ONLY for a function that legitimately writes to it; a
   * read-only function omits it. This scopes writes away from the project's
   * source warehouse.
   */
  destinations?: string[];
  /**
   * Capability to write app-scoped secrets via `ctx.secrets.set` (fail-closed:
   * omit → writes rejected). Only the app's own `apps/<app-id>/` namespace is
   * writable. Declare for a function that persists state — e.g. a scheduled
   * token-refresher that writes the rotated token back to Oxy Secrets.
   */
  secrets?: { write?: boolean };
  /**
   * Capability to send email via `ctx.email.send` (fail-closed: omit → the
   * host rejects `ctx.email.send` before any provider call). Declare for a
   * function that emails the app's users — e.g. a `notify` route that sends a
   * welcome message, or a scheduled digest. The sender mailbox is
   * platform-controlled; a function may set `replyTo` but never `from`.
   */
  email?: { send?: boolean };
  /**
   * Capability for `ctx.org.people()` — the org's people directory, READ-ONLY
   * (fail-closed: omit → the call is rejected before any query).
   *
   * Declare it for a function that has to name a person: an assignee, a roster
   * entry, who submitted something. It answers with a display name and a role.
   *
   * Three things it deliberately is not, so nobody plans around them:
   * it returns **no email and no phone** — naming a colleague is a different
   * need from contacting them off-platform; it returns **no location**, which
   * the platform does not hold for a member; and it does **not include
   * frontline workers**, who hold no org-membership row by design.
   *
   * One flag, not `read`/`write`: there is no write. Editing the directory
   * would put tenant membership behind an app's manifest.
   */
  org?: { read?: boolean };
  /**
   * Capability for `ctx.oltp` — read/write the app's OWN per-org OLTP schema on
   * the managed Postgres tenant (fail-closed: omit → every `ctx.oltp` call
   * rejected). A pure GATE: the target schema is derived from the app's own slug
   * host-side (`oltp-bookings` → `app_oltp_bookings`), never named here, so a
   * manifest cannot point `ctx.oltp` at another app's schema. The resolved role
   * has DML rights on that one schema and nothing else — reaching neither another
   * app's data nor the analyst-visible `raw_*` schemas. The store must be
   * provisioned first (ask whoever operates the org).
   *
   * NOTE: this shape is `{ enabled }`, not the earlier `{ writer }` — an app
   * had no business naming its own writer (that was the cross-app hole). A
   * manifest still carrying `"oltp": { "writer": "…" }` deserializes to
   * `enabled: undefined` → **disabled**, and `ctx.oltp` then reports the
   * capability as missing. Switch it to `{ "enabled": true }`.
   */
  oltp?: { enabled?: boolean };
  /**
   * Retry policy for **background** runs (a `schedule` fire or a manual job
   * trigger). Omit → a job run is attempted once. Route (HTTP) invocations are
   * request-scoped and never retried. `maxAttempts` counts the first try
   * (`maxAttempts: 3` = up to 2 retries); backoff is exponential (doubling)
   * between `minTimeoutMs` and `maxTimeoutMs`. Maps to the durable queue's
   * retry policy — a transient failure re-runs the whole isolate.
   */
  retries?: { maxAttempts?: number; minTimeoutMs?: number; maxTimeoutMs?: number };
  /**
   * Example input params for the function — a sample JSON body the admin "Run
   * now" surface prefills so an operator knows what to pass (the function reads
   * it as its `req` body, same as a route invocation). Advisory only; not
   * enforced at runtime.
   */
  inputExample?: unknown;
}

/** Wire shape of `oxy-app.json` (v2 only). */
export interface OxyAppManifest {
  /** Must be 2. v1 manifests are no longer supported. */
  schemaVersion: 2;
  /**
   * Optional display name. The admin "Link existing" dialog prefills
   * its Name field from this. Omit to let oxy fall back to the
   * folder basename.
   */
  name?: string;
  /**
   * URL slug. **Required.** The canonical source of truth — the
   * dialog locks the slug field to this value, and
   * `OXY_APP_BASE_PATH=/customer-apps/<org>/<slug>/` baked into the
   * build must match.
   */
  slug: string;
  /**
   * Optional org slug. Prefills the dialog's org picker; operator
   * can still override. Carries no security weight — the actual
   * access check is on the linked row.
   */
  orgSlug?: string;
  /**
   * Optional project (workspace) uuid the bundle expects to read
   * from. Used by `useQuery` to construct the
   * `/api/projects/:id/query` URL.
   */
  projectId?: string;
  /**
   * Optional map of Oxy Functions (server-side handlers) shipped in the
   * bundle's `functions/` dir, keyed by function name. Omit for a pure
   * static bundle (today's default). See the functions design doc.
   */
  functions?: Record<string, OxyAppFunctionManifest>;
  /**
   * Schema migrations that ship WITH this bundle and run on promote.
   *
   * `dir` is a directory inside the built bundle holding numbered `.sql` files.
   * The platform runs them in lexical order, **once each, ever**, inside a
   * transaction, as the app's own writer role, and records each one.
   *
   * What changes for the author: you no longer write defensive
   * `IF NOT EXISTS` / idempotent upserts, because re-running is a no-op by
   * construction rather than by your care. And you **may not edit, rename or
   * copy a migration that has already run** — all three fail the promote by
   * name, and the fix is always a new file.
   *
   * The `.sql` files are ordinary bundle files, fetchable over the app's own
   * host: put no secrets in them.
   */
  migrations?: { dir: string };
  /**
   * Optional Ask Oxygen binding (agent ref + composer chips). The
   * platform's registered copy is authoritative (surfaced by
   * shell-context); this local copy is the dev-time fallback so the
   * shell's Ask dock works before the app is registered.
   */
  ask?: { agent?: string; suggestedQuestions?: string[] };
  /**
   * The secrets this app expects, keyed by env-var name — the app's
   * `.env.example`, declared rather than written in a README.
   *
   * These are the keys your functions read as `ctx.env.KEY`. Declaring one puts
   * it in the app's Secrets surface (staff console → app → Secrets, and the
   * workspace's own settings) as a row to fill in, so a fresh deploy says what
   * is still missing instead of failing at the first invocation. Every function's
   * `webhook.secretVar` is folded in automatically — no need to repeat it here.
   *
   * **Names only, never values.** A manifest ships inside the bundle and is
   * fetchable over the app's own host; putting a secret in one publishes it.
   * Values are set out-of-band on the Secrets surface, or by
   * `ctx.secrets.set` from a function holding the `secrets.write` capability.
   *
   * App-level rather than per-function, because a secret is app-scoped by
   * construction: two functions sharing `STRIPE_API_KEY` read the same value,
   * so it can only be described once.
   *
   * ```jsonc
   * "env": {
   *   "STRIPE_API_KEY":    { "required": true, "description": "Restricted key, Dashboard → Developers" },
   *   "SLACK_WEBHOOK_URL": { "description": "Optional ops channel" }
   * }
   * ```
   *
   * Read by the platform at **publish time** (like {@link OxyAppStorageManifest}),
   * so the block is documented here but not round-tripped through the dev-time
   * manifest fetch.
   */
  env?: Record<string, OxyAppEnvDeclaration>;
  /**
   * Optional app-level storage policy. Distinct from the per-function
   * `storage: { read, write }` capability: those gate what one function may
   * call, while this governs the app's whole asset silo, which every function
   * shares.
   */
  storage?: OxyAppStorageManifest;
  /**
   * Browser-runtime performance opt-outs. Both features below are **on by
   * default** — an app that says nothing gets them — so this block exists only
   * to turn one off.
   *
   * Read by the platform at **publish time** (like {@link OxyAppStorageManifest})
   * rather than by this loader, so the field is documented here but not
   * round-tripped through the dev-time manifest fetch.
   */
  performance?: OxyAppPerformanceManifest;
  /**
   * Opt out of the platform's automatic, zero-config usage instrumentation:
   * SPA pageviews, Core Web Vitals, engagement time, and uncaught-error counts,
   * posted to `<base>/__oxy/beacon` by the runtime Oxy injects into every served
   * page. `false` silences the **client** runtime only — the server still
   * records one view row per HTML navigation (that floor is not opt-out-able),
   * so the Activity tab never goes dark, it just loses the in-page detail.
   *
   * Distinct from `useTrackEvent` (your own named events): those are additive and
   * always on. This governs only the events the platform sends on your behalf.
   *
   * Honored at publish time (see {@link performance} for why it is not
   * round-tripped here). Default: `true`.
   */
  analytics?: boolean;
}

/** One declared secret — an entry in the `env` block of `oxy-app.json`. */
export interface OxyAppEnvDeclaration {
  /**
   * Flag the key as **Missing** (rather than merely absent) while nothing is
   * stored for it, and count it in the app's missing-secrets badge.
   *
   * Advisory, not a gate: a publish is never blocked on an unset key, because
   * the first publish is exactly when nobody could have set one yet.
   *
   * Default: `false` — a declaration is documentation first.
   */
  required?: boolean;
  /**
   * Shown beside the key on the Secrets surface. Say what it is and where to
   * get one — this is the text that saves someone a Slack message.
   */
  description?: string;
}

/** Browser-runtime performance opt-outs — the `performance` block in `oxy-app.json`. */
export interface OxyAppPerformanceManifest {
  /**
   * Opt out of the platform service worker Oxy registers at `<base>/__oxy/sw.js`.
   * It precaches your build's entry assets and serves content-hashed files
   * cache-first, so a repeat load of a published app is near-instant.
   *
   * Set `false` only if your app ships its own service worker (two workers
   * cannot both control the same scope) or genuinely must never be cached.
   * There is nothing to configure to opt *in* — a normal build is precached
   * automatically, and a bundle that inlines everything into one HTML file
   * simply has nothing to precache, which is fine. Default: `true`.
   */
  serviceWorker?: boolean;
}

/** How long assets under a given prefix are kept. */
export interface OxyAppRetentionRule {
  /**
   * Key prefix inside your silo, as you write it — `"tmp/"`, `"generated/"`.
   * The `customer-app-storage/<app_id>/` part is implicit.
   */
  prefix: string;
  /**
   * One of the five supported classes. `null` (or omitted) pins the prefix to
   * "keep forever", which is how you protect it from a broader sibling rule.
   *
   * The set is closed on purpose — each class is one bucket-wide S3 lifecycle
   * rule, so an arbitrary duration can't be honoured. An unrecognized value is
   * ignored with a warning and the prefix simply doesn't expire.
   */
  expireAfter?: "1d" | "7d" | "30d" | "90d" | "365d" | null;
}

/** App-level `storage` block in `oxy-app.json`. */
export interface OxyAppStorageManifest {
  /**
   * Retention rules for the asset silo. **Longest matching prefix wins**; a key
   * matching no rule is kept forever.
   *
   * Expiry is enforced by S3 lifecycle rules on an object tag, so it is
   * approximate (evaluated daily, not on the hour) and applies from the time an
   * object was written. Editing a rule does not retag assets already stored —
   * new writes pick up the new class.
   *
   * ```jsonc
   * "storage": {
   *   "retention": [
   *     { "prefix": "tmp/",       "expireAfter": "1d"  },
   *     { "prefix": "generated/", "expireAfter": "90d" },
   *     { "prefix": "uploads/",   "expireAfter": null  }  // keep forever
   *   ]
   * }
   * ```
   */
  retention?: OxyAppRetentionRule[];
}

// ── Resolved manifest ───────────────────────────────────────────────────────

/**
 * Manifest + runtime-injected identity needed to call oxy. Callers
 * should treat this as the only source of truth for "which org/app
 * does this bundle belong to."
 */
export interface ResolvedCustomAppManifest {
  manifest: OxyAppManifest;
  /**
   * Always an empty array for v2 manifests. Kept for API compatibility;
   * callers that previously iterated product names should switch to
   * explicit `useQuery` calls.
   * @deprecated Will be removed in a future version.
   */
  productNames: string[];
  /** Org slug injected by oxy. */
  orgSlug: string;
  /** App slug injected by oxy. */
  appSlug: string;
  /**
   * The oxy server's API base URL. Empty string when oxy serves the
   * bundle itself (same-origin, the common case); a full URL only
   * when the bundle is running under a dev server proxy.
   */
  apiBaseUrl: string;
  /** App UUID; informational. */
  appId?: string;
  /**
   * Project (workspace) UUID. Injection (`window.__OXY_APP__.projectId`)
   * wins over the manifest's `projectId` field — the admin row is
   * authoritative. Manifest `projectId` is a dev-time hint used only
   * when running without a server. Used by `useQuery` to construct the
   * `/api/projects/:id/query` URL.
   */
  projectId?: string;
}

export interface LoadManifestOptions {
  /**
   * Override the URL the manifest is fetched from. Default:
   * `<injected_base>/oxy-app.json` or `/oxy-app.json`.
   * Useful for non-Next bundlers — set explicitly to wherever your
   * bundler emits static assets.
   */
  manifestUrl?: string;
}

let cached: Promise<ResolvedCustomAppManifest> | null = null;

/**
 * Load + validate the manifest. Cached after the first call so callers
 * can invoke this from every component without coordinating.
 */
export function loadCustomAppManifest(
  options: LoadManifestOptions = {}
): Promise<ResolvedCustomAppManifest> {
  if (!cached) {
    cached = fetchAndValidate(options);
  }
  return cached;
}

/** For tests: reset the cache between runs. */
export function _resetCustomAppManifestCacheForTest(): void {
  cached = null;
}

async function fetchAndValidate(options: LoadManifestOptions): Promise<ResolvedCustomAppManifest> {
  const log = getOxyAppLogger();
  const injected = readInjectedAppConfig();
  const manifestUrl = options.manifestUrl ?? defaultManifestUrl(injected);

  log.log("info", "loading manifest", {
    manifestUrl,
    injectionPresent: !!injected,
    orgSlug: injected?.orgSlug,
    appSlug: injected?.slug,
    appId: injected?.appId
  });

  const startedAt = Date.now();
  const res = await fetch(manifestUrl, { credentials: "same-origin" });
  if (!res.ok) {
    log.log("error", "manifest fetch failed", {
      manifestUrl,
      status: res.status,
      statusText: res.statusText
    });
    throw new Error(
      `Failed to load oxy-app.json from ${manifestUrl} (HTTP ${res.status}). ` +
        `The custom-app repo must commit this file alongside the bundle.`
    );
  }
  const raw = (await res.json()) as unknown;
  const manifest = validateManifest(raw, manifestUrl);

  const resolved: ResolvedCustomAppManifest = {
    manifest,
    productNames: [],
    orgSlug: injected?.orgSlug ?? "",
    appSlug: injected?.slug ?? "",
    apiBaseUrl: injected?.apiBaseUrl || "",
    appId: injected?.appId,
    projectId: injected?.projectId ?? manifest.projectId
  };
  log.log("info", "manifest ready", {
    durationMs: Date.now() - startedAt,
    schemaVersion: manifest.schemaVersion,
    slug: manifest.slug
  });
  return resolved;
}

/**
 * Default manifest URL.
 *
 * Resolution order (bundler-agnostic):
 *   1. `window.__OXY_APP__.orgSlug`/`slug` injection → the canonical
 *      `/customer-apps/<org>/<app>/oxy-app.json`. Works for every
 *      bundle oxy serves regardless of how it was built.
 *   2. `NEXT_PUBLIC_APP_BASE_PATH` env var — kept for backward compat
 *      with Next.js bundles that bake basePath at build time.
 *   3. Empty basePath → `/oxy-app.json` (only matches when running in
 *      a `vite dev` / `next dev` root mount; will 404 under oxy).
 */
function defaultManifestUrl(injected: OxyInjectedAppConfig | undefined): string {
  if (injected?.orgSlug && injected?.slug) {
    const org = encodeURIComponent(injected.orgSlug);
    const app = encodeURIComponent(injected.slug);
    return `/customer-apps/${org}/${app}/oxy-app.json`;
  }
  // No injection → bundle is running outside oxy (`pnpm dev` against
  // a local Vite, an iframe preview, etc.). Look up `/oxy-app.json`
  // at the document root; the vite-plugin's dev shim and the
  // standard `public/` convention both serve it there.
  return "/oxy-app.json";
}

// ── Validation ──────────────────────────────────────────────────────────────

/**
 * Validate a v2 manifest. Required: schemaVersion === 2, slug (non-empty).
 * Optional: name (display), orgSlug (dev-time hint for the admin dialog),
 * projectId (dev-time hint when there's no server-side injection).
 *
 * At serve time, oxy's identity injection (window.__OXY_APP__) overrides
 * the manifest's orgSlug/projectId — the manifest fields are advisory.
 */
function validateManifest(raw: unknown, url: string): OxyAppManifest {
  if (!isRecord(raw)) {
    throw new Error(`Manifest at ${url} is not a JSON object`);
  }
  if (raw.schemaVersion !== 2) {
    throw new Error(
      `oxy-app.json: schemaVersion must be 2 (got ${JSON.stringify(raw.schemaVersion)}). ` +
        `v1 manifests are no longer supported — upgrade to the identity-only shape.`
    );
  }
  if (raw.products !== undefined || raw.writers !== undefined) {
    throw new Error(
      `oxy-app.json is schemaVersion 2 (identity-only); \`products\` and \`writers\` are no longer supported`
    );
  }
  if (typeof raw.slug !== "string" || !raw.slug.trim()) {
    throw new Error("oxy-app.json: `slug` is required and must be a non-empty string");
  }
  if (!isValidSlug(raw.slug)) {
    // The slug becomes the app's OLTP schema/role name, a repo_path segment and
    // the served `/customer-apps/<org>/<slug>/` base path — `oxy publish` (and
    // `app_writer_name`) reject a bad one, so fail here at build, not in CI.
    throw new Error(
      `oxy-app.json: \`slug\` ${JSON.stringify(raw.slug)} is invalid — use 1–63 lowercase ` +
        `letters, digits and single hyphens (no leading/trailing/double hyphen, no underscore)`
    );
  }

  const name = typeof raw.name === "string" ? raw.name : undefined;
  const slug = raw.slug;
  const orgSlug = typeof raw.orgSlug === "string" ? raw.orgSlug : undefined;
  const projectId = typeof raw.projectId === "string" ? raw.projectId : undefined;
  const functions = raw.functions !== undefined ? validateFunctions(raw.functions) : undefined;
  const ask = isRecord(raw.ask)
    ? {
        agent: typeof raw.ask.agent === "string" ? raw.ask.agent : undefined,
        suggestedQuestions: Array.isArray(raw.ask.suggestedQuestions)
          ? raw.ask.suggestedQuestions.filter((q): q is string => typeof q === "string")
          : undefined
      }
    : undefined;

  return { schemaVersion: 2, name, slug, orgSlug, projectId, functions, ask };
}

// Mirrors the server's `is_valid_slug` (admin/apps/ops.rs): 1–63 chars of
// lowercase alphanumerics and single hyphens, no leading/trailing/double hyphen,
// no underscore. The regex forbids a leading/trailing/double hyphen structurally;
// the length is checked separately. (The vite plugin's build-time gate is the
// primary one; this runtime check only fires on standalone `pnpm dev`.)
const SLUG_RE = /^[a-z0-9]+(-[a-z0-9]+)*$/;
function isValidSlug(s: string): boolean {
  return s.length <= 63 && SLUG_RE.test(s);
}

const FUNCTION_NAME_RE = /^[a-z][a-z0-9-]{0,63}$/;

/**
 * Validate the optional `functions` map. Each key is a function name; each value
 * declares how the function is invoked. Mirrors the server-side function-name
 * rule enforced at publish in `custom_apps_publish.rs` — `is_valid_function_name`,
 * checked in `record_functions` before any row is written.
 *
 * This runs at manifest LOAD (app boot / `pnpm dev`), not at `oxy build` — the
 * build-time gate is the vite plugin's `validateManifest`, which now checks
 * function names too. `oxy publish` also validates them locally before esbuild,
 * so a bad name fails before the upload regardless.
 */
function validateFunctions(raw: unknown): Record<string, OxyAppFunctionManifest> {
  if (!isRecord(raw)) {
    throw new Error("oxy-app.json: `functions` must be an object keyed by function name");
  }
  const out: Record<string, OxyAppFunctionManifest> = {};
  for (const [fnName, value] of Object.entries(raw)) {
    if (!FUNCTION_NAME_RE.test(fnName)) {
      throw new Error(`oxy-app.json: function name "${fnName}" must match ^[a-z][a-z0-9-]{0,63}$`);
    }
    if (!isRecord(value)) {
      throw new Error(`oxy-app.json: function "${fnName}" must be an object`);
    }
    const fn: OxyAppFunctionManifest = {};
    if (value.entry !== undefined) {
      if (typeof value.entry !== "string" || !value.entry.trim()) {
        throw new Error(`oxy-app.json: function "${fnName}" \`entry\` must be a non-empty string`);
      }
      fn.entry = value.entry;
    }
    if (value.schedule !== undefined) {
      if (typeof value.schedule !== "string" || !value.schedule.trim()) {
        throw new Error(`oxy-app.json: function "${fnName}" \`schedule\` must be a cron string`);
      }
      fn.schedule = value.schedule;
    }
    if (value.timezone !== undefined) {
      if (typeof value.timezone !== "string") {
        throw new Error(`oxy-app.json: function "${fnName}" \`timezone\` must be a string`);
      }
      fn.timezone = value.timezone;
    }
    if (value.route !== undefined) {
      if (typeof value.route !== "boolean") {
        throw new Error(`oxy-app.json: function "${fnName}" \`route\` must be a boolean`);
      }
      fn.route = value.route;
    }
    if (value.airwayStep !== undefined) {
      const step = value.airwayStep;
      if (
        !isRecord(step) ||
        typeof step.pipeline !== "string" ||
        typeof step.resource !== "string"
      ) {
        throw new Error(
          `oxy-app.json: function "${fnName}" \`airwayStep\` must be { pipeline, resource }`
        );
      }
      fn.airwayStep = { pipeline: step.pipeline, resource: step.resource };
    }
    if (value.timeoutSeconds !== undefined) {
      const t = value.timeoutSeconds;
      if (typeof t !== "number" || !Number.isInteger(t) || t < 1 || t > 300) {
        throw new Error(
          `oxy-app.json: function "${fnName}" \`timeoutSeconds\` must be an integer in [1, 300]`
        );
      }
      fn.timeoutSeconds = t;
    }
    if (value.cache !== undefined) {
      const c = value.cache;
      if (!isRecord(c)) {
        throw new Error(`oxy-app.json: function "${fnName}" \`cache\` must be an object`);
      }
      if (c.ttlSeconds !== undefined) {
        const ttl = c.ttlSeconds;
        if (typeof ttl !== "number" || !Number.isInteger(ttl) || ttl < 1) {
          throw new Error(
            `oxy-app.json: function "${fnName}" \`cache.ttlSeconds\` must be a positive integer`
          );
        }
        fn.cache = { ttlSeconds: ttl };
      }
    }
    if (value.retries !== undefined) {
      const r = value.retries;
      if (!isRecord(r)) {
        throw new Error(`oxy-app.json: function "${fnName}" \`retries\` must be an object`);
      }
      const retries: { maxAttempts?: number; minTimeoutMs?: number; maxTimeoutMs?: number } = {};
      for (const key of ["maxAttempts", "minTimeoutMs", "maxTimeoutMs"] as const) {
        const n = r[key];
        if (n !== undefined) {
          if (typeof n !== "number" || !Number.isInteger(n) || n < 1) {
            throw new Error(
              `oxy-app.json: function "${fnName}" \`retries.${key}\` must be a positive integer`
            );
          }
          retries[key] = n;
        }
      }
      fn.retries = retries;
    }
    if (value.webhook !== undefined) {
      const w = value.webhook;
      if (!isRecord(w)) {
        throw new Error(`oxy-app.json: function "${fnName}" \`webhook\` must be an object`);
      }
      // Both are required, and the error is loud on purpose: a webhook block
      // the server cannot read is not a degraded endpoint, it is a permanent
      // 404 on a URL the author has already handed to a provider.
      for (const key of ["secretVar", "signatureHeader"] as const) {
        if (typeof w[key] !== "string" || !(w[key] as string).trim()) {
          throw new Error(
            `oxy-app.json: function "${fnName}" \`webhook.${key}\` must be a non-empty string`
          );
        }
      }
      const webhook: NonNullable<OxyAppFunctionManifest["webhook"]> = {
        secretVar: w.secretVar as string,
        signatureHeader: w.signatureHeader as string
      };
      if (w.encoding !== undefined) {
        if (w.encoding !== "hex" && w.encoding !== "base64") {
          throw new Error(
            `oxy-app.json: function "${fnName}" \`webhook.encoding\` must be "hex" or "base64"`
          );
        }
        webhook.encoding = w.encoding;
      }
      fn.webhook = webhook;
    }
    if (value.inputExample !== undefined) {
      // Arbitrary JSON sample — passed through verbatim for the "Run now" prefill.
      fn.inputExample = value.inputExample;
    }
    // At least one invocation surface must be active. `route` defaults
    // to true only when no other surface is declared, matching the doc.
    const hasSchedule = fn.schedule !== undefined;
    const hasAirway = fn.airwayStep !== undefined;
    const hasWebhook = fn.webhook !== undefined;
    const routeActive = fn.route ?? !(hasSchedule || hasAirway || hasWebhook);
    if (!routeActive && !hasSchedule && !hasAirway && !hasWebhook) {
      throw new Error(
        `oxy-app.json: function "${fnName}" must enable at least one of ` +
          `route/schedule/airwayStep/webhook`
      );
    }
    out[fnName] = fn;
  }
  return out;
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
