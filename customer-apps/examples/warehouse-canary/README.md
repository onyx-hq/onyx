# Warehouse Canary — `ctx.warehouse` writes, on a schedule

A functions-only custom app. Every five minutes it writes to a ClickHouse table
it owns through `ctx.warehouse`, then reads the rows back. When a write fails or
doesn't land, the run fails.

## Why it exists

In 0.5.140–0.5.144 every `ctx.warehouse.insert` against ClickHouse failed with
`Code: 27`. The audit tag the host added was being read as a row. Reads kept
working. The first report came from a customer whose report upload broke, two
days in. No platform check wrote anything, so none of them looked wrong.

The canary makes those writes on a schedule, in a table no customer reads:

| Step | Exercises | Broke in |
| ---- | --------- | -------- |
| `ctx.warehouse.insert` | `build_insert_sql`'s quoted `INSERT … VALUES` | 0.5.140–0.5.144 |
| `ctx.warehouse.exec` of an INSERT opening with `--` | a statement whose first word is not INSERT | #3158's fix |
| `ctx.warehouse.query` read-back | a write that returned but didn't land | — |

## How a failure reaches anyone

A failed run is an ordinary failed invocation:

- It logs one `WARN` line on target `oxy::app_function` to the platform log
  (HyperDX), with `error.type` and `error.fingerprint`.
- It marks the invocation span `ERROR`.
- Three failures carrying a fingerprint the function hasn't had in a week page
  `OXY_OPS_SLACK_CHANNEL` (`custom_apps_functions::failure_alert`).

At a five-minute cadence, a deploy that breaks a write path pages about fifteen
minutes after it lands, whether or not any customer is writing.

The page rule is "new in a week", so a canary failure can go unpaged:

- **Never paged:** the same failure returns within a week of its last page,
  for example a bad change that was rolled back and then redeployed.
- **Paged late:** the canary already paged for another failure in the last six
  hours. The new failure is held back until that window passes, then pages on
  its next run.

Both cases still log the `WARN` line and mark the span. After any
rollback-and-redeploy, check the canary's invocations in the admin console.

After a deploy you don't have to wait for the schedule:

```sh
oxyc api /api/admin/apps/<app-id>/functions/canary/runs -X POST --env prod
```

## Setup (once per environment)

1. **Pick an internal org and workspace.** Not a customer's.
2. **Create a ClickHouse database and user for it.** The user needs `CREATE
   TABLE`, `INSERT` and `SELECT` on that one database, and nothing else.
3. **Declare it in the workspace's `config.yml`** under the name the manifest
   allows (`destinations: ["canary_warehouse"]`):

   ```yaml
   databases:
     - name: canary_warehouse
       type: clickhouse
       host: https://<clickhouse-host>:8443
       user: oxy_canary
       password_var: CANARY_CLICKHOUSE_PASSWORD
       database: oxy_canary
   ```

   Store `CANARY_CLICKHOUSE_PASSWORD` in the workspace's secrets.
4. **Publish** from this directory:

   ```sh
   oxy publish
   ```

   Publishing registers the `*/5 * * * *` schedule. Scheduled runs need the
   global worker (`OXY_INPROC_GLOBAL_WORKER`). With it off, the schedule exists
   and never fires.

The table cleans itself: rows expire after seven days (`TTL`).

To cover another engine (Postgres, Snowflake, BigQuery), add a database to
`config.yml`, add its name to `destinations`, and repeat the steps against it.
A `CREATE TABLE` in the dialect is the only part that changes.
