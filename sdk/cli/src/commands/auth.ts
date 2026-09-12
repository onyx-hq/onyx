/**
 * `oxyc login`, `logout`, `whoami`, `token`.
 *
 * These write the SAME file the Rust `oxy` binary uses, so logging in with
 * either tool authenticates both. That is the whole reason `oxyc` can be
 * installed on its own: a developer with only `npx @oxy-hq/cli` is not a
 * second-class citizen, and one with both never logs in twice.
 */

import { parseJson, request } from "../api/request.js";
import { clearCredential, loadCredential, resolveBearer } from "../auth/credentials.js";
import { adminStatusLine, login } from "../auth/login.js";
import type { Context } from "../context/resolve.js";
import * as log from "../ui/log.js";
import { out } from "../ui/tty.js";
import { CliError, ExitCode } from "../util/errors.js";
import { runAssumeStart } from "./assume.js";

/**
 * Log into one deployment, or several.
 *
 * MULTI-ENV IS SEQUENTIAL, not concurrent: each one opens a browser and waits
 * for its callback, and two tabs racing for the same loopback port is a login
 * that fails for a reason nobody can read. The Rust `oxy login --env dev,staging`
 * did the same, and its logins are still in the same credential file.
 *
 * A FAILURE DOES NOT ABANDON THE REST. Logging into dev and staging is a
 * sequence of independent acts, and stopping at the first would leave you
 * having typed a browser flow for nothing. Each is reported as it lands, and
 * the exit code is non-zero if any failed.
 */
export async function runLogin(
  ctx: Context,
  envs: string[],
  assume?: { org?: string; reason: string }
): Promise<void> {
  const targets = envs.length > 0 ? envs.map((e) => ctx.withEnv(e)) : [ctx];
  const failures: string[] = [];

  for (const one of targets) {
    const target = one.target();
    try {
      const { user } = await login(target);
      process.stderr.write(`${out.green(`Logged in as ${user.email} (${target}).`)}\n`);
      process.stderr.write(`${adminStatusLine(user)}\n`);
    } catch (cause) {
      failures.push(target);
      log.warn(`could not log into ${target}: ${(cause as Error).message}`);
      continue;
    }

    // Single-target by construction — `main.ts` refuses the multi-env case
    // before any browser opens, which is where a usage error belongs.
    if (assume) {
      await runAssumeStart(one, assume.org, assume.reason);
    }
  }

  if (failures.length > 0) {
    throw new CliError(`could not log into ${failures.length} of ${targets.length} deployment(s)`, {
      code: ExitCode.FAILURE,
      detail: failures.join("\n")
    });
  }
}

export function runLogout(ctx: Context): void {
  const target = ctx.target();
  if (clearCredential(target)) {
    process.stderr.write(`${out.green(`Logged out of ${target}.`)}\n`);
    return;
  }
  log.info(`no cached credential for ${target}`);
}

/**
 * Who this token is, and what it can reach.
 *
 * Deliberately makes a live call rather than printing the cached email: the
 * cached value is what was true at login, and the failure this command exists
 * to diagnose — an expired token, a revoked grant, a missing assume session —
 * is invisible in the cache. A `whoami` that reads a file cannot tell you the
 * token stopped working.
 */
export async function runWhoami(ctx: Context, json: boolean): Promise<void> {
  const target = ctx.target();
  const bearer = ctx.bearer();

  const response = await request({
    target,
    path: "/api/user",
    method: "GET",
    bearer,
    timeoutMs: 30_000
  });
  if (response.status === 401 || response.status === 403) {
    throw new CliError(`the cached token for ${target} is no longer accepted`, {
      code: ExitCode.AUTH,
      hint: `oxyc login --env ${ctx.flags.env ?? "production"}`
    });
  }
  if (response.status < 200 || response.status >= 300) {
    throw new CliError(`could not read /api/user (${response.status})`, {
      code: ExitCode.UNAVAILABLE
    });
  }

  const payload = parseJson(response.body);

  // A 200 whose body is `null` is the shape an EXPIRED token produces here:
  // the request is accepted, no user resolves, and the server says so with a
  // null rather than a 401. Reporting that as success — falling back to the
  // cached email, which is still sitting in the credentials file — is the
  // precise failure this command exists to catch, and it is what the first
  // run of this code did. `login.rs` refuses the same shape at login time.
  if (payload === null || payload === undefined) {
    throw new CliError(`the token for ${target} no longer resolves to a user`, {
      code: ExitCode.AUTH,
      detail:
        "GET /api/user answered 200 with a null body, which is what an expired session looks like.",
      hint: `oxyc login --env ${ctx.flags.env ?? "production"}`
    });
  }

  if (json) {
    process.stdout.write(`${response.body.trim()}\n`);
    return;
  }

  const user = payload as Record<string, unknown>;
  const cached = loadCredential(target);
  const lines = [
    `${out.bold("target")}      ${target}`,
    `${out.bold("email")}       ${String(user.email ?? cached?.email ?? "unknown")}`,
    `${out.bold("app admin")}   ${user.is_app_admin ? "yes" : "no"}`
  ];
  if (typeof user.id === "string") lines.push(`${out.bold("user id")}     ${user.id}`);
  const customer = ctx.customer();
  if (customer)
    lines.push(`${out.bold("customer")}    ${customer.name}  (from the repo you are in)`);
  process.stdout.write(`${lines.join("\n")}\n`);
}

/**
 * Print the bearer, for a raw `curl`.
 *
 * It exists because the alternative is people copying tokens out of the
 * credentials file by hand, which is worse in every way — including that they
 * then paste the wrong host's.
 */
export function runToken(ctx: Context): void {
  const target = ctx.target();
  const token = resolveBearer(target, ctx.flags.tokenEnv ?? "OXY_TOKEN");
  if (!token) {
    throw new CliError(`not authenticated for ${target}`, {
      code: ExitCode.AUTH,
      hint: `oxyc login --env ${ctx.flags.env ?? "production"}`
    });
  }
  process.stdout.write(`${token}\n`);
}
