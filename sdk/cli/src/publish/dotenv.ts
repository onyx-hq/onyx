/**
 * `.env.local`, then `.env`, loaded into `process.env` without overriding
 * anything already set — so a laptop publish sees what the shell would, and
 * CI's real environment always wins.
 *
 * Each file is looked for in the working directory and then every parent,
 * nearest first, which is how the Rust `dotenv` crate found them: an app under
 * `apps/<org>/<app>/` picks up a repo-root `.env`.
 *
 * `${VAR}` expansion is NOT supported. Nothing a publish reads has needed it,
 * and a half-implemented expander is worse than an honest literal.
 */

import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";

/** The nearest `name` at or above `dir`. */
function findUp(dir: string, name: string): string | undefined {
  let current = dir;
  for (;;) {
    const candidate = join(current, name);
    if (existsSync(candidate)) return candidate;
    const parent = dirname(current);
    if (parent === current) return undefined;
    current = parent;
  }
}

/** `KEY=value` lines into pairs. Comments, blanks and malformed lines are skipped. */
export function parseDotenv(text: string): Array<[string, string]> {
  const pairs: Array<[string, string]> = [];
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim().replace(/^export\s+/, "");
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq).trim();
    if (!/^[A-Za-z_][A-Za-z0-9_.]*$/.test(key)) continue;
    pairs.push([key, parseValue(line.slice(eq + 1).trim())]);
  }
  return pairs;
}

function parseValue(value: string): string {
  const quote = value[0];
  if ((quote === '"' || quote === "'") && value.length >= 2) {
    const end = value.indexOf(quote, 1);
    if (end > 0) {
      const inner = value.slice(1, end);
      return quote === '"' ? inner.replace(/\\n/g, "\n") : inner;
    }
  }
  // Unquoted: a ` #` starts a trailing comment.
  const hash = value.search(/\s#/);
  return (hash >= 0 ? value.slice(0, hash) : value).trim();
}

/** Load both files for `cwd`. Returns the paths that were read. */
export function loadDotenv(cwd: string, env: NodeJS.ProcessEnv = process.env): string[] {
  const read: string[] = [];
  for (const name of [".env.local", ".env"]) {
    const path = findUp(cwd, name);
    if (!path) continue;
    let text: string;
    try {
      text = readFileSync(path, "utf8");
    } catch {
      continue;
    }
    read.push(path);
    for (const [key, value] of parseDotenv(text)) {
      if (env[key] === undefined) env[key] = value;
    }
  }
  return read;
}
