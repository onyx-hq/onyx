/**
 * `oxyc assume start | status | end` — acting as an organization.
 *
 * A thin client over `/api/assume`. It replaced the Rust `oxy assume`, which
 * was deleted once this covered it. The sessions are Postgres rows hanging off
 * the account, so the browser and this terminal act as one — there is no
 * per-client state to keep in sync.
 *
 * WHY THE EXIT IS NOT BEHIND `/admin`: `/api/assume` is mounted outside it
 * deliberately, because acting as an org CLOSES the admin surface. An `end`
 * that lived behind the door it locks would leave an operator stuck for the
 * full 60 minutes. `router/global.rs` states that where the mount is.
 */

import { parseJson, request } from "../api/request.js";
import type { Context } from "../context/resolve.js";
import { looksLikeUrl, parseEnvUrl } from "../context/target.js";
import * as log from "../ui/log.js";
import { table } from "../ui/render.js";
import { out } from "../ui/tty.js";
import { CliError, ExitCode, exitCodeForStatus } from "../util/errors.js";

/** What `/api/assume` returns for one live session. */
interface SessionDto {
  id: string;
  org_id: string;
  org_name?: string | null;
  org_slug?: string | null;
  /**
   * Carried because the server sends it and a reader of this type should know
   * why it exists: a partner's surface is the partner console, not the org
   * home. Nothing here branches on it today — the line below
   * prints the org home either way — so the distinction is documented here
   * rather than asserted next to code that does not make it.
   */
  is_partner: boolean;
  actor_email: string;
  reason: string;
  started_at: string;
  expires_at: string;
  /**
   * THE SERVER'S COUNTDOWN, and the reason `minutesLeft` prefers it: it is
   * `expires_at - now` computed where `now` is the server's clock. Recomputing
   * locally misreports the one field an operator uses to decide whether to
   * start again, on any laptop whose clock has drifted.
   */
  expires_in_seconds?: number;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * `--org` as an org id.
 *
 * THREE SPELLINGS, because all three are things an operator has to hand: the
 * UUID from a support ticket, the slug from a conversation, and the URL they
 * are already looking at. A URL is the one worth supporting explicitly —
 * `--env https://poke-house.oxygen-hq.com` already names the org, so requiring
 * `--org poke-house` beside it would be asking for what was just said.
 */
/**
 * The org a verb is about: what was passed, else what the `--env` URL named.
 *
 * EXPORTED AND NAMED because both `start` and `end` need it and neither could
 * be tested on it: a loopback harness always passes `--target`, which
 * overrides `--env` and takes the org slug with it, so the hint is
 * unreachable end-to-end. Written once, it is one small thing to pin — and the
 * bug it exists to prevent was `end` not having it, which quietly ended every
 * live session when an operator named one org by URL.
 */
export function orgOrHint(ctx: Pick<Context, "env">, org: string | undefined): string {
  return (org ?? ctx.env().orgSlug ?? "").trim();
}

async function resolveOrgId(ctx: Context, org: string | undefined): Promise<string> {
  const raw = orgOrHint(ctx, org);
  if (!raw) {
    throw new CliError("no organization given", {
      code: ExitCode.USAGE,
      remedy:
        "pass --org <slug|uuid|url>, or point --env at an org URL " +
        "(e.g. --env https://poke-house.oxygen-hq.com)"
    });
  }
  if (UUID.test(raw)) return raw;

  const slug = looksLikeUrl(raw) ? parseEnvUrl(raw)?.orgSlug : raw;
  if (!slug) {
    throw new CliError(`could not read an org from '${raw}'`, {
      code: ExitCode.USAGE,
      detail: "org URLs look like https://<org-slug>.oxygen-hq.com",
      remedy: "pass the slug or the UUID instead"
    });
  }
  return orgIdForSlug(ctx, slug);
}

/** Rows from an endpoint, or undefined when it is not reachable for this token. */
async function rows(ctx: Context, path: string): Promise<unknown[] | undefined> {
  const response = await request({
    target: ctx.target(),
    path,
    method: "GET",
    bearer: ctx.bearer()
  });
  if (response.status < 200 || response.status >= 300) return undefined;
  const body = parseJson(response.body);
  if (Array.isArray(body)) return body;
  // `/api/admin/orgs-meta` paginates; the rows are under a key.
  for (const key of ["items", "orgs", "data", "results"]) {
    const nested = (body as Record<string, unknown> | undefined)?.[key];
    if (Array.isArray(nested)) return nested;
  }
  return undefined;
}

/** `id` on `/orgs` and `/admin/orgs-meta`; `org_id` on a partner's client list. */
function idForSlug(list: unknown[], slug: string): string | undefined {
  for (const row of list) {
    const r = row as Record<string, unknown>;
    if (r.slug !== slug) continue;
    const id = r.id ?? r.org_id;
    if (typeof id === "string") return id;
  }
  return undefined;
}

/**
 * THREE ENDPOINTS, IN ORDER OF REACH.
 *
 * `/api/orgs` is what a member sees, `/admin/orgs-meta` what staff see, and
 * the partner client list what a partner sees. Trying them in that order means
 * the common case costs one call and the rarest costs three — and a partner,
 * who can reach only the third, still gets an answer rather than a 403 from
 * the first.
 */
async function orgIdForSlug(ctx: Context, slug: string): Promise<string> {
  const direct = await rows(ctx, "/api/orgs");
  if (direct) {
    const id = idForSlug(direct, slug);
    if (id) return id;
  }
  const admin = await rows(
    ctx,
    `/api/admin/orgs-meta?search=${encodeURIComponent(slug)}&page_size=200`
  );
  if (admin) {
    const id = idForSlug(admin, slug);
    if (id) return id;
  }
  const partner = await partnerClientId(ctx, slug);
  if (partner) return partner;
  throw new CliError(`no organization with slug '${slug}' is visible to you on ${ctx.target()}`, {
    code: ExitCode.NOT_FOUND,
    remedy: "check the slug, or pass the org UUID directly with --org"
  });
}

/**
 * A partner's assigned clients, across every partner they operate.
 *
 * TWO HOPS, because there is no one-shot route: `/api/partners` lists the
 * partners you hold a role at, and each one's clients are under
 * `/api/partners/{id}/orgs`. The first version of this called
 * `/api/partner/clients`, which is not a route in this repo at all — so the
 * one population this tier exists for got "no organization with slug 'x' is
 * visible to you" about an org that is.
 *
 * One unreadable partner does not abort the search: the next may hold the org.
 */
async function partnerClientId(ctx: Context, slug: string): Promise<string | undefined> {
  const partners = await rows(ctx, "/api/partners");
  if (!partners) return undefined;
  for (const p of partners) {
    const partnerId = (p as Record<string, unknown>).partner_id;
    if (typeof partnerId !== "string") continue;
    const clients = await rows(ctx, `/api/partners/${encodeURIComponent(partnerId)}/orgs`);
    if (!clients) continue;
    const id = idForSlug(clients, slug);
    if (id) return id;
  }
  return undefined;
}

/** `Acme (acme)` — or just whichever half the server gave us. */
function describe(s: SessionDto): string {
  const name = s.org_name?.trim();
  const slug = s.org_slug?.trim();
  if (name && slug) return `${name} (${slug})`;
  return name || slug || s.org_id;
}

function minutesLeft(s: SessionDto): string {
  const seconds =
    typeof s.expires_in_seconds === "number"
      ? s.expires_in_seconds
      : (Date.parse(s.expires_at) - Date.now()) / 1000;
  if (Number.isNaN(seconds)) return "?";
  if (seconds <= 0) return "expired";
  return `${Math.floor(seconds / 60)}m`;
}

/**
 * The rules, printed after starting — because they are the part people are
 * surprised by, and a session you did not know was time-boxed is a session you
 * discover has ended mid-investigation.
 */
function printSessionRules(target: string): void {
  log.info("60 minutes, not renewable — re-running returns the same session.");
  // The one people are most surprised by, and the reason this lists three
  // properties rather than two: the session hangs off your ACCOUNT.
  log.info("it is your account that acts, not this terminal — your browser is in there too.");
  log.info(`acting closes /admin; end it with \`oxyc assume end\` against ${target}.`);
}

export async function runAssumeStart(
  ctx: Context,
  org: string | undefined,
  reason: string
): Promise<void> {
  const trimmed = reason.trim();
  if (!trimmed) {
    throw new CliError("--reason must not be empty", {
      code: ExitCode.USAGE,
      detail: "it is recorded in the impersonation log, and an unexplained one is a red flag"
    });
  }
  const orgId = await resolveOrgId(ctx, org);
  const target = ctx.target();

  const response = await request({
    target,
    path: "/api/assume",
    method: "POST",
    bearer: ctx.bearer(),
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ org_id: orgId, reason: trimmed })
  });

  if (response.status < 200 || response.status >= 300) {
    throw assumeError(response.status, orgId);
  }
  const session = parseJson(response.body) as SessionDto;
  process.stdout.write(`${out.green(`Now acting as ${describe(session)} on ${target}.`)}\n`);
  if (session.org_slug) {
    process.stdout.write(`${out.dim(`Surface: ${target}/${session.org_slug}`)}\n`);
  }
  printSessionRules(target);
}

/**
 * Map the server's status onto the reason it refused, so an operator is not
 * left guessing at a bare 403.
 */
function assumeError(status: number, orgId: string): CliError {
  const detail =
    status === 403
      ? "you are not allowed to act as this org (Oxy staff may act as any org; a partner only as an assigned client, and only with `develop_apps`)"
      : status === 404
        ? "no organization with that id exists on this deployment"
        : status === 400
          ? "the request was rejected — a non-empty --reason is required"
          : status === 401
            ? "not authenticated for this target"
            : "the server refused the request";
  return new CliError(`could not start an assume-role session for ${orgId} (${status})`, {
    code: exitCodeForStatus(status),
    detail,
    remedy: status === 401 ? "oxyc login --env <env>" : undefined
  });
}

export async function runAssumeStatus(ctx: Context, json: boolean): Promise<void> {
  const target = ctx.target();
  const response = await request({
    target,
    path: "/api/assume/current",
    method: "GET",
    bearer: ctx.bearer()
  });
  if (response.status < 200 || response.status >= 300) {
    throw new CliError(`could not read assume sessions from ${target} (${response.status})`, {
      code: exitCodeForStatus(response.status),
      remedy: response.status === 401 ? "oxyc login --env <env>" : undefined
    });
  }
  const sessions = (parseJson(response.body) ?? []) as SessionDto[];

  if (json) {
    process.stdout.write(`${JSON.stringify(sessions, null, 2)}\n`);
    return;
  }
  if (sessions.length === 0) {
    // NOT an error: "you are not acting as anyone" is a legitimate answer, and
    // the commonest reason a staff request 403s is that this is the state.
    log.info(`no live assume-role session on ${target}`);
    return;
  }
  process.stdout.write(
    `${table(sessions, [
      { header: "ORG", value: (s) => describe(s) },
      { header: "LEFT", value: (s) => minutesLeft(s) },
      { header: "REASON", value: (s) => s.reason }
    ])}\n`
  );
  printSessionRules(target);
}

export async function runAssumeEnd(
  ctx: Context,
  org: string | undefined,
  all: boolean
): Promise<void> {
  const target = ctx.target();
  // AN EMPTY `--org` IS NOT AN ABSENT ONE. `oxyc assume end --org "$ORG"` with
  // `$ORG` unset is the ordinary way this happens, and reading it as "no org
  // named" sent an unscoped DELETE — every live session, across every org, on
  // a verb that cannot be undone. It also swallowed the `--env` hint, so a URL
  // naming one org ended all of them. So it refuses: a present-but-blank
  // `--org` answers "no organization given".
  //
  // Checked HERE rather than in `orgOrHint`, because "" is harmless for
  // `start` — `resolveOrgId` already refuses it — and the consequence only
  // lands on the destructive verb.
  if (org !== undefined && org.trim() === "") {
    throw new CliError("--org was given but is empty", {
      code: ExitCode.USAGE,
      detail:
        "an unset shell variable reads this way, and ending every session is not the safe guess",
      remedy: "name an org, or pass --all to end every session deliberately"
    });
  }

  // THE `--env` URL NAMES AN ORG, and `end` has to hear it — `start` does
  // (`resolveOrgId` falls back to the same hint), so ignoring it here made one
  // URL mean an org for one verb and nothing for the other. `--all` is what
  // suppresses the hint — the flag exists to let you say "everything"
  // explicitly, not to unlock it.
  const hinted = orgOrHint(ctx, org);
  const orgId = all || !hinted ? undefined : await resolveOrgId(ctx, hinted);
  const path = orgId ? `/api/assume?org_id=${encodeURIComponent(orgId)}` : "/api/assume";

  const response = await request({ target, path, method: "DELETE", bearer: ctx.bearer() });
  if (response.status < 200 || response.status >= 300) {
    throw new CliError(`could not end the assume-role session (${response.status})`, {
      code: exitCodeForStatus(response.status),
      remedy: response.status === 401 ? "oxyc login --env <env>" : undefined
    });
  }
  process.stdout.write(
    `${out.green(orgId ? `Stopped acting as ${orgId} on ${target}.` : `Stopped acting on ${target}.`)}\n`
  );
}
