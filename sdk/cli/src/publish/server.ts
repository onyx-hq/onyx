/**
 * The three HTTP calls a publish makes, plus the trusted-publishing exchange.
 *
 * Direct `fetch` rather than `api/request.ts`: the upload is multipart and the
 * two lookups are public, and neither fits a JSON-string request with a
 * credential chosen by path. What they share with it is the exit-code mapping.
 */

import { CliError, ExitCode, exitCodeForStatus } from "../util/errors.js";

const LOOKUP_TIMEOUT_MS = 30_000;
/** The bundle upload, which can be tens of megabytes. */
const UPLOAD_TIMEOUT_MS = 120_000;

async function send(url: string, init: RequestInit, timeoutMs: number): Promise<Response> {
  try {
    return await fetch(url, { ...init, signal: AbortSignal.timeout(timeoutMs) });
  } catch (cause) {
    throw new CliError(`${init.method ?? "GET"} ${url} failed: ${(cause as Error).message}`, {
      code: ExitCode.UNAVAILABLE
    });
  }
}

async function json<T>(response: Response, what: string): Promise<T> {
  const text = await response.text();
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new CliError(`could not parse the ${what} response`, {
      code: ExitCode.UNAVAILABLE,
      detail: text.slice(0, 2000)
    });
  }
}

function base(target: string): string {
  return target.replace(/\/+$/, "");
}

/** The project an app publishes into, from its `(org, app)` on the target. Public. */
export async function fetchProject(target: string, org: string, app: string): Promise<string> {
  const url = `${base(target)}/api/apps/${encodeURIComponent(org)}/${encodeURIComponent(app)}/build-config`;
  const response = await send(url, {}, LOOKUP_TIMEOUT_MS);
  if (response.status === 404) {
    throw new CliError(`app ${org}/${app} is not registered on ${target}`, {
      code: ExitCode.NOT_FOUND,
      hint: "register it in the oxy admin UI, or pass --project <uuid> for its first publish"
    });
  }
  if (!response.ok) {
    throw new CliError(`build-config lookup failed (${response.status}) at ${url}`, {
      code: exitCodeForStatus(response.status)
    });
  }
  return (await json<{ project_id: string }>(response, "build-config")).project_id;
}

/** The org a pinned workspace belongs to — a workspace has exactly one. Public. */
export async function fetchOrgForProject(target: string, projectId: string): Promise<string> {
  const url = `${base(target)}/api/org-for-project/${encodeURIComponent(projectId)}`;
  const response = await send(url, {}, LOOKUP_TIMEOUT_MS);
  if (response.status === 404) {
    throw new CliError(`workspace ${projectId} not found on ${target}`, {
      code: ExitCode.NOT_FOUND,
      hint: "check --project / OXY_PROJECT"
    });
  }
  if (!response.ok) {
    throw new CliError(`org-for-project lookup failed (${response.status}) at ${url}`, {
      code: exitCodeForStatus(response.status)
    });
  }
  return (await json<{ org_slug: string }>(response, "org-for-project")).org_slug;
}

/** The server's `PublishResult`. Extra fields are tolerated. */
export interface PublishResult {
  app_id: string;
  build_id: string;
  url: string;
  channel: string;
  org_slug?: string;
  is_new_app?: boolean;
  warnings?: string[];
}

export interface UploadRequest {
  target: string;
  token: string;
  /** Multipart text fields, in order. Undefined values are left out. */
  fields: Array<[string, string | undefined]>;
  tarball: Buffer;
}

/**
 * POST the bundle. Resolves the parsed result, or throws with the status
 * mapped onto the exit-code contract and the server's body as detail.
 */
export async function uploadBundle(req: UploadRequest): Promise<PublishResult> {
  const form = new FormData();
  for (const [name, value] of req.fields) {
    if (value !== undefined) form.append(name, value);
  }
  // A copy into a plain ArrayBuffer: `Blob` does not accept a Buffer that may
  // sit on a SharedArrayBuffer, which is what the type of `gzipSync`'s result allows.
  const bytes = new Uint8Array(req.tarball);
  form.append("bundle", new Blob([bytes], { type: "application/gzip" }), "bundle.tar.gz");

  const url = `${base(req.target)}/api/customer-apps/publish`;
  const response = await send(
    url,
    { method: "POST", headers: { authorization: `Bearer ${req.token}` }, body: form },
    UPLOAD_TIMEOUT_MS
  );
  if (!response.ok) {
    const body = await response.text();
    throw new CliError(`publish failed (${response.status})`, {
      code: exitCodeForStatus(response.status),
      detail: body.slice(0, 4000)
    });
  }
  return json<PublishResult>(response, "publish");
}

/** The two variables GitHub sets in a job granted `id-token: write`. */
export function githubOidcAvailable(env: NodeJS.ProcessEnv = process.env): boolean {
  return Boolean(env.ACTIONS_ID_TOKEN_REQUEST_URL && env.ACTIONS_ID_TOKEN_REQUEST_TOKEN);
}

/** The audience the server pins. A token minted for any other is refused. */
export const OIDC_AUDIENCE = "oxy-publish";

/**
 * Trusted publishing: a GitHub OIDC token, exchanged for a publish credential
 * scoped to exactly `org/app` and valid for fifteen minutes.
 *
 * Called immediately before the upload, not at startup: the OIDC token is
 * single-use and the credential short-lived, and a build can take longer than
 * either should be held. The server matches the token's claims against a
 * publisher registered for the app — repository owner id, repo, workflow path,
 * environment — so a 403 here almost always means a registration mismatch.
 */
export async function exchangeGithubOidc(
  target: string,
  orgSlug: string,
  app: string,
  env: NodeJS.ProcessEnv = process.env
): Promise<string> {
  const requestUrl = new URL(env.ACTIONS_ID_TOKEN_REQUEST_URL ?? "");
  requestUrl.searchParams.set("audience", OIDC_AUDIENCE);
  const minted = await send(
    requestUrl.toString(),
    { headers: { authorization: `bearer ${env.ACTIONS_ID_TOKEN_REQUEST_TOKEN}` } },
    LOOKUP_TIMEOUT_MS
  );
  if (!minted.ok) {
    throw new CliError(`GitHub refused to mint an OIDC token (${minted.status})`, {
      code: ExitCode.AUTH,
      hint: "the publishing job needs `permissions: id-token: write`"
    });
  }
  const oidc = (await json<{ value?: string }>(minted, "GitHub OIDC")).value;
  if (!oidc) throw new CliError("GitHub returned no OIDC token", { code: ExitCode.AUTH });

  const url = `${base(target)}/api/customer-apps/publish/oidc-exchange`;
  const exchanged = await send(
    url,
    {
      method: "POST",
      // `Bearer` exactly: the server matches the prefix case-sensitively.
      headers: { authorization: `Bearer ${oidc}`, "content-type": "application/json" },
      body: JSON.stringify({ app: `${orgSlug}/${app}` })
    },
    LOOKUP_TIMEOUT_MS
  );
  if (!exchanged.ok) {
    const body = await exchanged.text();
    throw new CliError(`trusted-publishing exchange failed (${exchanged.status})`, {
      code: exitCodeForStatus(exchanged.status),
      detail: body.slice(0, 2000),
      hint: `is this workflow registered as a publisher for ${orgSlug}/${app}, with the same repository, workflow file and environment?`
    });
  }
  const token = (await json<{ token?: string }>(exchanged, "exchange")).token;
  if (!token) throw new CliError("the exchange returned no token", { code: ExitCode.AUTH });
  return token;
}
