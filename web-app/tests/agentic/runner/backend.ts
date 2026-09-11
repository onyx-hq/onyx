import { type ChildProcess, spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import type { BackendMode } from "./types";

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "..");
// `oxy start --local` always serves `demo_project/`. Flows that need a
// different fixture must commit a local file (DuckDB / Parquet / CSV)
// and reference it from inside `demo_project` — there is intentionally
// no env override so fixtures cannot point at an external warehouse.
const PROJECT_DIR = resolve(REPO_ROOT, "demo_project");
const LOG_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..", ".logs");
const STARTUP_TIMEOUT_MS = 240_000;
const POLL_INTERVAL_MS = 1_000;

// Pitfall #1: a stale `oxy` on PATH (often older than the workspace build)
// can refuse flags the workspace expects. Resolve in this order:
//   1. $OXY_BIN (explicit override)
//   2. <repo>/target/debug/oxy (fresh workspace build)
//   3. `oxy` on PATH
const DEBUG_OXY = resolve(REPO_ROOT, "target", "debug", "oxy");
const OXY_BIN =
  process.env.OXY_BIN && existsSync(process.env.OXY_BIN)
    ? process.env.OXY_BIN
    : existsSync(DEBUG_OXY)
      ? DEBUG_OXY
      : "oxy";

export interface BackendHandle {
  url: string;
  spawned: boolean;
  shutdown: () => Promise<void>;
}

export interface BackendOptions {
  /**
   * Which oxy backend mode to bring up. `local` spawns
   * `oxy start --local --enterprise` from `demo_project/` against the
   * auth-disabled public port (3000). `cloud` spawns
   * `oxy start --enterprise --clean` (multi-tenant, fresh postgres) and
   * the runner drives the auth-disabled internal port (3001).
   */
  mode: BackendMode;
}

/**
 * Public URL the runner should drive. `OXY_BASE_URL` overrides; otherwise
 * we pick by mode (local → :3000, cloud → :3001).
 */
export function resolveBaseUrl(mode: BackendMode): string {
  return process.env.OXY_BASE_URL ?? defaultBaseUrl(mode);
}

/**
 * Health-check URL. `OXY_HEALTH_URL` overrides; otherwise derive from the
 * resolved base URL.
 */
export function resolveHealthUrl(mode: BackendMode): string {
  return process.env.OXY_HEALTH_URL ?? `${resolveBaseUrl(mode)}/api/health`;
}

/**
 * Scope a flow's `goto:` path to a workspace when the target deployment needs
 * one.
 *
 * A flow says `goto:/automations` because that is what the surface is called.
 * In the single-workspace `--local` backend that path resolves directly; in a
 * cloud deployment the same surface lives under
 * `/<org>/workspaces/<workspace-id>/automations`, and the bare path silently
 * lands on the org home instead. The flow is not wrong — the deployment shape
 * is a property of where the run points, so the prefix belongs here next to
 * `OXY_BASE_URL`, not duplicated into every YAML file.
 *
 * Bare `/` is left alone deliberately: it already means "app root, route me",
 * which every deployment handles, and prefixing it would change the entry
 * point of flows that pass today.
 *
 * Every membership test below runs against the PATH ONLY, with any query string
 * or fragment stripped first. Comparing the whole target instead is a bug that
 * shipped here and cost a flow: `admin-airhouse-fleet`'s setup is
 * `goto:/dev-login?email=…&next=/admin/airhouse`, which is neither `/dev-login`
 * nor a `/dev-login/…` and so escaped the top-level list and got prefixed into
 * `/local/workspaces/<id>/dev-login?…`. The SPA rendered its fallback and the
 * flow timed out on its first locator after 30 seconds, reading exactly like a
 * broken admin page. Covered by backend.test.ts.
 */
export function applyPathPrefix(target: string): string {
  const prefix = process.env.OXY_PATH_PREFIX?.replace(/\/+$/, "");
  if (!prefix) return target;
  if (!target.startsWith("/")) return target; // absolute URL — caller means it
  if (target === "/") return target;
  const path = target.split(/[?#]/, 1)[0];
  if (path === prefix || path.startsWith(`${prefix}/`)) return target;
  if (TOP_LEVEL_SURFACES.some((p) => path === p || path.startsWith(`${p}/`))) return target;
  return `${prefix}${target}`;
}

/**
 * Surfaces that are NOT workspace-scoped, so the prefix must not reach them.
 *
 * The prefix exists because `goto:/automations` means a different URL in a
 * cloud deployment than in `--local`. But `/admin/workspace-health` means the
 * SAME url in both — it hangs off the app root, not off a workspace. Prefixing
 * it produces `/<org>/workspaces/<id>/admin/workspace-health`, which routes
 * nowhere; the SPA renders its fallback and the flow times out waiting for a
 * testid that was never going to appear. Three admin flows failed exactly that
 * way before this list existed, and none of the failures looked like a routing
 * problem — they looked like three unrelated broken pages.
 */
const TOP_LEVEL_SURFACES = [
  "/admin",
  "/partners",
  "/customer-apps",
  "/login",
  "/dev-login",
  "/invite",
  "/cli-auth"
];

function defaultBaseUrl(mode: BackendMode): string {
  return mode === "cloud" ? "http://localhost:3001" : "http://localhost:3000";
}

export async function ensureBackend(opts: BackendOptions): Promise<BackendHandle> {
  const healthUrl = resolveHealthUrl(opts.mode);
  if (await isHealthy(healthUrl, 5_000)) {
    console.log(`[backend] using running backend at ${healthUrl}`);
    return { url: healthUrl, spawned: false, shutdown: async () => {} };
  }

  const args = spawnArgs(opts.mode);
  const cwd = opts.mode === "cloud" ? REPO_ROOT : PROJECT_DIR;
  console.log(
    `[backend] not reachable at ${healthUrl}; starting \`${OXY_BIN} ${args.join(" ")}\` from ${cwd}`
  );
  if (opts.mode === "cloud") {
    console.log(
      "[backend] cloud mode: --clean wipes the local oxy postgres volume — start + `oxy seed` a backend yourself if the flow needs data"
    );
  }
  mkdirSync(LOG_DIR, { recursive: true });
  const logPath = resolve(LOG_DIR, "backend.log");
  const out = openSync(logPath, "a");
  const proc = spawn(OXY_BIN, args, {
    cwd,
    env: process.env,
    stdio: ["ignore", out, out],
    detached: false
  });

  // Race spawn against process exit so a misconfigured invocation surfaces
  // the actual error from the log instead of a 4-minute health-check timeout.
  const earlyExit = new Promise<never>((_, reject) => {
    proc.once("exit", (code, signal) => {
      reject(new Error(`oxy start exited early (code=${code} signal=${signal}) — see ${logPath}`));
    });
  });
  let ready: boolean;
  try {
    ready = await Promise.race([waitForHealthy(healthUrl, STARTUP_TIMEOUT_MS), earlyExit]);
  } catch (err) {
    proc.kill("SIGTERM");
    throw err;
  }
  if (!ready) {
    proc.kill("SIGTERM");
    throw new Error(
      `backend did not become healthy within ${STARTUP_TIMEOUT_MS}ms — see ${logPath}`
    );
  }

  console.log(`[backend] healthy after spawn (logs: ${logPath})`);

  return {
    url: healthUrl,
    spawned: true,
    shutdown: () => shutdownProc(proc)
  };
}

function spawnArgs(mode: BackendMode): string[] {
  if (mode === "cloud") {
    // No `--local`. `--clean` gives every run the same empty Postgres: no
    // org exists and no UI path creates one (orgs are staff/partner-made), so
    // a flow that needs org data runs against a backend started and
    // `oxy seed`ed beforehand, which ensureBackend reuses. Cloud-mode flows
    // drive the auth-disabled internal port (3001), which `oxy start` exposes
    // by default; the public 3000 port has magic-link auth that the test
    // runner can't drive.
    return ["start", "--enterprise", "--clean"];
  }
  return ["start", "--local", "--enterprise"];
}

async function isHealthy(url: string, timeoutMs: number): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(timeoutMs) });
    return res.ok;
  } catch {
    return false;
  }
}

async function waitForHealthy(url: string, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await isHealthy(url, 2_000)) return true;
    await sleep(POLL_INTERVAL_MS);
  }
  return false;
}

async function shutdownProc(proc: ChildProcess): Promise<void> {
  if (proc.exitCode !== null || proc.killed) return;
  proc.kill("SIGTERM");
  await Promise.race([new Promise<void>((r) => proc.once("exit", () => r())), sleep(5_000)]);
  if (proc.exitCode === null && !proc.killed) proc.kill("SIGKILL");
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
