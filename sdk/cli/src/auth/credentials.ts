/**
 * The token cache — the SAME file the deleted Rust `oxy login` wrote.
 *
 * It began as interop with a program written in another language, and the
 * shape is still that contract: every login cached before `oxyc` took over
 * lives in this file, so the path, the host key and the field names were
 * transcribed from the Rust `login.rs` rather than chosen here. Change any of
 * them and every existing login silently disappears.
 *
 * THE PATH IS NOT `~/.config` ON macOS, whatever the Rust doc comment said.
 * `login.rs` built it from `dirs::config_dir()`, and that crate returns
 * `$HOME/Library/Application Support` on macOS — verified against a live
 * credentials file. A `~/.config/oxy` implementation would read an empty store
 * on every Mac in the company and report every developer as logged out.
 * `configDir()` reproduces the crate's platform rules, and
 * `credentials.test.ts` pins them.
 */

import { chmodSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { CliError, ExitCode } from "../util/errors.js";

/** One host's cached credential. Field names are the Rust struct's, verbatim. */
export interface HostCredential {
  token: string;
  email: string;
  is_app_admin: boolean;
}

/** The file: a flat map of host key to credential. No envelope, no version. */
export type CredentialStore = Record<string, HostCredential>;

/**
 * `dirs::config_dir()`, reimplemented.
 *
 * Deliberately not `env-paths` or any other convenience package: those pick
 * their own conventions (`~/Library/Preferences`, an app suffix), and being
 * *conventional* is worth nothing here — being byte-identical to what the Rust
 * crate returns is worth everything.
 */
export function configDir(): string {
  if (process.platform === "darwin") {
    return join(homedir(), "Library", "Application Support");
  }
  if (process.platform === "win32") {
    const appData = process.env.APPDATA;
    if (appData) return appData;
    return join(homedir(), "AppData", "Roaming");
  }
  const xdg = process.env.XDG_CONFIG_HOME;
  if (xdg?.startsWith("/")) return xdg;
  return join(homedir(), ".config");
}

/** The credentials file. `OXY_CREDENTIALS_PATH` overrides it, for tests. */
export function credentialsPath(): string {
  return process.env.OXY_CREDENTIALS_PATH ?? join(configDir(), "oxy", "credentials.json");
}

/**
 * The cache key for a target: `host` or `host:port`.
 *
 * Transcribed from `login.rs::host_key`. The port is part of the key because
 * a laptop routinely holds a `localhost:5173` token (the Vite dev server) and
 * a `localhost:3000` one (oxy itself) that are not interchangeable. An
 * unparseable target falls back to itself with a trailing slash trimmed —
 * same as the Rust, so a malformed value at least collides consistently.
 */
export function hostKey(target: string): string {
  try {
    const url = new URL(target);
    return url.port ? `${url.hostname}:${url.port}` : url.hostname;
  } catch {
    return target.replace(/\/+$/, "");
  }
}

/**
 * The whole store, or an empty one.
 *
 * Unreadable and unparseable both degrade to empty rather than throwing: a
 * missing file is the ordinary state on a fresh machine, and the caller's next
 * move — say "not authenticated, run oxyc login" — is the right answer to a
 * corrupt file too. What is NOT acceptable is silently *writing over* it; see
 * `writeStore`.
 */
export function readStore(): CredentialStore {
  try {
    const raw = readFileSync(credentialsPath(), "utf8");
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as CredentialStore;
  } catch {
    return {};
  }
}

/**
 * Write the store back, atomically, owner-only.
 *
 * tmp-then-rename because the file holds live bearer tokens for every host you
 * have ever logged into: a partial write during `oxyc login --env dev` would
 * take production's token with it. `0600` matches what `login.rs` sets, and
 * for the same reason — a world-readable token file on a shared machine is a
 * credential leak with no symptom.
 */
export function writeStore(store: CredentialStore): void {
  const path = credentialsPath();
  const dir = dirname(path);
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  // THE TEMP FILE IS A SIBLING OF THE TARGET, not something in `os.tmpdir()`.
  // `rename(2)` cannot cross a filesystem boundary, and `/tmp` is a separate
  // mount on plenty of machines (a tmpfs on most Linux distributions, a
  // separate volume in many container images) — from there this throws a raw
  // `EXDEV` and `oxyc login` dies on a Node stack trace with the token already
  // captured. A sibling is on the same filesystem by construction.
  //
  // The unit test cannot see this: it points OXY_CREDENTIALS_PATH inside
  // tmpdir(), so both paths were on the same mount there either way.
  const tmp = join(dir, `.credentials.${process.pid}.${Date.now()}.tmp`);
  try {
    writeFileSync(tmp, `${JSON.stringify(store, null, 2)}\n`, { mode: 0o600 });
    renameSync(tmp, path);
  } catch (cause) {
    try {
      rmSync(tmp, { force: true });
    } catch {
      // Nothing more to do; the throw below is the report.
    }
    throw new CliError(`could not write ${path}`, {
      code: ExitCode.FAILURE,
      detail: (cause as Error).message,
      hint: "check the directory is writable, or set OXY_CREDENTIALS_PATH"
    });
  }
  try {
    chmodSync(path, 0o600);
  } catch {
    // Best-effort: a filesystem without POSIX modes (a Windows share) still
    // gets the file. Failing the login over the mode would be worse.
  }
}

/** The cached token for `target`, if there is a non-empty one. */
export function loadToken(target: string): string | undefined {
  const entry = readStore()[hostKey(target)];
  const token = entry?.token?.trim();
  return token ? token : undefined;
}

/** The whole cached credential for `target` — token plus who it belongs to. */
export function loadCredential(target: string): HostCredential | undefined {
  return readStore()[hostKey(target)];
}

/** Cache `credential` under `target`, leaving every other host untouched. */
export function saveCredential(target: string, credential: HostCredential): void {
  const store = readStore();
  store[hostKey(target)] = credential;
  writeStore(store);
}

/** Drop `target`'s credential. Returns whether there was one to drop. */
export function clearCredential(target: string): boolean {
  const store = readStore();
  const key = hostKey(target);
  if (!(key in store)) return false;
  delete store[key];
  writeStore(store);
  return true;
}

/**
 * The bearer for `target`, by the same precedence the Rust CLI uses:
 * the env var first (the CI path), then the login cache.
 *
 * Returns `undefined` rather than throwing so the caller can decide whether
 * missing auth is fatal — `oxyc routes` against a cached catalog is not.
 */
export function resolveBearer(target: string, tokenEnv = "OXY_TOKEN"): string | undefined {
  const fromEnv = process.env[tokenEnv]?.trim();
  if (fromEnv) return fromEnv;
  return loadToken(target);
}
