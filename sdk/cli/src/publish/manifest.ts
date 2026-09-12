/**
 * The half of `oxy-app.json` that publishing reads, and the two name rules the
 * server enforces on it.
 *
 * `context/target.ts` owns identity and `environments`; this adds `build` and
 * `functions`. Both read the same file and ignore what they do not know, so an
 * SDK field added tomorrow cannot break a publish today.
 */

import { readAppManifest } from "../context/target.js";

export interface FunctionSpec {
  entry?: string;
}

export interface PublishManifest {
  slug?: string;
  orgSlug?: string;
  build?: { install?: string; command?: string; outDir?: string };
  functions?: Record<string, FunctionSpec>;
}

/**
 * `<dir>/oxy-app.json`, or `undefined` when there is none. A broken one throws:
 * publishing past it used to ship a bundle with no functions and an identity
 * taken from flags or the directory.
 */
export function loadPublishManifest(dir: string): PublishManifest | undefined {
  return readAppManifest(dir) as PublishManifest | undefined;
}

/** A blank manifest value is no value: `"install": ""` falls back to the default. */
function orDefault(value: string | undefined, fallback: string): string {
  return value?.trim() ? value : fallback;
}

export interface BuildSteps {
  install: string;
  command: string;
  outDir: string;
}

/**
 * Install / build / output directory. `out` matches the directory the Vite
 * plugin forces, so an identity-only manifest publishes without a `build` block.
 */
export function buildSteps(manifest: PublishManifest | undefined): BuildSteps {
  return {
    install: orDefault(manifest?.build?.install, "pnpm install"),
    command: orDefault(manifest?.build?.command, "pnpm build"),
    outDir: orDefault(manifest?.build?.outDir, "out")
  };
}

/** Declared function names, or an empty list. */
export function declaredFunctions(manifest: PublishManifest | undefined): string[] {
  const functions = manifest?.functions;
  return functions && typeof functions === "object" ? Object.keys(functions) : [];
}

/** Source entry relative to the app directory. Default `functions/<name>.ts`. */
export function functionEntry(manifest: PublishManifest | undefined, name: string): string {
  return orDefault(manifest?.functions?.[name]?.entry, `functions/${name}.ts`);
}

/**
 * `^[a-z][a-z0-9-]{0,63}$` — the server's `is_valid_function_name`.
 *
 * A manifest key becomes `functions/<name>.js`, so `../../x` has to be refused
 * before esbuild is told to write there.
 */
export function isValidFunctionName(name: string): boolean {
  return /^[a-z][a-z0-9-]{0,63}$/.test(name);
}

/**
 * The server's `is_valid_slug`: 1–63 lowercase letters, digits and single
 * hyphens, no leading or trailing hyphen.
 *
 * Checked before anything is built, so an author whose bundle never went
 * through the Vite plugin learns about it before uploading rather than after.
 */
export function isValidSlug(slug: string): boolean {
  return (
    slug.length > 0 &&
    slug.length <= 63 &&
    /^[a-z0-9-]+$/.test(slug) &&
    !slug.startsWith("-") &&
    !slug.endsWith("-") &&
    !slug.includes("--")
  );
}
