/**
 * Build provenance: what is recorded, what is stripped, and what is warned
 * about. The gap cases are the Rust `oxy publish` tests, case for case.
 */

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

import {
  captureGitSource,
  ciBuildId,
  GAP_MESSAGES,
  provenanceGaps,
  resolveBuildId,
  sanitizeRemoteUrl,
  worktreeIsDirty
} from "./provenance.js";

describe("sanitizeRemoteUrl", () => {
  it("strips embedded credentials", () => {
    expect(sanitizeRemoteUrl("https://x-access-token:ghs_SECRET@github.com/oxy-hq/app.git")).toBe(
      "https://github.com/oxy-hq/app.git"
    );
    expect(sanitizeRemoteUrl("https://user:pass@gitlab.com/o/r")).toBe("https://gitlab.com/o/r");
  });

  it("leaves clean and SSH remotes, and an @ in the path, untouched", () => {
    expect(sanitizeRemoteUrl("https://github.com/oxy-hq/app.git")).toBe(
      "https://github.com/oxy-hq/app.git"
    );
    expect(sanitizeRemoteUrl("git@github.com:oxy-hq/app.git")).toBe(
      "git@github.com:oxy-hq/app.git"
    );
    expect(sanitizeRemoteUrl("https://github.com/o/r@weird")).toBe("https://github.com/o/r@weird");
  });
});

describe("provenanceGaps", () => {
  it("flags a build with no source at all", () => {
    expect(provenanceGaps(undefined, undefined, undefined)).toEqual(["no-source"]);
    // No recorded sha, so nothing for a dirty tree to contradict.
    expect(provenanceGaps(true, undefined, undefined)).toEqual(["no-source"]);
  });

  it("flags a dirty tree behind a recorded commit", () => {
    expect(provenanceGaps(true, "abc123", "git@github.com:o/r.git")).toEqual(["dirty-tree"]);
  });

  /** The CI shape that used to publish silently and then sit amber in the admin list. */
  it("flags a half-recorded source", () => {
    expect(provenanceGaps(undefined, "abc123", undefined)).toEqual(["missing-repo"]);
    expect(provenanceGaps(false, undefined, "git@github.com:o/r.git")).toEqual(["missing-commit"]);
    expect(provenanceGaps(false, "abc123", "  ")).toEqual(["missing-repo"]);
  });

  /** Incompleteness and dirtiness are orthogonal, so both are reported. */
  it("reports a missing repo and a dirty tree together", () => {
    expect(provenanceGaps(true, "abc123", undefined)).toEqual(["missing-repo", "dirty-tree"]);
    expect(provenanceGaps(true, undefined, "git@github.com:o/r.git")).toEqual(["missing-commit"]);
    expect(provenanceGaps(true, "   ", "git@github.com:o/r.git")).toEqual(["missing-commit"]);
  });

  it("is silent on a clean, traceable publish — and when git could not answer", () => {
    expect(provenanceGaps(false, "abc123", "git@github.com:o/r.git")).toEqual([]);
    expect(provenanceGaps(undefined, "abc123", "git@github.com:o/r.git")).toEqual([]);
  });

  it("has a distinct message for every gap", () => {
    const messages = Object.values(GAP_MESSAGES);
    expect(new Set(messages).size).toBe(messages.length);
  });
});

describe("ciBuildId", () => {
  /** A re-run on the same commit must not reuse an immutable, cached build id. */
  it("is unique per run, not per commit", () => {
    expect(ciBuildId({ GITHUB_SHA: "abc123" })).toBe("abc123");
    expect(ciBuildId({ GITHUB_SHA: "abc123", GITHUB_RUN_ID: "42" })).toBe("abc123-42");
    expect(ciBuildId({ GITHUB_SHA: "abc123", GITHUB_RUN_ID: "42", GITHUB_RUN_ATTEMPT: "2" })).toBe(
      "abc123-42.2"
    );
    expect(ciBuildId({})).toBeUndefined();
  });

  it("prefers the flag, then CI, then a random id with no dashes", () => {
    expect(resolveBuildId("mine", { GITHUB_SHA: "abc" })).toBe("mine");
    expect(resolveBuildId(undefined, { GITHUB_SHA: "abc" })).toBe("abc");
    expect(resolveBuildId(undefined, {})).toMatch(/^[0-9a-f]{32}$/);
  });
});

describe("the checkout", () => {
  function repo(): string {
    const dir = mkdtempSync(join(tmpdir(), "oxyc-provenance-"));
    const git = (...args: string[]) =>
      spawnSync("git", ["-c", "user.email=t@t", "-c", "user.name=t", ...args], { cwd: dir });
    git("init", "-q", "-b", "main");
    mkdirSync(join(dir, "apps", "a"), { recursive: true });
    mkdirSync(join(dir, "apps", "b"), { recursive: true });
    writeFileSync(join(dir, "apps", "a", "x.txt"), "x");
    writeFileSync(join(dir, "apps", "b", "y.txt"), "y");
    git("add", ".");
    git("commit", "-q", "--no-gpg-sign", "-m", "init");
    return dir;
  }

  /** `rev-parse --abbrev-ref` prints `HEAD` when detached; that is not a branch. */
  it("records no branch on a detached HEAD", () => {
    const dir = repo();
    try {
      expect(captureGitSource(dir).branch).toBe("main");
      spawnSync("git", ["checkout", "-q", "--detach"], { cwd: dir });
      const source = captureGitSource(dir);
      expect(source.branch).toBeUndefined();
      expect(source.commit).toMatch(/^[0-9a-f]{40}$/);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  /** Scoped to the app: a colleague's edit to another app is not this app's dirt. */
  it("reads dirt under the app directory only", () => {
    const dir = repo();
    try {
      writeFileSync(join(dir, "apps", "b", "y.txt"), "changed");
      expect(worktreeIsDirty(join(dir, "apps", "a"))).toBe(false);
      writeFileSync(join(dir, "apps", "a", "new.txt"), "untracked counts");
      expect(worktreeIsDirty(join(dir, "apps", "a"))).toBe(true);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("answers unknown, not an error, outside a checkout", () => {
    const dir = mkdtempSync(join(tmpdir(), "oxyc-no-git-"));
    try {
      expect(captureGitSource(dir)).toEqual({
        repo: undefined,
        commit: undefined,
        branch: undefined
      });
      expect(worktreeIsDirty(dir)).toBeUndefined();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
