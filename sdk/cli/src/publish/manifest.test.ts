/** The publish half of `oxy-app.json`, and the two server name rules. */

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

import { loadDotenv, parseDotenv } from "./dotenv.js";
import {
  buildSteps,
  declaredFunctions,
  functionEntry,
  isValidFunctionName,
  isValidSlug,
  loadPublishManifest
} from "./manifest.js";

describe("buildSteps", () => {
  it("defaults when absent, so an identity-only manifest publishes", () => {
    expect(buildSteps(undefined)).toEqual({
      install: "pnpm install",
      command: "pnpm build",
      outDir: "out"
    });
  });

  it("takes overrides, and treats a blank value as no value", () => {
    expect(buildSteps({ build: { install: "npm ci", command: "  ", outDir: "dist/spa" } })).toEqual(
      { install: "npm ci", command: "pnpm build", outDir: "dist/spa" }
    );
  });
});

describe("functions", () => {
  it("lists declared names and defaults each entry to functions/<name>.ts", () => {
    const manifest = { functions: { "top-stores": {}, rollup: { entry: "src/rollup.ts" } } };
    expect(declaredFunctions(manifest)).toEqual(["top-stores", "rollup"]);
    expect(functionEntry(manifest, "top-stores")).toBe("functions/top-stores.ts");
    expect(functionEntry(manifest, "rollup")).toBe("src/rollup.ts");
    expect(declaredFunctions(undefined)).toEqual([]);
  });

  it("follows the server's function-name grammar", () => {
    for (const ok of ["a", "top-stores", "a1", `a${"b".repeat(63)}`]) {
      expect(isValidFunctionName(ok), ok).toBe(true);
    }
    for (const bad of ["", "1a", "-a", "A", "a_b", "../x", `a${"b".repeat(64)}`]) {
      expect(isValidFunctionName(bad), bad).toBe(false);
    }
  });
});

describe("isValidSlug", () => {
  it("accepts lower kebab up to 63 characters", () => {
    for (const ok of ["a", "store-pulse", "a1-b2", "x".repeat(63)]) {
      expect(isValidSlug(ok), ok).toBe(true);
    }
  });

  /** An underscore is the one people reach for; the slug becomes a schema name. */
  it("rejects every other shape", () => {
    for (const bad of ["", "-a", "a-", "a--b", "A", "a_b", "a b", "x".repeat(64)]) {
      expect(isValidSlug(bad), bad).toBe(false);
    }
  });
});

describe("loadPublishManifest", () => {
  it("reads the file, answers undefined for a missing one and throws for a broken one", () => {
    const dir = mkdtempSync(join(tmpdir(), "oxyc-manifest-"));
    try {
      expect(loadPublishManifest(dir)).toBeUndefined();
      writeFileSync(join(dir, "oxy-app.json"), "{ not json");
      expect(() => loadPublishManifest(dir)).toThrow("is not a valid oxy-app.json");
      writeFileSync(join(dir, "oxy-app.json"), JSON.stringify({ functions: { "top-stores": 1 } }));
      expect(() => loadPublishManifest(dir)).toThrow("`functions.top-stores` must be an object");
      writeFileSync(join(dir, "oxy-app.json"), JSON.stringify({ slug: "s", orgSlug: "o" }));
      expect(loadPublishManifest(dir)).toEqual({ slug: "s", orgSlug: "o" });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("dotenv", () => {
  it("parses the shapes a .env file actually has", () => {
    expect(
      parseDotenv(
        [
          "# comment",
          "OXY_ORG=acme",
          "export OXY_APP=sales",
          'QUOTED="a # not a comment"',
          "SINGLE='x=y'",
          "TRAILING=value # comment",
          'NEWLINE="a\\nb"',
          "not a line",
          "=novalue",
          ""
        ].join("\n")
      )
    ).toEqual([
      ["OXY_ORG", "acme"],
      ["OXY_APP", "sales"],
      ["QUOTED", "a # not a comment"],
      ["SINGLE", "x=y"],
      ["TRAILING", "value"],
      ["NEWLINE", "a\nb"]
    ]);
  });

  /** CI's real environment always wins; `.env.local` beats `.env`; parents are searched. */
  it("loads .env.local then .env from the nearest ancestor, never overriding", () => {
    const root = mkdtempSync(join(tmpdir(), "oxyc-dotenv-"));
    try {
      const app = join(root, "apps", "acme", "sales");
      mkdirSync(app, { recursive: true });
      writeFileSync(join(root, ".env"), "A=root\nB=root\nC=root\n");
      writeFileSync(join(app, ".env.local"), "A=local\n");
      const env: NodeJS.ProcessEnv = { C: "shell" };
      const read = loadDotenv(app, env);
      expect(read).toEqual([join(app, ".env.local"), join(root, ".env")]);
      expect(env).toEqual({ A: "local", B: "root", C: "shell" });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
