/**
 * The merge point of the two tools.
 *
 * `oxyc`'s customer half knows WHO — which customer, where their repo is, what
 * org they are. `oxyc`'s API half knows HOW to ask a deployment a question.
 * This is what joins them: one resolution that produces both halves, so
 * `oxyc api {org}/workspaces` works from inside a customer repo without
 * anyone typing an id.
 *
 * `gh api` does the same trick with `{owner}` and `{repo}` read off the git
 * remote, and it is the single feature that makes that command usable from
 * memory. Everything here exists to make ours as cheap to use.
 *
 * RESOLUTION IS LAZY AND PARTIAL, deliberately. `oxyc routes` needs a target
 * and no customer; `oxyc list` needs neither. Resolving everything eagerly
 * would make a command fail on a fact it never uses — which is how a tool
 * ends up demanding a GitHub login to print its own help.
 */

import type { PlaceholderValues } from "../api/paths.js";
import { loadCredential, resolveBearer } from "../auth/credentials.js";
import { dossierPath, isCloned, slugForDirectory } from "../customer/dossier.js";
import { type Customer, customersOrg, resolveCustomer } from "../github/customers.js";
import { authError, CliError, ExitCode } from "../util/errors.js";
import { repoRoot } from "../util/git.js";
import { loadForTargetResolution, type ResolvedEnv, resolveEnv } from "./target.js";

/** Flags every command shares. Kept in one shape so `main.ts` wires them once. */
export interface GlobalFlags {
  env?: string;
  target?: string;
  tokenEnv?: string;
  apiKeyEnv?: string;
  org?: string;
  workspace?: string;
  project?: string;
  customer?: string;
  refresh?: boolean;
}

/** Everything a command might need, resolved on demand. */
export interface Context {
  readonly cwd: string;
  readonly flags: GlobalFlags;

  /** The deployment to talk to. Throws when nothing resolves. */
  target(): string;
  /** The resolved env, including the org slug a pasted URL carried. */
  env(): ResolvedEnv;
  /** The bearer, or a thrown `authError` naming the login command. */
  bearer(): string;
  /** The bearer if there is one, without throwing. */
  maybeBearer(): string | undefined;
  /** The API key for the `/external/api` surface, if one is configured. */
  apiKey(): string | undefined;
  /** The customer this invocation is about, if it is about one. */
  customer(): Customer | undefined;
  /** The customer's repo checkout on this machine, if it is here. */
  repoDir(): string | undefined;
  /** Values for `{org}` / `{workspace}` / … in a path. */
  placeholders(): PlaceholderValues;
  /**
   * The same invocation pointed at a different deployment.
   *
   * For `oxyc login --login-env dev,staging`, which is several independent acts
   * rather than one act with several targets — each needs its own resolved
   * env, and a `Context` memoizes the one it was built with. Rebuilding is
   * cheaper than making every consumer take a target parameter it has no use
   * for; `--target` is dropped, because it overrides `--env` and carrying it
   * would point all of them at one host.
   */
  withEnv(env: string): Context;
}

/**
 * Build the context for one invocation.
 *
 * Every accessor memoises, so a command that asks for the bearer three times
 * reads the credentials file once — and, more importantly, a command that
 * never asks never touches the network or the disk at all.
 */
export function createContext(flags: GlobalFlags, cwd = process.cwd()): Context {
  const memo = new Map<string, unknown>();

  const once = <T>(key: string, compute: () => T): T => {
    if (!memo.has(key)) memo.set(key, compute());
    return memo.get(key) as T;
  };

  const env = (): ResolvedEnv =>
    once("env", () => {
      // Read here rather than up front: a broken oxy-app.json is an error, and
      // it must not fail a command that never resolves a target (`oxyc list`).
      const manifest = loadForTargetResolution(cwd, flags.target);
      const resolved = resolveEnv(flags.env ?? "production", flags.target, manifest);
      if (!resolved) {
        throw new CliError(`could not resolve a target for --env ${flags.env}`, {
          code: ExitCode.USAGE,
          hint: "pass --target <url>, use a URL as the env (--env https://…), or add it to oxy-app.json environments"
        });
      }
      return resolved;
    });

  const customer = (): Customer | undefined =>
    once("customer", () => {
      // An explicit --customer (or a positional the launcher already resolved)
      // wins. Otherwise infer from the checkout we are standing in, which is
      // what makes the placeholders free inside a customer session.
      if (flags.customer) return resolveCustomer(flags.customer, { refresh: flags.refresh });
      const slug = slugForDirectory(cwd);
      if (!slug) return undefined;
      const [, name] = slug.split("/");
      if (!name) return undefined;
      try {
        return resolveCustomer(name, { refresh: flags.refresh });
      } catch {
        // Standing in one of OUR repos, or any repo that is not a customer.
        // Not an error: most invocations are not about a customer at all.
        return undefined;
      }
    });

  const repoDir = (): string | undefined =>
    once("repoDir", () => {
      const found = customer();
      if (found) {
        const slug = `${customersOrg()}/${found.name}`;
        if (isCloned(slug)) return dossierPath(slug);
      }
      // `--here`: working in one of our own repos on a customer's behalf.
      return repoRoot(cwd);
    });

  return {
    cwd,
    flags,
    env,
    // `target: undefined` on purpose: it overrides `--env`, so carrying it
    // would point every rebuilt context at one host — which is the opposite of
    // what asking for a different env means.
    withEnv: (next: string) => createContext({ ...flags, env: next, target: undefined }, cwd),
    target: () => env().target,
    maybeBearer: () =>
      once("bearer", () => resolveBearer(env().target, flags.tokenEnv ?? "OXY_TOKEN")),
    bearer() {
      const token = this.maybeBearer();
      if (!token)
        throw authError(env().target, flags.env ?? "production", flags.tokenEnv ?? "OXY_TOKEN");
      return token;
    },
    apiKey: () => {
      const name = flags.apiKeyEnv ?? "OXY_API_KEY";
      return process.env[name]?.trim() || undefined;
    },
    customer,
    repoDir,
    placeholders: () =>
      once("placeholders", () => {
        const found = customer();
        const values: PlaceholderValues = {
          // `--org` wins, then the org a pasted `--env` URL named, then the
          // customer's own slug. The URL is ahead of the customer because
          // pasting an address bar is a deliberate act of naming an org.
          org: flags.org ?? env().orgSlug ?? found?.name,
          workspace: flags.workspace,
          project: flags.project,
          customer: found?.name,
          me: loadCredential(env().target)?.email
        };
        // A workspace id was not passed and the repo is here: the workspace is
        // a directory, not an id, so it cannot fill {workspace}. Left unset so
        // the placeholder error says how to find one rather than substituting
        // a path into a URL.
        return values;
      })
  };
}
