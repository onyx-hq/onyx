/**
 * `oxyc publish` — build a custom app and ship the bundle to an Oxy deployment.
 *
 * From an app directory: read `oxy-app.json`, build per its `build` block (or
 * the defaults), bundle any Oxy Functions, resolve the project from the
 * target's public `build-config`, and POST the tarball. It replaced the Rust
 * `oxy publish` flag for flag; what it adds:
 *
 *   --build-only   build and bundle, stop before anything needs a credential
 *   --prebuilt     with --dir: the functions are already bundled — check, don't rebuild
 *   --json         the server's result, warnings included, on stdout
 *
 * and trusted publishing: in a GitHub Actions job granted `id-token: write`
 * with no token set, it exchanges the job's OIDC token for a credential scoped
 * to this one app.
 *
 * `--build-only` and `--prebuilt` exist for CI that keeps the credential out of
 * the job that runs package scripts: build there, publish the artifact in a job
 * that installs nothing.
 */

import { spawnSync } from "node:child_process";
import { join, resolve, sep } from "node:path";

import type { Context } from "../context/resolve.js";
import { loadDotenv } from "../publish/dotenv.js";
import {
  bundleFunctions,
  enforceReservedFunctionsDir,
  requirePrebuiltFunctions,
  validateFunctionNames
} from "../publish/functions.js";
import {
  buildSteps,
  declaredFunctions,
  functionEntry,
  isValidSlug,
  loadPublishManifest,
  type PublishManifest
} from "../publish/manifest.js";
import {
  captureGitSource,
  GAP_MESSAGES,
  isRecorded,
  provenanceGaps,
  resolveBuildId,
  sanitizeRemoteUrl,
  worktreeIsDirty
} from "../publish/provenance.js";
import {
  exchangeGithubOidc,
  fetchOrgForProject,
  fetchProject,
  githubOidcAvailable,
  type PublishResult,
  uploadBundle
} from "../publish/server.js";
import { tarGzDir } from "../publish/tarball.js";
import * as log from "../ui/log.js";
import { out } from "../ui/tty.js";
import { authError, CliError, ExitCode, usageError } from "../util/errors.js";

export interface PublishFlags {
  app?: string;
  buildId?: string;
  dir?: string;
  promote?: boolean;
  name?: string;
  repo?: string;
  commit?: string;
  branch?: string;
  buildOnly?: boolean;
  prebuilt?: boolean;
  json?: boolean;
}

function envValue(name: string): string | undefined {
  return process.env[name]?.trim() || undefined;
}

/** `(org, app)` from a working directory shaped `…/apps/<org>/<app>[/…]`. */
export function inferOrgApp(cwd: string): { org?: string; app?: string } {
  const parts = cwd.split(sep).filter(Boolean);
  const index = parts.lastIndexOf("apps");
  if (index < 0) return {};
  return { org: parts[index + 1], app: parts[index + 2] };
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** How this publish will authenticate, decided before anything is built. */
type Credential = { kind: "token"; token: string } | { kind: "oidc" };

function resolveCredential(ctx: Context): Credential {
  const token = ctx.maybeBearer();
  if (token) return { kind: "token", token };
  if (githubOidcAvailable()) return { kind: "oidc" };
  throw authError(ctx.target(), ctx.flags.env ?? "production", ctx.flags.tokenEnv ?? "OXY_TOKEN");
}

/** One manifest build step, output to stderr so stdout stays the result. */
function runBuildStep(label: string, command: string, cwd: string, basePath: string): void {
  process.stderr.write(`[${label}] $ ${command}\n`);
  const result = spawnSync("sh", ["-c", command], {
    cwd,
    stdio: ["inherit", 2, 2],
    env: { ...process.env, OXY_APP_BASE_PATH: basePath }
  });
  if (result.error) throw new CliError(`failed to start \`${command}\`: ${result.error.message}`);
  if (result.status !== 0) {
    throw new CliError(`build step \`${command}\` failed (exit ${result.status ?? "signal"})`);
  }
}

interface Identity {
  org?: string;
  app: string;
}

/** flag → env → manifest → directory, for each half. */
function resolveIdentity(ctx: Context, flags: PublishFlags, manifest?: PublishManifest): Identity {
  const inferred = inferOrgApp(ctx.cwd);
  const org = ctx.flags.org ?? envValue("OXY_ORG") ?? manifest?.orgSlug ?? inferred.org;
  const app = flags.app ?? envValue("OXY_APP") ?? manifest?.slug ?? inferred.app;
  if (!app) throw usageError("missing app", "set oxy-app.json `slug`, --app, or OXY_APP");
  if (!isValidSlug(app)) {
    throw usageError(
      `invalid app slug ${JSON.stringify(app)}`,
      "1-63 lowercase letters, digits and single hyphens — no leading, trailing or double hyphen, no underscore"
    );
  }
  return { org, app };
}

/** Build from source (unless --dir), then check and bundle `functions/`. */
function prepareBundle(
  ctx: Context,
  flags: PublishFlags,
  manifest: PublishManifest | undefined,
  identity: Identity
): string {
  const declared = declaredFunctions(manifest);
  validateFunctionNames(declared);

  let bundleDir: string;
  if (flags.dir) {
    bundleDir = resolve(ctx.cwd, flags.dir);
  } else {
    if (!identity.org) {
      throw usageError(
        "missing org: the app's base path needs it",
        "set oxy-app.json `orgSlug`, --org, OXY_ORG, or --project (a workspace determines its org), or publish a pre-built bundle with --dir"
      );
    }
    const steps = buildSteps(manifest);
    const basePath = `/customer-apps/${identity.org}/${identity.app}/`;
    runBuildStep("install", steps.install, ctx.cwd, basePath);
    runBuildStep("build", steps.command, ctx.cwd, basePath);
    bundleDir = join(ctx.cwd, steps.outDir);
  }

  const warning = enforceReservedFunctionsDir(bundleDir, declared, declared.length > 0);
  if (warning) log.warn(warning);

  if (flags.prebuilt) {
    requirePrebuiltFunctions(bundleDir, declared);
  } else {
    const entries = declared.map((name) => ({ name, entry: functionEntry(manifest, name) }));
    bundleFunctions(entries, ctx.cwd, bundleDir);
  }
  return bundleDir;
}

interface Provenance {
  repo?: string;
  commit?: string;
  branch?: string;
}

/** Flags win, then the checkout, then the CI variables; every gap is a warning. */
function resolveProvenance(cwd: string, flags: PublishFlags): Provenance {
  const git = captureGitSource(cwd);
  const repoSource = flags.repo ?? git.repo;
  const provenance = {
    repo: repoSource === undefined ? undefined : sanitizeRemoteUrl(repoSource),
    commit: flags.commit ?? git.commit ?? envValue("GITHUB_SHA"),
    branch: flags.branch ?? git.branch ?? envValue("GITHUB_REF_NAME")
  };
  // Only ask git about dirt when there is a recorded commit for it to contradict.
  const dirty = isRecorded(provenance.commit) ? worktreeIsDirty(cwd) : undefined;
  for (const gap of provenanceGaps(dirty, provenance.commit, provenance.repo)) {
    log.warn(GAP_MESSAGES[gap]);
  }
  return provenance;
}

/** The credential for the upload itself — the OIDC exchange happens here, last. */
async function uploadToken(
  ctx: Context,
  credential: Credential,
  identity: Identity
): Promise<string> {
  if (credential.kind === "token") return credential.token;
  if (!identity.org || UUID.test(identity.org)) {
    throw usageError(
      "trusted publishing needs the org SLUG",
      "set oxy-app.json `orgSlug` or pass --org <slug> — the exchange is registered by slug"
    );
  }
  log.info("exchanging the GitHub OIDC token for a publish credential");
  return exchangeGithubOidc(ctx.target(), identity.org, identity.app);
}

function printResult(
  target: string,
  identity: Identity,
  result: PublishResult,
  asJson: boolean
): void {
  for (const warning of result.warnings ?? []) log.warn(warning);
  if (asJson) {
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
    return;
  }
  const org = result.org_slug ?? identity.org ?? "";
  const headline = result.is_new_app
    ? `Registered new app ${org}/${identity.app} (id ${result.app_id})`
    : `Published new version of ${org}/${identity.app} (id ${result.app_id})`;
  process.stdout.write(
    `${out.green(headline)}\n` +
      `  build ${result.build_id} → ${result.channel} channel · ${target}${result.url}\n`
  );
}

export async function runPublish(ctx: Context, flags: PublishFlags): Promise<void> {
  if (flags.prebuilt && !flags.dir) {
    throw usageError("--prebuilt needs --dir", "it names a bundle that was built elsewhere");
  }
  if (flags.prebuilt && flags.buildOnly) {
    throw usageError("--prebuilt and --build-only together do nothing", "drop one");
  }

  for (const path of loadDotenv(ctx.cwd)) log.info(`loaded ${path}`);
  const manifest = loadPublishManifest(ctx.cwd);
  const identity = resolveIdentity(ctx, flags, manifest);
  const projectPin = ctx.flags.project ?? envValue("OXY_PROJECT");

  // Decided before the build, so a missing login fails in a second rather
  // than after a two-minute install.
  const credential = flags.buildOnly ? undefined : resolveCredential(ctx);
  if (!identity.org && projectPin) {
    identity.org = await fetchOrgForProject(ctx.target(), projectPin);
  }

  const bundleDir = prepareBundle(ctx, flags, manifest, identity);
  if (flags.buildOnly || !credential) {
    if (flags.json) process.stdout.write(`${JSON.stringify({ bundle_dir: bundleDir }, null, 2)}\n`);
    else process.stdout.write(`${out.green("built")} ${bundleDir}\n`);
    return;
  }

  const target = ctx.target();
  let project = projectPin;
  if (!project) {
    if (!identity.org) {
      throw usageError(
        "missing org: needed to look up the project",
        "set oxy-app.json `orgSlug`, --org or OXY_ORG, or pass --project <uuid>"
      );
    }
    project = await fetchProject(target, identity.org, identity.app);
  }

  const buildId = resolveBuildId(flags.buildId);
  const provenance = resolveProvenance(ctx.cwd, flags);
  const tarball = tarGzDir(bundleDir);
  const channel = flags.promote ? "published" : "draft";
  const who = identity.org
    ? `${identity.org}/${identity.app}`
    : `${identity.app} → workspace ${project}`;
  log.info(`publishing ${who} (${tarball.length} bytes) → ${target} [${channel}]`);

  const token = await uploadToken(ctx, credential, identity);
  let result: PublishResult;
  try {
    result = await uploadBundle({
      target,
      token,
      tarball,
      fields: [
        ["app", identity.app],
        ["project", project],
        ["build_id", buildId],
        ["channel", channel],
        // Optional: a pre-built bundle pinned to a project lets the server infer it.
        ["org", identity.org],
        ["name", flags.name],
        ["source_repo", provenance.repo],
        ["commit_sha", provenance.commit],
        ["branch", provenance.branch]
      ]
    });
  } catch (cause) {
    if (cause instanceof CliError && cause.code === ExitCode.AUTH && credential.kind === "token") {
      throw new CliError(cause.message, {
        code: cause.code,
        detail: cause.detail,
        hint: `publishing needs app-admin rights on the org — \`oxyc whoami --env ${ctx.flags.env ?? "production"}\` shows yours`
      });
    }
    throw cause;
  }
  printResult(target, identity, result, Boolean(flags.json));
}
