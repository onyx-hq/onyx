/**
 * Turning `--env` / `--target` into a base URL, and mining the org slug a
 * pasted URL carries.
 *
 * A port of the Rust `env_url.rs` and the target half of `app_manifest.rs`,
 * both since deleted. Kept faithful rather than improved: the credentials file
 * is keyed by HOST and still holds every login the Rust `oxy login` cached, so
 * a change to which host an `--env` names would look those tokens up under a
 * different key and report the login as missing.
 *
 * The one thing worth restating, because it is not obvious from the signature:
 * both org host schemes canonicalise back to the *deployment's* product host.
 * `poke-house.oxygen-hq.com` and `acme.oxygen-hq.com` are the same target with
 * different org slugs, so you log in once per deployment and not once per
 * customer.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { CliError, ExitCode } from "../util/errors.js";

/** A resolved `--env`: where to send the request, and whose org it named. */
export interface ResolvedEnv {
  /** Base URL. No trailing slash, no path. */
  target: string;
  /** Org slug the host carried, when it carried one. Never invented. */
  orgSlug?: string;
}

/**
 * Org-subdomain zones and the product host that serves each.
 *
 * Mirrors the server's one-zone-per-deployment model (`OXY_ORG_SUBDOMAIN_ZONE`
 * in `oxy_app_core::org_host_dispatch`). The CLI cannot read that env var, so
 * the well-known deployments are listed; an unknown zone falls through to
 * "the URL is its own target", which is the honest answer for a self-hosted or
 * preview deployment rather than a guess.
 */
const ORG_ZONES: ReadonlyArray<readonly [zone: string, product: string]> = [
  ["oxygen-hq.com", "https://app.oxygen-hq.com"],
  ["dev.oxy.tech", "https://aip.dev.oxy.tech"],
  ["staging.oxy.tech", "https://aip.staging.oxy.tech"]
];

/** Host labels that are infrastructure — present, they imply no org. */
const RESERVED_LABELS = new Set([
  "app",
  "aip",
  "www",
  "api",
  "admin",
  "static",
  "assets",
  "cdn",
  "docs",
  "customer-apps",
  "customerapps"
]);

/**
 * Built-in target for a well-known env name.
 *
 * `local` is the Vite dev server on 5173, NOT oxy's own 3000 — and that is
 * load-bearing rather than a preference. `oxyc login` opens `<target>/cli-auth`,
 * a route that exists only in the live web app, while `oxy serve` serves a
 * pre-built embedded bundle that may predate it. Vite proxies `/api/*` through
 * to :3000, so requests work either way; login only works through 5173.
 */
export function defaultTarget(env: string): string | undefined {
  switch (env) {
    case "local":
      return "http://localhost:5173";
    case "dev":
    case "development":
      return "https://aip.dev.oxy.tech";
    case "staging":
      return "https://aip.staging.oxy.tech";
    case "production":
    case "prod":
      return "https://app.oxygen-hq.com";
    default:
      return undefined;
  }
}

/**
 * Should this `--env` value be read as a URL rather than an env name?
 *
 * Env names are bare identifiers; a `:`, `/` or `.` means somebody pasted an
 * address bar. Permissive about the scheme so a copied `app.oxygen-hq.com`
 * works without the `https://`.
 */
export function looksLikeUrl(value: string): boolean {
  const v = value.trim();
  if (!v) return false;
  return v.includes("://") || v.includes(".") || v.includes("/") || v.includes(":");
}

/**
 * Add a scheme so the value parses. Loopback gets `http` — nobody runs TLS on
 * a local `oxy serve` — everything else `https`.
 */
function withScheme(value: string): string {
  if (value.includes("://")) return value;
  // A bracketed IPv6 literal is taken through its closing `]`; splitting on
  // ':' first would chop `[::1]:3000` into `[`.
  const host = value.startsWith("[")
    ? `${value.split("]")[0]}]`
    : (value.split(/[/:]/)[0] ?? value);
  const loopback = ["localhost", "127.0.0.1", "0.0.0.0", "[::1]"].includes(host);
  return `${loopback ? "http" : "https"}://${value}`;
}

/**
 * `scheme://host[:port]`. Path, query and fragment are dropped because the URL
 * a user pastes is a *page* (`/orgs/…/threads/…`), not an API base. `--target`
 * is the escape hatch that stays verbatim, for a deployment under a path.
 */
function baseUrl(url: URL): string {
  const host = url.hostname.replace(/\.+$/, "").toLowerCase();
  return url.port ? `${url.protocol}//${host}:${url.port}` : `${url.protocol}//${host}`;
}

/** The single label of `host` inside `zone`. A multi-label prefix is refused. */
function labelInZone(host: string, zone: string): string | undefined {
  const suffix = `.${zone}`;
  if (!host.endsWith(suffix)) return undefined;
  const label = host.slice(0, -suffix.length);
  if (!label || label.includes(".")) return undefined;
  return label;
}

/** Org slug carried by `<org>--<app>.customer-apps.<zone>`. */
function customAppOrg(host: string, zone: string): string | undefined {
  const label = labelInZone(host, `customer-apps.${zone}`);
  if (!label) return undefined;
  const idx = label.indexOf("--");
  if (idx <= 0) return undefined;
  const org = label.slice(0, idx);
  const app = label.slice(idx + 2);
  if (!org || !app) return undefined;
  return org;
}

/** Resolve a pasted URL to a target, plus the org slug its host named. */
export function parseEnvUrl(value: string): ResolvedEnv | undefined {
  let url: URL;
  try {
    url = new URL(withScheme(value.trim()));
  } catch {
    return undefined;
  }
  const host = url.hostname.replace(/\.+$/, "").toLowerCase();

  for (const [zone, product] of ORG_ZONES) {
    // Custom-app subdomain first: its host also ends in the org zone, but its
    // label carries a `--` pair the org rule would mis-read as a slug.
    const appOrg = customAppOrg(host, zone);
    if (appOrg) return { target: product, orgSlug: appOrg };

    const label = labelInZone(host, zone);
    if (!label) continue;
    if (RESERVED_LABELS.has(label)) return { target: product };
    return { target: product, orgSlug: label };
  }

  return { target: baseUrl(url) };
}

/** The subset of `oxy-app.json` this CLI reads. Unknown fields are ignored. */
export interface OxyAppManifest {
  slug?: string;
  orgSlug?: string;
  name?: string;
  environments?: Record<string, { target?: string }>;
}

/**
 * Read `<dir>/oxy-app.json`. `undefined` only when there is no such file.
 *
 * A file that exists but cannot be read, is not JSON, or carries a field this
 * CLI reads in the wrong shape is an error naming the path — never `undefined`.
 * Treating it as absent made every caller carry on with the wrong target,
 * identity and function set: `publish` shipped with no functions bundled.
 * Unknown fields still pass, so an SDK field added tomorrow breaks nothing.
 */
export function readAppManifest(dir: string): Record<string, unknown> | undefined {
  const path = join(dir, "oxy-app.json");
  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw invalidManifest(path, `cannot read it: ${(err as Error).message}`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    throw invalidManifest(path, (err as Error).message);
  }
  const problem = shapeProblem(parsed);
  if (problem) throw invalidManifest(path, problem);
  return parsed as Record<string, unknown>;
}

function invalidManifest(path: string, detail: string): CliError {
  return new CliError(`${path} is not a valid oxy-app.json: ${detail}`, {
    code: ExitCode.USAGE,
    remedy: "fix the file and re-run"
  });
}

const isObject = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

/** Checks only the fields this CLI reads, as the types it reads them as. `null` is absent. */
function shapeProblem(manifest: unknown): string | undefined {
  if (!isObject(manifest)) return "the top level must be an object";
  const notString = (obj: Record<string, unknown>, key: string) =>
    obj[key] != null && typeof obj[key] !== "string";
  const notObject = (obj: Record<string, unknown>, key: string) =>
    obj[key] != null && !isObject(obj[key]);

  for (const key of ["slug", "orgSlug", "name"]) {
    if (notString(manifest, key)) return `\`${key}\` must be a string`;
  }
  for (const key of ["environments", "functions", "build"]) {
    if (notObject(manifest, key)) return `\`${key}\` must be an object`;
  }
  const build = (manifest.build ?? {}) as Record<string, unknown>;
  for (const key of ["install", "command", "outDir"]) {
    if (notString(build, key)) return `\`build.${key}\` must be a string`;
  }
  for (const [section, field] of [
    ["environments", "target"],
    ["functions", "entry"]
  ] as const) {
    const entries = (manifest[section] ?? {}) as Record<string, unknown>;
    for (const [name, entry] of Object.entries(entries)) {
      if (!isObject(entry)) return `\`${section}.${name}\` must be an object`;
      if (notString(entry, field)) return `\`${section}.${name}.${field}\` must be a string`;
    }
  }
  return undefined;
}

/** `<dir>/oxy-app.json` for identity and target, strictly — see `readAppManifest`. */
export function loadManifest(dir: string): OxyAppManifest | undefined {
  return readAppManifest(dir) as OxyAppManifest | undefined;
}

/**
 * The manifest for a command that reads it only to resolve the target.
 *
 * A non-blank `--target` wins outright in `resolveEnv`, so the file cannot
 * change the outcome: it is not read, and a broken `oxy-app.json` does not fail
 * a command that was told where to go. Otherwise `loadManifest`, strict as
 * ever. `publish` and `init-ci` read it for identity and functions whatever
 * `--target` says, so they do not come through here.
 */
export function loadForTargetResolution(
  dir: string,
  targetFlag: string | undefined
): OxyAppManifest | undefined {
  return targetFlag?.trim() ? undefined : loadManifest(dir);
}

/**
 * Resolve the deployment to talk to.
 *
 * Precedence, matching the Rust exactly:
 *   `--target`  →  manifest `environments.<env>.target`  →  built-in default
 *   →  the `--env` value read as a URL
 *
 * The URL reading is last and purely additive: every named env keeps working
 * exactly as before, and a name always beats the URL interpretation. That
 * ordering is why `--env local` never tries to resolve `local` as a hostname.
 */
export function resolveEnv(
  env: string | undefined,
  targetFlag: string | undefined,
  manifest?: OxyAppManifest
): ResolvedEnv | undefined {
  // `--target` is the explicit escape hatch and stays verbatim, including for
  // a deployment served under a path. Its org slug is still mined, so
  // `--target https://<org>.oxygen-hq.com` knows which org it points at.
  if (targetFlag?.trim()) {
    const verbatim = targetFlag.trim().replace(/\/+$/, "");
    return { target: verbatim, orgSlug: parseEnvUrl(verbatim)?.orgSlug };
  }

  const name = env?.trim();
  if (!name) return undefined;

  const fromManifest = manifest?.environments?.[name]?.target?.trim();
  if (fromManifest) {
    return {
      target: fromManifest.replace(/\/+$/, ""),
      orgSlug: parseEnvUrl(fromManifest)?.orgSlug
    };
  }

  const builtin = defaultTarget(name);
  if (builtin) return { target: builtin };

  if (looksLikeUrl(name)) return parseEnvUrl(name);
  return undefined;
}
