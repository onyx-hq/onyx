/**
 * `functions/` as a reserved path, and the esbuild call — ported case for case
 * from the Rust `oxy publish` this replaced.
 */

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { CliError, ExitCode } from "../util/errors.js";
import {
  bundleFunctions,
  describeEntries,
  enforceReservedFunctionsDir,
  functionsDirConflicts,
  requirePrebuiltFunctions,
  validateFunctionNames
} from "./functions.js";

let root: string;
beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), "oxyc-functions-"));
});
afterEach(() => rmSync(root, { recursive: true, force: true }));

function touch(...names: string[]): string {
  const dir = join(root, "functions");
  mkdirSync(dir, { recursive: true });
  for (const name of names) writeFileSync(join(dir, name), "x");
  return dir;
}

function refusalOf(fn: () => unknown): CliError {
  try {
    fn();
  } catch (cause) {
    if (cause instanceof CliError) return cause;
    throw cause;
  }
  throw new Error("expected a refusal");
}

describe("functionsDirConflicts", () => {
  /** A re-publish into an un-cleaned build dir is normal and must not fail. */
  it("tolerates our own artifacts and reports frontend files, sorted", () => {
    const declared = ["top-stores", "daily-rollup"];
    const dir = touch("top-stores.js", "daily-rollup.js");
    expect(functionsDirConflicts(dir, declared)).toEqual({ staleArtifacts: [], frontend: [] });

    touch("index.html", "about.html");
    const conflicts = functionsDirConflicts(dir, declared);
    expect(conflicts.frontend).toEqual(["about.html", "index.html"]);
    expect(conflicts.staleArtifacts).toEqual([]);
  });

  it("separates a removed function's leftover from a frontend file", () => {
    const dir = touch("top-stores.js", "removed-fn.js", "index.html");
    const conflicts = functionsDirConflicts(dir, ["top-stores"]);
    expect(conflicts.staleArtifacts).toEqual(["removed-fn.js"]);
    expect(conflicts.frontend).toEqual(["index.html"]);
  });

  /** Matched on the grammar that produced our artifacts, not the `.js` suffix. */
  it("treats bundler chunks as frontend, not leftovers", () => {
    const dir = touch(
      "chunk-a1b2.d4f1.js",
      "main.min.js",
      "_app.js",
      "Chunk.js",
      "2fast.js",
      "removed-fn.js"
    );
    const conflicts = functionsDirConflicts(dir, []);
    expect(conflicts.staleArtifacts).toEqual(["removed-fn.js"]);
    expect(conflicts.frontend).toEqual([
      "2fast.js",
      "Chunk.js",
      "_app.js",
      "chunk-a1b2.d4f1.js",
      "main.min.js"
    ]);
  });

  it("flags everything when nothing is declared", () => {
    expect(functionsDirConflicts(touch("index.html"), []).frontend).toEqual(["index.html"]);
  });

  it("is empty when the directory does not exist", () => {
    expect(functionsDirConflicts(join(root, "functions"), ["top-stores"])).toEqual({
      staleArtifacts: [],
      frontend: []
    });
  });
});

describe("enforceReservedFunctionsDir", () => {
  /** The bucket picks the wording only; `main.js` satisfies the grammar too. */
  it("refuses a leftover as well, with its own explanation", () => {
    touch("removed-fn.js");
    const error = refusalOf(() => enforceReservedFunctionsDir(root, ["top-stores"], true));
    expect(error.code).toBe(ExitCode.REFUSED);
    expect(error.detail).toContain("functions/removed-fn.js");
    expect(error.detail).toContain("previous publish");

    rmSync(join(root, "functions", "removed-fn.js"));
    touch("main.js");
    expect(() => enforceReservedFunctionsDir(root, ["top-stores"], true)).toThrow(CliError);
  });

  /** One publish, every problem — not one class of problem per round-trip. */
  it("reports both buckets in one refusal", () => {
    touch("index.html", "removed-fn.js");
    const error = refusalOf(() => enforceReservedFunctionsDir(root, ["top-stores"], true));
    expect(error.detail).toContain("functions/index.html");
    expect(error.detail).toContain("functions/removed-fn.js");
  });

  it("refuses only when this publish writes there, and warns otherwise", () => {
    touch("index.html");
    expect(() => enforceReservedFunctionsDir(root, ["top-stores"], true)).toThrow(CliError);
    const warning = enforceReservedFunctionsDir(root, [], false);
    expect(warning).toContain("functions/index.html");
    expect(warning).toContain("will NOT be served");
  });

  /** Capping a merged list would let six leftovers push the one page out of view. */
  it("names both classes in the warning despite the cap", () => {
    touch(...Array.from({ length: 6 }, (_, i) => `aaa-fn-${i}.js`), "index.html");
    const warning = enforceReservedFunctionsDir(root, [], false);
    expect(warning).toContain("functions/index.html");
    expect(warning).toContain("(and 1 more)");
  });

  it("is silent on a clean directory", () => {
    touch("top-stores.js");
    expect(enforceReservedFunctionsDir(root, ["top-stores"], true)).toBeUndefined();
  });
});

describe("describeEntries", () => {
  it("bounds a long list", () => {
    const rendered = describeEntries(Array.from({ length: 8 }, (_, i) => `p${i}.html`));
    expect(rendered.startsWith("functions/p0.html, ")).toBe(true);
    expect(rendered.endsWith("(and 3 more)")).toBe(true);
    expect(describeEntries(["only.html"])).toBe("functions/only.html");
  });
});

describe("bundleFunctions", () => {
  /**
   * The exact esbuild argv: `--outfile=` joined (a space-separated form is
   * rejected), neutral platform, inline maps without source content.
   */
  it("runs pnpm exec esbuild per function, from the app directory", () => {
    const calls: Array<{ label: string; command: string; args: string[]; cwd: string }> = [];
    bundleFunctions(
      [{ name: "top-stores", entry: "functions/top-stores.ts" }],
      "/app",
      root,
      (label, command, args, cwd) => calls.push({ label, command, args, cwd })
    );
    expect(calls).toEqual([
      {
        label: "fn:top-stores",
        command: "pnpm",
        args: [
          "exec",
          "esbuild",
          "functions/top-stores.ts",
          "--bundle",
          "--format=esm",
          "--platform=neutral",
          "--sourcemap=inline",
          "--sources-content=false",
          `--outfile=${join(root, "functions", "top-stores.js")}`
        ],
        cwd: "/app"
      }
    ]);
  });

  it("does nothing without functions", () => {
    bundleFunctions([], "/app", root, () => {
      throw new Error("should not run");
    });
  });
});

describe("requirePrebuiltFunctions", () => {
  /** The server does not check, so a missing one would 404 at runtime. */
  it("names every declared function missing from a prebuilt bundle", () => {
    touch("present.js");
    const error = refusalOf(() => requirePrebuiltFunctions(root, ["present", "absent", "gone"]));
    expect(error.code).toBe(ExitCode.USAGE);
    expect(error.detail).toBe("  functions/absent.js\n  functions/gone.js");
    expect(() => requirePrebuiltFunctions(root, ["present"])).not.toThrow();
  });
});

describe("validateFunctionNames", () => {
  /** A key becomes a path: `../../x` would make esbuild write outside the bundle. */
  it("refuses a name outside the grammar", () => {
    expect(() => validateFunctionNames(["../../x"])).toThrow(/invalid function name/);
    expect(() => validateFunctionNames(["top-stores", "a1"])).not.toThrow();
  });
});
