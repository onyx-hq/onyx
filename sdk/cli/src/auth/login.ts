/**
 * The browser loopback login — the same dance the Rust `oxy login` did, wire for wire.
 *
 * Bind an ephemeral `127.0.0.1` port, send the browser to
 * `<target>/cli-auth?port&state`, and catch the token the web app hands back to
 * `/callback`. The `/cli-auth` page reads the token from the already-logged-in
 * session, so there is no new minting endpoint and no password anywhere.
 *
 * Every constant here — the callback path, the two query parameter names, the
 * `state` check, the five-minute deadline — was transcribed from
 * the Rust `login.rs` (since deleted). It is a protocol shared with a page
 * we do not change in this repo, so drifting from it is a silent
 * "login hangs forever", not a compile error.
 */

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
import * as log from "../ui/log.js";
import { err } from "../ui/tty.js";
import { CliError, ExitCode } from "../util/errors.js";
import { type HostCredential, saveCredential } from "./credentials.js";

/** What `/api/user` gives back. Only the two fields the login flow reports. */
interface UserResponse {
  email?: string;
  is_app_admin?: boolean;
}

/** The tab the user is left looking at. */
const SUCCESS_HTML =
  "<!doctype html><meta charset=utf-8><title>oxyc login</title>" +
  '<body style="font-family:system-ui;padding:3rem;text-align:center">' +
  "<h2>Logged in to oxy ✓</h2><p>You can close this tab and return to your terminal.</p>";

/** The Rust waits 5 minutes. Long enough for an SSO detour with an MFA prompt. */
const LOGIN_TIMEOUT_MS = 300_000;

/**
 * Run the loopback flow against one target and cache the result.
 *
 * Returns the token as well as storing it, so a caller chaining straight into
 * an assume-role session does not have to read the file it just wrote.
 */
export async function login(target: string): Promise<{ token: string; user: HostCredential }> {
  const state = randomUUID();
  const { port, waitForToken, close } = await startLoopback(state);

  try {
    const authUrl = `${target.replace(/\/+$/, "")}/cli-auth?port=${port}&state=${encodeURIComponent(state)}`;
    log.info(`Opening ${authUrl} in your browser to log in…`);
    log.info("If it doesn't open automatically, paste that URL into your browser.");
    openBrowser(authUrl);

    const token = await waitForToken;
    const user = await fetchUser(target, token);

    const credential: HostCredential = {
      token,
      email: user.email ?? "",
      is_app_admin: Boolean(user.is_app_admin)
    };
    saveCredential(target, credential);
    return { token, user: credential };
  } finally {
    close();
  }
}

/**
 * The loopback listener.
 *
 * It keeps accepting until it sees a callback whose `state` matches, rather
 * than resolving on the first request: a browser will cheerfully send a
 * `/favicon.ico` alongside the redirect, and treating that as the callback
 * would fail a login that was actually about to succeed.
 */
async function startLoopback(expectedState: string): Promise<{
  port: number;
  waitForToken: Promise<string>;
  close: () => void;
}> {
  let resolveToken!: (token: string) => void;
  let rejectToken!: (reason: Error) => void;
  const waitForToken = new Promise<string>((resolve, reject) => {
    resolveToken = resolve;
    rejectToken = reject;
  });

  const server = createServer((req, res) => {
    // `req.url` is a path+query, so it needs a base to parse. The base is
    // discarded — only the path and the query matter.
    const parsed = new URL(req.url ?? "/", "http://localhost");
    if (parsed.pathname !== "/callback") {
      plain(res, 404, "Not found");
      return;
    }
    if (parsed.searchParams.get("state") !== expectedState) {
      // A mismatch means this callback belongs to some other login attempt —
      // or to something that guessed the port. Refuse and keep listening.
      plain(res, 400, "State mismatch — please retry `oxyc login`.");
      return;
    }
    const token = parsed.searchParams.get("token");
    if (!token) {
      plain(res, 400, "No token in callback.");
      return;
    }
    res.writeHead(200, {
      "Content-Type": "text/html; charset=utf-8",
      Connection: "close"
    });
    res.end(SUCCESS_HTML);
    resolveToken(token);
  });

  const timer = setTimeout(() => {
    rejectToken(
      new CliError("timed out waiting for the browser to complete login (5 min)", {
        code: ExitCode.UNAVAILABLE,
        hint: "oxyc login --env <env>   — and complete the browser flow"
      })
    );
  }, LOGIN_TIMEOUT_MS);
  // Do not hold the process open on the timer alone; the promise decides.
  timer.unref?.();

  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });

  const address = server.address() as AddressInfo | null;
  if (!address) {
    server.close();
    throw new CliError("could not bind loopback port", { code: ExitCode.UNAVAILABLE });
  }

  return {
    port: address.port,
    waitForToken,
    close: () => {
      clearTimeout(timer);
      server.close();
    }
  };
}

function plain(res: import("node:http").ServerResponse, status: number, body: string): void {
  res.writeHead(status, {
    "Content-Type": "text/plain; charset=utf-8",
    Connection: "close"
  });
  res.end(body);
}

/**
 * Confirm the token and find out who it belongs to.
 *
 * Not merely informational: it is the difference between "we captured a
 * string" and "the server accepts it". A token cached without this check
 * fails later, at some unrelated command, with a 401 nobody connects back to
 * the login.
 */
async function fetchUser(target: string, token: string): Promise<UserResponse> {
  const url = `${target.replace(/\/+$/, "")}/api/user`;
  let response: Response;
  try {
    response = await fetch(url, {
      headers: { Authorization: `Bearer ${token}` },
      signal: AbortSignal.timeout(30_000)
    });
  } catch (cause) {
    throw new CliError(`GET ${url} failed: ${(cause as Error).message}`, {
      code: ExitCode.UNAVAILABLE
    });
  }
  if (!response.ok) {
    throw new CliError(`login token rejected by ${url} (${response.status})`, {
      code: ExitCode.AUTH
    });
  }
  const body = (await response.json()) as UserResponse | null;
  if (!body) {
    throw new CliError("login token did not resolve to a user (got null)", {
      code: ExitCode.AUTH
    });
  }
  return body;
}

/** Best-effort browser open. A failure is fine — the URL was already printed. */
function openBrowser(url: string): void {
  const [bin, args] =
    process.platform === "darwin"
      ? (["open", [url]] as const)
      : process.platform === "win32"
        ? (["cmd", ["/C", "start", "", url]] as const)
        : (["xdg-open", [url]] as const);
  try {
    spawn(bin, [...args], { stdio: "ignore", detached: true }).unref();
  } catch {
    // The URL is on stderr already; a machine with no browser is a valid
    // place to run this, and failing the login over it would be wrong.
  }
}

/** The line `login` prints about publish rights, shared with `whoami`. */
export function adminStatusLine(credential: HostCredential): string {
  return credential.is_app_admin
    ? err.green("Global admin: yes — you can publish.")
    : err.yellow(
        "Global admin: no — you can't publish yet. Ask #platform to add you to OXY_GLOBAL_ADMINS."
      );
}
