// canary — writes through `ctx.warehouse` every five minutes, against a
// ClickHouse table it owns, and fails loudly when a write does not land.
//
// In 0.5.140–0.5.144 every `ctx.warehouse.insert` against ClickHouse failed,
// and the first to notice was a customer whose report upload broke. Reads kept
// working, so nothing that only read looked wrong. This function does the
// writes nobody on the platform side does: the exact shapes that broke, on a
// schedule, into a table no customer reads.
//
// A failed run is an ordinary failed invocation. Three of them with a failure
// the function has not had in a week page ops through the function failure
// alert (`custom_apps_functions::failure_alert`) — about fifteen minutes after
// a deploy that breaks a write path, whether or not any customer writes.
//
// See README.md for the one-time setup (database, credential, publish).

import type { OxyFunctionContext, OxyFunctionRequest } from "@oxy-hq/sdk";

/** The workspace database this app may write — `destinations` in oxy-app.json. */
const DATABASE = "canary_warehouse";
const TABLE = "oxy_canary_writes";

/**
 * Created on first use. The TTL is the cleanup: rows age out after a week, so
 * the canary never needs a delete of its own.
 */
async function ensureTable(ctx: OxyFunctionContext): Promise<void> {
  await ctx.warehouse.exec(
    DATABASE,
    `CREATE TABLE IF NOT EXISTS ${TABLE} (
       run String,
       path LowCardinality(String),
       written_at DateTime DEFAULT now()
     ) ENGINE = MergeTree
     ORDER BY (written_at, run)
     TTL written_at + INTERVAL 7 DAY`
  );
}

/** An id for this run. The isolate has no `crypto`, and this only has to be unique. */
function runId(): string {
  return `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
}

export default async function canary(
  _req: OxyFunctionRequest,
  ctx: OxyFunctionContext
): Promise<Response> {
  await ensureTable(ctx);
  const run = runId();

  // 1. `ctx.warehouse.insert`: quoted identifiers and a VALUES list — the
  //    statement every app insert sent when the outage hit.
  await ctx.warehouse.insert(DATABASE, TABLE, [{ run, path: "insert" }]);

  // 2. `ctx.warehouse.exec` of an INSERT that opens with a comment — the shape
  //    the first fix still broke, because it looked for INSERT as the first word.
  await ctx.warehouse.exec(
    DATABASE,
    `-- canary: a statement that opens with a comment
     INSERT INTO ${TABLE} (run, path) VALUES ('${run}', 'exec')`
  );

  // 3. Read both back. A write that "succeeded" without landing is a failure too.
  const { rows } = await ctx.warehouse.query(
    DATABASE,
    `SELECT path FROM ${TABLE} WHERE run = '${run}' ORDER BY path`
  );
  const paths = rows.map((row) => String(row.path));
  if (paths.join(",") !== "exec,insert") {
    return Response.json({ ok: false, run, landed: paths }, { status: 500 });
  }
  return Response.json({ ok: true, run });
}
