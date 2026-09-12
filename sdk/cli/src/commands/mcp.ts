/**
 * `oxyc mcp` — the Oxy API as MCP tools, over stdio.
 *
 * NOT THE SAME THING AS `oxy mcp`. That one is workspace tooling: it takes a
 * local checkout and exposes the semantic model, automations and `.sql` files
 * in it. This one takes a TOKEN and exposes the deployment's HTTP API — the
 * same surface `oxyc api` reaches, for an agent that would rather call a tool
 * than shell out. Different input, different audience, no overlap.
 *
 * FOUR TOOLS, NOT SIX HUNDRED — the design decision that makes this affordable.
 *
 * The obvious shape is one tool per endpoint, and it is the wrong one: the API
 * has ~670 routes, an agent runtime ships every tool's JSON schema in every
 * request, and that is tens of kilobytes of context spent on each turn before
 * a single question is asked. It would also go stale on every deploy, since
 * the tool list would be baked into this package rather than read from the
 * deployment.
 *
 * So discovery stays a QUESTION the agent asks (`routes`, `schema`) rather
 * than a payload it carries, exactly as the CLI does it. Four tools cost a few
 * hundred bytes per turn and reach every endpoint, including ones added after
 * this package was published.
 *
 * Auth, target resolution and placeholder substitution are the CLI's — the
 * same `Context`, so a token cached by `oxyc login` works
 * here with no separate setup, and `{org}` / `{workspace}` resolve the same
 * way.
 */

import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { loadCatalog, loadOpenApi, searchRoutes } from "../api/catalog.js";
import { paramsToQuery, parseFields } from "../api/fields.js";
import { runJq } from "../api/output.js";
import { isExternalSurface, normalizePath, substitutePlaceholders } from "../api/paths.js";
import { parseJson, request } from "../api/request.js";
import type { Context } from "../context/resolve.js";
import { CliError, ExitCode } from "../util/errors.js";
import { comparablePath } from "./discover.js";

/**
 * Package version, reported in the MCP handshake.
 *
 * Read from `package.json` rather than restated, so a release bump cannot
 * leave the server announcing a version nobody shipped.
 */
const VERSION: string = (() => {
  try {
    const here = dirname(fileURLToPath(import.meta.url));
    for (const candidate of [
      resolve(here, "..", "package.json"),
      resolve(here, "..", "..", "package.json")
    ]) {
      if (existsSync(candidate)) {
        return (
          (JSON.parse(readFileSync(candidate, "utf8")) as { version?: string }).version ?? "0.0.0"
        );
      }
    }
  } catch {
    // A packaging shape we did not anticipate. The handshake needs a string,
    // not an accurate one.
  }
  return "0.0.0";
})();

/**
 * A tool result. MCP wants content blocks; everything here is text, because
 * the caller is a language model and the payloads are JSON or markdown.
 */
function text(body: string, isError = false) {
  return { content: [{ type: "text" as const, text: body }], isError };
}

const TOOLS = [
  {
    name: "oxy_routes",
    description:
      "List the API endpoints this Oxy deployment mounts, with what each one does. " +
      "ALWAYS call this before oxy_request rather than guessing a path. " +
      "Pass a filter (matched against method, path, surface and description) to narrow it — " +
      "unfiltered is ~670 routes. Above 60 matches the descriptions are dropped and only " +
      "method, path and credential come back, so narrow enough to stay under that when you " +
      "need to know what an endpoint does; past ~400 it is refused outright.",
    inputSchema: {
      type: "object",
      properties: {
        filter: {
          type: "string",
          description: "Substring to narrow by, e.g. 'threads', 'sql', 'admin', 'semantic'."
        },
        all: {
          type: "boolean",
          description:
            "Include ide-only and worker-only routes. Off by default: those are mounted on one " +
            "instance of the fleet, so a caller hitting the load balancer cannot reach them directly."
        }
      }
    }
  },
  {
    name: "oxy_schema",
    description:
      "The request and response schema for ONE endpoint, so a body can be built correctly. " +
      "Covers the data plane (SQL, semantic query, and the org/workspace lookups); a blank " +
      "result means undocumented, not nonexistent — use oxy_routes to confirm the endpoint exists.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", description: "Endpoint path, e.g. '{workspace}/sql/query'." },
        method: { type: "string", description: "Narrow to one HTTP method." }
      },
      required: ["path"]
    }
  },
  {
    name: "oxy_request",
    description:
      "Make an authenticated request to the Oxy API. The credential is picked from the path " +
      "(/api/** takes a bearer, /external/api/** an API key). Placeholders {org}, {workspace}, " +
      "{project}, {customer} and {me} are substituted from context. " +
      "Read freely; ask the human before any mutating request against production.",
    inputSchema: {
      type: "object",
      properties: {
        path: { type: "string", description: "Path relative to /api, e.g. '{org}/workspaces'." },
        method: {
          type: "string",
          description: "HTTP method. Defaults to GET, or POST with fields."
        },
        fields: {
          type: "object",
          description:
            "Request parameters. Sent as a JSON body on POST/PUT/PATCH, or as query parameters " +
            "on GET/HEAD/DELETE. Values keep their JSON type."
        },
        jq: {
          type: "string",
          description:
            "A jq program to reduce the response before it is returned. Use it — a full list " +
            "response is far more context than the two fields you need."
        }
      },
      required: ["path"]
    }
  },
  {
    name: "oxy_whoami",
    description:
      "Which deployment this is pointed at and who the token belongs to. " +
      "Use it when a call returns 401/403, or a 200 with a null body — on this API an expired " +
      "session answers 200 null rather than 401, and this tells the two apart.",
    inputSchema: { type: "object", properties: {} }
  }
];

/**
 * Above this many matches, `oxy_routes` drops the descriptions.
 *
 * DEGRADES, IT DOES NOT REFUSE — the same shape as `oxyc routes`, which picks
 * `renderDetailed` under 40 matches and the compact method+path table above it
 * (`discover.ts`). The expensive field is `description`, not the row: a trimmed
 * row is a few tens of bytes, so several hundred still fit in a result an agent
 * can read, while the descriptions are what turn a broad match into hundreds of
 * KB of context spent before a question is asked.
 *
 * The threshold is on the COUNT, not on whether a `filter` was supplied — a
 * filter is not evidence of narrowing. `searchRoutes` matches a substring of
 * method, path, surface or description, so `"admin"`, `"workspace"` and
 * `"query"` are all filters a model reaches for in good faith that clear this.
 */
const MAX_DESCRIBED_ROUTES = 60;

/**
 * And above THIS many, there is nothing useful to return at all.
 *
 * Deliberately far above the threshold that drops descriptions: refusing is the
 * answer only when even the trimmed listing would be a context dump, which for
 * a deployment of ~670 routes means "you asked for effectively all of them".
 */
const MAX_ROUTES_PER_RESULT = 400;

/** `{ a: 1 }` → the `-f`/`-F` pairs `parseFields` understands. */
function fieldPairs(fields: Record<string, unknown> | undefined): string[] {
  if (!fields) return [];
  return Object.entries(fields).map(([k, v]) => `${k}=${JSON.stringify(v)}`);
}

/**
 * Check the arguments the schema says are required.
 *
 * `inputSchema.required` is ADVISORY on the low-level `Server` — it is handed
 * to the model, not enforced by the transport. Without this, `oxy_schema` with
 * no `path` matched every path and returned the whole OpenAPI document, and
 * `oxy_request` with none built a request to `/api/`.
 */
function requireArgs(name: string, args: Record<string, unknown>, required: string[]): void {
  const missing = required.filter((k) => {
    const v = args[k];
    return v === undefined || v === null || (typeof v === "string" && v.trim() === "");
  });
  if (missing.length > 0) {
    throw new CliError(`${name} needs ${missing.join(", ")}`, { code: ExitCode.USAGE });
  }
}

async function callTool(ctx: Context, name: string, args: Record<string, unknown>) {
  switch (name) {
    case "oxy_routes": {
      const catalog = await loadCatalog({ target: ctx.target(), bearer: ctx.maybeBearer() });
      const filter = (args.filter as string | undefined)?.trim();
      let matches = searchRoutes(catalog, filter || undefined);
      if (!args.all) matches = matches.filter((r) => r.role === "fleet-ok");

      // A cache stale enough to be wrong is worth saying INSIDE the result:
      // `loadCatalog` warns on stderr, which an MCP client never shows anyone.
      const staleNote = catalog.stale
        ? "NOTE: this route table came from a stale local cache; the deployment was unreachable.\n\n"
        : "";

      const asked = filter ? ` ${JSON.stringify(filter)}` : "";

      // Only a match set too large even to LIST is refused. Below that the
      // result degrades instead, because a filter like "admin" or "query" can
      // clear the description threshold in good faith and "guess something
      // narrower" is a dead end for a model with no way to see what it hit.
      if (matches.length > MAX_ROUTES_PER_RESULT) {
        return text(
          `${staleNote}${matches.length} routes match${asked}, which is effectively the whole ` +
            `deployment. Call oxy_routes again with a narrower \`filter\` (matched against ` +
            `method, path, surface and description), e.g. "threads", "sql", "semantic", "admin".`,
          true
        );
      }

      if (matches.length === 0) {
        // An empty array reads as "this deployment has no such endpoint",
        // which is usually false — the filter was wrong, or the route is
        // ide-only and hidden by default.
        return text(
          `${staleNote}No route matches ${JSON.stringify(filter ?? "")}. Try a broader filter, ` +
            `or pass all=true to include ide-only and worker-only mounts.`,
          true
        );
      }
      // Trimmed to the fields that help a caller build a request. The full
      // record carries the handler path and mount notes, which are for a
      // maintainer reading source, not for an agent composing a call.
      const described = matches.length <= MAX_DESCRIBED_ROUTES;
      // `credential` SURVIVES THE TRIM. It is what separates the bearer mount
      // from the `/external/api` API-key one, so a caller composing a request
      // needs it in either form — and `renderCompact`, the CLI shape this
      // mirrors, keeps it as a per-surface heading. It is also not the
      // expensive field: ~25 bytes a row, ~10 KB across the whole ceiling,
      // against descriptions that run to hundreds of KB.
      const trimmed = matches.map((r) =>
        described
          ? { method: r.method, path: r.path, credential: r.credential, description: r.description }
          : { method: r.method, path: r.path, credential: r.credential }
      );
      // The note says the listing is trimmed AND how to get the descriptions
      // back. Without it a model reads a description-less row as an endpoint
      // that has no documentation, rather than one it did not ask narrowly
      // enough to see.
      const trimNote = described
        ? ""
        : `NOTE: ${matches.length} routes match${asked} — too many to describe, so each row is ` +
          `method, path and credential WITHOUT its description. Narrow the \`filter\` to ` +
          `${MAX_DESCRIBED_ROUTES} or fewer matches to get the descriptions back.\n\n`;
      return text(`${staleNote}${trimNote}${JSON.stringify(trimmed)}`);
    }

    case "oxy_schema": {
      requireArgs("oxy_schema", args, ["path"]);
      const doc = (await loadOpenApi({ target: ctx.target(), bearer: ctx.maybeBearer() })) as {
        paths?: Record<string, Record<string, unknown>>;
        components?: unknown;
      };
      const wanted = String(args.path ?? "");
      const method = args.method as string | undefined;
      // `comparablePath` is imported from `discover.ts` rather than copied:
      // a second reduction of the same two spellings is a second thing to keep
      // in step, and the copy here was untested. Exact first, then substring —
      // the order `runSchema` uses, so the two commands answer alike.
      const paths = Object.entries(doc.paths ?? {});
      const exact = paths.filter(([p]) => comparablePath(p) === comparablePath(wanted));
      const hit =
        exact.length > 0
          ? exact
          : paths.filter(([p]) => comparablePath(p).includes(comparablePath(wanted)));
      if (hit.length === 0) {
        return text(
          `No documented schema for ${wanted}. The OpenAPI document covers the data plane only — ` +
            `call oxy_routes with a filter to confirm the endpoint exists.`,
          true
        );
      }
      const selected = Object.fromEntries(
        hit.map(([p, ops]) => [
          p,
          method
            ? Object.fromEntries(
                Object.entries(ops).filter(([verb]) => verb.toLowerCase() === method.toLowerCase())
              )
            : ops
        ])
      );
      return text(JSON.stringify({ paths: selected, components: doc.components }, null, 2));
    }

    case "oxy_request": {
      requireArgs("oxy_request", args, ["path"]);
      const raw = String(args.path ?? "");
      const method = String(args.method ?? "").toUpperCase() || undefined;
      const fields = parseFields([], fieldPairs(args.fields as Record<string, unknown>));
      const verb = method ?? (fields.present ? "POST" : "GET");
      const carriesBody = !["GET", "HEAD", "DELETE"].includes(verb);

      let path = substitutePlaceholders(normalizePath(raw), ctx.placeholders());
      let body: string | undefined;
      if (fields.present) {
        if (carriesBody) body = JSON.stringify(fields.params);
        else {
          const query = paramsToQuery(fields.params);
          if (query) path += (path.includes("?") ? "&" : "?") + query;
        }
      }

      const apiKey = ctx.apiKey();
      const external = isExternalSurface(path);
      const response = await request({
        target: ctx.target(),
        path,
        method: verb,
        body,
        bearer: external && apiKey ? ctx.maybeBearer() : ctx.bearer(),
        apiKey
      });

      if (response.status < 200 || response.status >= 300) {
        // The STATUS and the body, not a thrown error: the model can act on
        // "403, here is what the server said" and cannot act on a transport
        // exception. It is marked `isError` so the runtime shows it as one.
        return text(`HTTP ${response.status} ${response.statusText}\n${response.body}`, true);
      }

      if (response.body.trim() === "null") {
        return text(
          "null\n\n(NOTE: a 200 with a null body can mean an expired session on this API, " +
            "not 'no such thing'. Call oxy_whoami to tell them apart.)"
        );
      }

      const jq = args.jq as string | undefined;
      if (jq) return text(runJq(response.body, jq));
      return text(response.body);
    }

    case "oxy_whoami": {
      const target = ctx.target();
      const response = await request({
        target,
        path: "/api/user",
        method: "GET",
        bearer: ctx.bearer()
      });
      const payload = parseJson(response.body);
      if (payload === null || payload === undefined) {
        return text(
          `target: ${target}\nThe token no longer resolves to a user — the session has expired. ` +
            `Ask the human to run \`oxyc login\`.`,
          true
        );
      }
      return text(JSON.stringify({ target, user: payload }, null, 2));
    }

    default:
      return text(`unknown tool: ${name}`, true);
  }
}

/**
 * Serve MCP on stdio until the client disconnects.
 *
 * STDIO IS THE WHOLE TRANSPORT, deliberately. An HTTP/SSE server would need a
 * port, a lifetime and an auth story of its own; stdio is what every agent
 * runtime already launches, and the process inherits the caller's environment
 * — which is where the token and the target come from.
 *
 * NOTHING MAY BE WRITTEN TO STDOUT except protocol frames. `log.info` and
 * friends already go to stderr, which is why this can share the CLI's own
 * plumbing without corrupting the stream.
 */
export async function runMcp(ctx: Context): Promise<void> {
  const server = new Server({ name: "oxyc", version: VERSION }, { capabilities: { tools: {} } });

  server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: TOOLS }));

  server.setRequestHandler(CallToolRequestSchema, async (req) => {
    try {
      return await callTool(
        ctx,
        req.params.name,
        (req.params.arguments ?? {}) as Record<string, unknown>
      );
    } catch (cause) {
      // A throw here would kill the session. The model can act on a message;
      // it cannot act on a dead transport.
      const message = (cause as Error).message ?? String(cause);
      // BOTH CHANNELS. A model gets one string, so there is no marker to
      // distinguish them — but reading `hint` alone drops the one line that
      // says what to DO, which is the half a model can act on.
      const { hint, remedy } = cause as { hint?: string; remedy?: string };
      // Blank line between them, the way every other renderer of these two
      // separates them — a model given two consecutive imperatives with no
      // boundary loses the distinction the second field exists to make.
      // `!== undefined` for the narrowing TS gives it, and a length check
      // because an empty string would open the block with a blank line —
      // `filter(Boolean)` covered that and cost the narrowing, so both.
      const extra = [hint, remedy].filter((line) => line !== undefined && line !== "").join("\n\n");
      return text(extra ? `${message}\n\n${extra}` : message, true);
    }
  });

  await server.connect(new StdioServerTransport());

  // `connect` returns once the transport is wired; the process must stay up
  // until the client closes stdin.
  await new Promise<void>((resolve) => {
    process.stdin.on("close", resolve);
    server.onclose = resolve;
  });
}
