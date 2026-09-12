/**
 * The workflow `oxyc init-ci` writes. Its value is the job split — the job
 * holding `id-token: write` runs no package script — so that is what is pinned,
 * structurally, on the parsed YAML rather than by grepping text.
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { parse } from "yaml";

import { CliError, ExitCode } from "../util/errors.js";
import { WORKFLOW_PATH, type WorkflowOptions, workflowYaml } from "./init-ci.js";

const BIN = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "dist", "main.mjs");

interface Step {
  uses?: string;
  run?: string;
  with?: Record<string, unknown>;
  env?: Record<string, string>;
  "working-directory"?: string;
}
interface Job {
  permissions?: Record<string, string>;
  environment?: string;
  steps: Step[];
}

const OPTIONS: WorkflowOptions = {
  org: "acme",
  app: "sales",
  environment: "oxy-publish",
  env: "production",
  appDir: ".",
  outDir: "out",
  pnpmFromPackageJson: false,
  nodeVersionFile: false,
  cliVersion: "9.9.9"
};

function jobs(options: WorkflowOptions): { build: Job; publish: Job } {
  return (parse(workflowYaml(options)) as { jobs: { build: Job; publish: Job } }).jobs;
}

describe("workflowYaml", () => {
  it("gives the id-token to the publish job only", () => {
    const { build, publish } = jobs(OPTIONS);
    expect(build.permissions?.["id-token"]).toBeUndefined();
    expect(publish.permissions).toEqual({ "id-token": "write", contents: "read" });
    expect(publish.environment).toBe("oxy-publish");
  });

  /** The isolation IS the security model: nothing in that job may run a package script. */
  it("keeps the publish job to checkout, download, node and one publish", () => {
    const { publish } = jobs(OPTIONS);
    expect(publish.steps.map((s) => s.uses ?? "run")).toEqual([
      "actions/checkout@v4",
      "actions/download-artifact@v4",
      "actions/setup-node@v4",
      "run"
    ]);
    const run = publish.steps[3];
    expect(run?.run).toBe(
      "npx --yes @oxy-hq/cli@9.9.9 publish --prebuilt --dir out --env production --org acme --app sales"
    );
    expect(run?.env).toEqual({ npm_config_ignore_scripts: "true" });
    expect(JSON.stringify(publish)).not.toMatch(/pnpm|install|build-only/);
  });

  it("builds and bundles in the job without the credential, pinned to this CLI", () => {
    const { build } = jobs(OPTIONS);
    const runs = build.steps.filter((s) => s.run).map((s) => s.run);
    expect(runs).toEqual([
      "pnpm dlx @oxy-hq/cli@9.9.9 publish --build-only --org acme --app sales"
    ]);
    const upload = build.steps.find((s) => s.uses === "actions/upload-artifact@v4");
    expect(upload?.with).toMatchObject({ path: "out", "include-hidden-files": true });
  });

  it("runs from the app directory when it is not the repo root", () => {
    const { build, publish } = jobs({ ...OPTIONS, appDir: "apps/acme/sales", outDir: "dist" });
    expect(build.steps.find((s) => s.run)?.["working-directory"]).toBe("apps/acme/sales");
    expect(publish.steps.at(-1)?.["working-directory"]).toBe("apps/acme/sales");
    const download = publish.steps.find((s) => s.uses === "actions/download-artifact@v4");
    expect(download?.with?.path).toBe("apps/acme/sales/dist");
  });

  /**
   * Every value lands unquoted in a `run:` line or a `path:`, and one of those
   * lines runs in the job holding `id-token: write`. An `orgSlug` of
   * `acme$(curl …|sh)` in a committed manifest must not become a workflow.
   */
  it("refuses a value that would run as shell in the generated workflow", () => {
    for (const bad of [
      { org: "acme$(id)" },
      { app: "sales;id" },
      { environment: "oxy publish" },
      { env: "dev && id" },
      { outDir: "out`id`" },
      { outDir: "../elsewhere" },
      { appDir: "apps/a b" },
      { cliVersion: "1.0.0 || id" }
    ]) {
      expect(() => workflowYaml({ ...OPTIONS, ...bad }), JSON.stringify(bad)).toThrow(CliError);
    }
  });

  it("still accepts a deployment URL as the env and a nested out dir", () => {
    const { publish } = jobs({ ...OPTIONS, env: "https://acme.oxygen-hq.com", outDir: "dist/spa" });
    expect(publish.steps.at(-1)?.run).toContain("--env https://acme.oxygen-hq.com");
  });

  /** pnpm/action-setup refuses two pnpm versions; package.json's pin is the one. */
  it("defers to package.json's pnpm and the repo's .nvmrc when present", () => {
    const { build } = jobs({ ...OPTIONS, pnpmFromPackageJson: true, nodeVersionFile: true });
    expect(build.steps.find((s) => s.uses === "pnpm/action-setup@v4")?.with).toBeUndefined();
    expect(build.steps.find((s) => s.uses === "actions/setup-node@v4")?.with).toEqual({
      "node-version-file": ".nvmrc"
    });
  });
});

describe("oxyc init-ci, through the binary", () => {
  let repo: string;
  beforeEach(() => {
    repo = mkdtempSync(join(tmpdir(), "oxyc-init-ci-"));
    spawnSync("git", ["init", "-q"], { cwd: repo });
    spawnSync("git", ["remote", "add", "origin", "git@github.com:acme-co/acme-apps.git"], {
      cwd: repo
    });
  });
  afterEach(() => rmSync(repo, { recursive: true, force: true }));

  const run = (cwd: string, args: string[]) =>
    spawnSync(process.execPath, [BIN, "init-ci", ...args], {
      cwd,
      encoding: "utf8",
      env: { PATH: process.env.PATH ?? "", HOME: repo, NO_COLOR: "1" }
    });

  /** GitHub reads workflows from the repo root, wherever the command was run. */
  it("writes the workflow at the repo root from an app directory, and prints the registration", () => {
    const appDir = join(repo, "apps", "acme", "sales");
    mkdirSync(appDir, { recursive: true });
    writeFileSync(join(appDir, "oxy-app.json"), JSON.stringify({ slug: "sales", orgSlug: "acme" }));

    const result = run(appDir, ["--env", "dev"]);
    expect(result.status, result.stderr).toBe(0);
    const written = readFileSync(join(repo, WORKFLOW_PATH), "utf8");
    expect(written).toContain("working-directory: apps/acme/sales");
    expect(written).toContain("--env dev");
    expect(result.stdout).toContain("-f repo_owner=acme-co");
    expect(result.stdout).toContain("-f repo_name=acme-apps");
    expect(result.stdout).toContain(`-f workflow_ref=${WORKFLOW_PATH}`);
  });

  it("refuses to overwrite without --force", () => {
    mkdirSync(dirname(join(repo, WORKFLOW_PATH)), { recursive: true });
    writeFileSync(join(repo, WORKFLOW_PATH), "mine");
    const refused = run(repo, ["--app", "acme/sales"]);
    expect(refused.status).toBe(ExitCode.REFUSED);
    expect(readFileSync(join(repo, WORKFLOW_PATH), "utf8")).toBe("mine");

    expect(run(repo, ["--app", "acme/sales", "--force"]).status).toBe(0);
    expect(readFileSync(join(repo, WORKFLOW_PATH), "utf8")).toContain("publish --prebuilt");
  });

  it("asks which app when neither --app nor a manifest says", () => {
    const result = run(repo, []);
    expect(result.status).toBe(ExitCode.USAGE);
    expect(existsSync(join(repo, WORKFLOW_PATH))).toBe(false);
  });
});
