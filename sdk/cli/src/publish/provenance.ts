/**
 * Where a build came from: git remote, commit and branch, recorded per build so
 * the admin UI can link it back to code.
 *
 * NEVER FAILS A PUBLISH. A `--dir` publish from a directory that is not a
 * checkout, or a runner with no git at all, is legitimate — every git answer
 * here degrades to "unknown", and the gaps become warnings.
 */

import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";

/** One git answer, or `undefined` for any failure including a missing binary. */
function gitAnswer(args: string[], cwd: string): string | undefined {
  try {
    const result = spawnSync("git", args, { cwd, encoding: "utf8" });
    if (result.error || result.status !== 0) return undefined;
    return result.stdout.trim() || undefined;
  } catch {
    return undefined;
  }
}

export interface GitSource {
  repo?: string;
  commit?: string;
  branch?: string;
}

/**
 * Remote, HEAD and branch of the checkout at `dir`.
 *
 * A detached HEAD reports no branch rather than the literal `HEAD` that
 * `rev-parse --abbrev-ref` prints — so a CI checkout of a pinned commit falls
 * through to `GITHUB_REF_NAME` instead of recording a branch named "HEAD".
 */
export function captureGitSource(dir: string): GitSource {
  const branch = gitAnswer(["rev-parse", "--abbrev-ref", "HEAD"], dir);
  return {
    repo: gitAnswer(["remote", "get-url", "origin"], dir),
    commit: gitAnswer(["rev-parse", "HEAD"], dir),
    branch: branch === "HEAD" ? undefined : branch
  };
}

/**
 * Uncommitted changes under `dir` — staged, unstaged or untracked — or
 * `undefined` when git cannot say.
 *
 * SCOPED TO `dir` by the `-- .` pathspec: many apps share one repo, and a
 * colleague's edit to a different app must not make this warning cry wolf.
 */
export function worktreeIsDirty(dir: string): boolean | undefined {
  try {
    const result = spawnSync("git", ["status", "--porcelain", "--", "."], {
      cwd: dir,
      encoding: "utf8"
    });
    if (result.error || result.status !== 0) return undefined;
    return result.stdout.trim().length > 0;
  } catch {
    return undefined;
  }
}

/**
 * Strip `userinfo@` from a scheme URL before it is persisted — an
 * `https://x-access-token:<TOKEN>@github.com/…` remote must never reach the
 * database. The scp-like `git@github.com:o/r` form carries no secret.
 */
export function sanitizeRemoteUrl(url: string): string {
  const schemeEnd = url.indexOf("://");
  if (schemeEnd < 0) return url;
  const start = schemeEnd + 3;
  const rest = url.slice(start);
  const authorityEnd = rest.includes("/") ? rest.indexOf("/") : rest.length;
  const at = rest.slice(0, authorityEnd).lastIndexOf("@");
  return at < 0 ? url : url.slice(0, start) + rest.slice(at + 1);
}

/** A field counts only with non-whitespace content: `--repo ""` is not provenance. */
export function isRecorded(value: string | undefined): boolean {
  return value !== undefined && value.trim().length > 0;
}

export type ProvenanceGap = "no-source" | "missing-repo" | "missing-commit" | "dirty-tree";

/**
 * Every way the recorded provenance fails to describe what is being shipped.
 *
 * Incompleteness (the same classification the admin list makes) and dirtiness
 * are orthogonal, so both are returned. A dirty tree only matters behind a
 * recorded commit — without one there is no claim for it to contradict.
 */
export function provenanceGaps(
  dirty: boolean | undefined,
  commit: string | undefined,
  repo: string | undefined
): ProvenanceGap[] {
  const hasRepo = isRecorded(repo);
  const hasCommit = isRecorded(commit);
  const gaps: ProvenanceGap[] = [];
  if (!hasRepo && !hasCommit) gaps.push("no-source");
  else if (!hasRepo) gaps.push("missing-repo");
  else if (!hasCommit) gaps.push("missing-commit");
  if (dirty === true && hasCommit) gaps.push("dirty-tree");
  return gaps;
}

export const GAP_MESSAGES: Record<ProvenanceGap, string> = {
  "no-source":
    "no git source recorded for this build — nobody will be able to trace it back to code. " +
    "Publish from the app's git checkout, or pass --repo/--commit.",
  "missing-commit":
    "a git repo is recorded for this build but no commit — the link points at a branch, which " +
    "moves, so it won't identify the code that is running. Pass --commit, or publish from the checkout.",
  "missing-repo":
    "a commit is recorded for this build but no git repo — there is nowhere to resolve that sha. " +
    "Pass --repo, or publish from the checkout.",
  "dirty-tree":
    "publishing from a dirty working tree — the commit recorded for this build does NOT contain " +
    "the changes you are shipping. Commit and re-publish if this build needs to be reproducible."
};

/**
 * The default build id under GitHub Actions: the commit, qualified by the run.
 *
 * `GITHUB_SHA` alone repeats on "Re-run all jobs", and a build id is immutable
 * and cached by id — a reused one is a 409 at best and a merge into a cached
 * build at worst. Each qualifier falls back independently.
 */
export function ciBuildId(env: NodeJS.ProcessEnv = process.env): string | undefined {
  const sha = env.GITHUB_SHA?.trim();
  if (!sha) return undefined;
  const run = env.GITHUB_RUN_ID?.trim();
  const attempt = env.GITHUB_RUN_ATTEMPT?.trim();
  if (run && attempt) return `${sha}-${run}.${attempt}`;
  if (run) return `${sha}-${run}`;
  return sha;
}

/** `--build-id`, else the CI id, else a random one. */
export function resolveBuildId(
  flag: string | undefined,
  env: NodeJS.ProcessEnv = process.env
): string {
  return flag?.trim() || ciBuildId(env) || randomUUID().replaceAll("-", "");
}
