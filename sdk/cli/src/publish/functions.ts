/**
 * `functions/` in a published bundle: reserved for Oxy Functions, bundled here.
 *
 * The serve plane blocks `functions/<child>` for EVERY app, so a frontend file
 * built into that directory is uploaded and then never served. The check runs
 * whether or not the app declares functions; only its severity depends on
 * whether this publish is about to write there.
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync } from "node:fs";
import { join } from "node:path";

import { CliError, ExitCode } from "../util/errors.js";
import { isValidFunctionName } from "./manifest.js";

/** What sits in `functions/` that this publish did not put there, sorted. */
export interface FunctionsDirConflicts {
  /**
   * `<name>.js` for a valid function name no longer declared — plausibly Oxy's
   * own output from an earlier publish. A GUESS from the filename (`main.js`
   * satisfies the grammar too), so it chooses wording only, never severity.
   */
  staleArtifacts: string[];
  /** Anything else: a page, an asset, a bundler chunk, a directory. */
  frontend: string[];
}

function looksLikeOurArtifact(name: string): boolean {
  return name.endsWith(".js") && isValidFunctionName(name.slice(0, -".js".length));
}

/**
 * Inspect the reserved directory. An absent directory is the normal case; any
 * other read failure is fatal, because "could not tell" must not read as "no
 * collision".
 */
export function functionsDirConflicts(outFns: string, declared: string[]): FunctionsDirConflicts {
  const expected = new Set(declared.map((name) => `${name}.js`));
  let entries: string[];
  try {
    entries = readdirSync(outFns);
  } catch (cause) {
    if ((cause as NodeJS.ErrnoException).code === "ENOENT")
      return { staleArtifacts: [], frontend: [] };
    throw new CliError(
      `cannot inspect the reserved bundle path ${outFns}: ${(cause as Error).message}`
    );
  }
  const conflicts: FunctionsDirConflicts = { staleArtifacts: [], frontend: [] };
  for (const name of entries.filter((entry) => !expected.has(entry))) {
    (looksLikeOurArtifact(name) ? conflicts.staleArtifacts : conflicts.frontend).push(name);
  }
  conflicts.staleArtifacts.sort();
  conflicts.frontend.sort();
  return conflicts;
}

/** A bounded, stable rendering: five names, then a count. */
export function describeEntries(entries: string[]): string {
  const shown = 5;
  const head = entries
    .slice(0, shown)
    .map((entry) => `functions/${entry}`)
    .join(", ");
  const extra = entries.length - shown;
  return extra > 0 ? `${head} (and ${extra} more)` : head;
}

/**
 * Enforce the reserved path. Returns a warning to print, or throws.
 *
 * Writing (functions declared): ANY conflict refuses, one message for both
 * buckets. Not writing: one warning — refusing would break a publish that
 * works, over files that are merely unreachable.
 */
export function enforceReservedFunctionsDir(
  bundleDir: string,
  declared: string[],
  weWillWrite: boolean
): string | undefined {
  const conflicts = functionsDirConflicts(join(bundleDir, "functions"), declared);
  if (conflicts.frontend.length === 0 && conflicts.staleArtifacts.length === 0) return undefined;

  if (!weWillWrite) {
    // Capped per bucket, then joined: capping a merged list lets alphabetical
    // order decide which class of file gets named at all.
    const listed = [conflicts.frontend, conflicts.staleArtifacts]
      .filter((list) => list.length > 0)
      .map(describeEntries)
      .join(", ");
    return (
      `this bundle declares no Oxy Functions, but its build output contains ${listed}. ` +
      "`functions/` is reserved in a published bundle — those files will be uploaded and " +
      "will NOT be served. Emit them somewhere else."
    );
  }

  const detail: string[] = [];
  if (conflicts.frontend.length > 0) {
    detail.push(
      `  - ${describeEntries(conflicts.frontend)} — looks like frontend output; emit it somewhere else`
    );
  }
  if (conflicts.staleArtifacts.length > 0) {
    detail.push(
      `  - ${describeEntries(conflicts.staleArtifacts)} — looks like an artifact from a previous ` +
        "publish, for a function oxy-app.json no longer declares"
    );
  }
  throw new CliError(
    "`functions/` is reserved for Oxy Functions in a published bundle, and this build output " +
      "already contains files Oxy did not put there",
    {
      code: ExitCode.REFUSED,
      detail: detail.join("\n"),
      hint:
        "publishing would upload them into a path the serve plane blocks — emit them somewhere " +
        "else, or clean the build directory and re-publish"
    }
  );
}

/** Runs a command, streaming its output; throws when it fails. */
export type Runner = (label: string, command: string, args: string[], cwd: string) => void;

export const spawnRunner: Runner = (label, command, args, cwd) => {
  process.stderr.write(`[${label}] $ ${[command, ...args].join(" ")}\n`);
  // The child's stdout goes to OUR stderr: stdout is the result (`--json`).
  const result = spawnSync(command, args, { cwd, stdio: ["inherit", 2, 2] });
  if (result.error) throw new CliError(`failed to start \`${command}\`: ${result.error.message}`);
  if (result.status !== 0) {
    throw new CliError(
      `\`${[command, ...args].join(" ")}\` failed (exit ${result.status ?? "signal"})`
    );
  }
};

/**
 * esbuild each declared function into `<bundleDir>/functions/<name>.js`.
 *
 * Through the app's own toolchain (`pnpm exec esbuild`) so dependencies resolve
 * exactly as in `pnpm build`, from the app directory. Argv, not a shell string,
 * so an entry path cannot inject into the command line.
 */
export function bundleFunctions(
  entries: Array<{ name: string; entry: string }>,
  appDir: string,
  bundleDir: string,
  run: Runner = spawnRunner
): void {
  if (entries.length === 0) return;
  const outFns = join(bundleDir, "functions");
  mkdirSync(outFns, { recursive: true });
  for (const { name, entry } of entries) {
    run(
      `fn:${name}`,
      "pnpm",
      [
        "exec",
        "esbuild",
        entry,
        "--bundle",
        "--format=esm",
        // The runtime provides `ctx` / `fetch`, not Node built-ins.
        "--platform=neutral",
        // Stack traces remap to the author's `.ts` lines…
        "--sourcemap=inline",
        // …without shipping every original source file inside the map.
        "--sources-content=false",
        `--outfile=${join(outFns, `${name}.js`)}`
      ],
      appDir
    );
  }
}

/**
 * `--prebuilt`: the bundle was built and its functions bundled elsewhere (a CI
 * build job). Every declared function must already be there — the server does
 * not check, so a missing one would publish fine and 404 at runtime.
 */
export function requirePrebuiltFunctions(bundleDir: string, declared: string[]): void {
  const missing = declared.filter(
    (name) => !existsSync(join(bundleDir, "functions", `${name}.js`))
  );
  if (missing.length === 0) return;
  throw new CliError(`--prebuilt bundle is missing ${missing.length} declared function(s)`, {
    code: ExitCode.USAGE,
    detail: missing.map((name) => `  functions/${name}.js`).join("\n"),
    hint: "build it with `oxyc publish --build-only` first, which bundles them into the output directory"
  });
}

/** Validate every declared name before anything is written. */
export function validateFunctionNames(declared: string[]): void {
  const bad = declared.find((name) => !isValidFunctionName(name));
  if (bad !== undefined) {
    throw new CliError(`invalid function name ${JSON.stringify(bad)}`, {
      code: ExitCode.USAGE,
      hint: "function names must match ^[a-z][a-z0-9-]{0,63}$"
    });
  }
}
