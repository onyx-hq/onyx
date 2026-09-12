use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, Statement};

/// Index for the anomaly inbox's page-count query.
///
/// The inbox pages **events**, not rows, so its `total` is
/// `COUNT(DISTINCT COALESCE(event_id, id))` filtered by workspace and status.
/// `idx_metric_anomalies_workspace_status_detected` narrows the rows, but the
/// distinct is over an expression, so Postgres still had to read every matching
/// row and aggregate. That count runs on every list request once a workspace
/// holds more than one page of anomalies — including the Semantic Model tab
/// badge, which mounts whether or not anyone opens the inbox, and re-runs after
/// each scan poll and each Ack.
///
/// Indexing the expression itself lets the same query run index-only.
/// `COALESCE` over two immutable columns is immutable, so it is indexable.
///
/// Raw SQL because sea-orm's schema builder has no expression-index form.
///
/// **Locking, deliberately.** This is a plain `CREATE INDEX`, so it holds a
/// `SHARE` lock on `metric_anomalies` for the duration of the build: reads keep
/// working, but every write blocks — scan upserts and status changes alike.
/// When this was written the reasoning was that `CONCURRENTLY` was not an
/// option, because sea-orm ran migrations inside a transaction and Postgres
/// refuses concurrent index builds inside one. That held for
/// sea-orm-migration 1.x; since 2.0 a migration can opt out with
/// `use_transaction() -> Some(false)` and build `CONCURRENTLY` — see
/// `m20260911_000002_function_failure_fingerprint_index`. A new index on a
/// table that grows with traffic should do that rather than take this lock.
/// The call to take the lock anyway
/// rests on the table's size, not on hope — `metric_anomalies` holds one row per
/// flagged bucket per monitored segment, upserted rather than appended, so it
/// grows with monitor count and history, not with traffic. That is a table in
/// the thousands-to-low-millions, where this build is seconds at the outside,
/// and it happens once during a deploy's migration step — a window in which the
/// inbox still serves and only writes wait.
///
/// Two caveats on that estimate, since it is the whole basis for taking the
/// lock. It has **no retention**: nothing prunes resolved anomalies, so the
/// bound is monitors × segments × history, and history only grows. And a single
/// long-running regime shift is one event with an unbounded bucket count (which
/// is why the *read* path caps buckets per event). Neither breaks the estimate
/// on a workspace of ordinary age, but both mean "thousands-to-low-millions" is
/// a claim to re-check on a mature tenant rather than a property of the schema.
/// The migration logs its own row estimate before building, so the number is in
/// the deploy log rather than something to go looking for afterwards. If that ever stops
/// being true, the move is a separate out-of-band `CREATE INDEX CONCURRENTLY`
/// and an empty migration here — not a silent downgrade to no index, which puts
/// a full aggregate back on the inbox's page-count query.
///
/// That escape hatch needs no code change, which is the point of `IF NOT
/// EXISTS`: an operator who knows their `metric_anomalies` is large can build
/// the index out of band *before* deploying, and this migration then finds it
/// and does nothing.
///
/// ```sql
/// CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_metric_anomalies_ws_status_event_key
///   ON metric_anomalies (workspace_id, status, (COALESCE(event_id, id)));
/// ```
#[derive(DeriveMigrationName)]
pub struct Migration;

const INDEX: &str = "idx_metric_anomalies_ws_status_event_key";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Estimated, not counted: `reltuples` is free where `COUNT(*)` on the
        // table we are about to lock is not. It is there so an operator reading
        // a stalled deploy can see what the build was up against, and so a
        // large tenant leaves a trail pointing at the CONCURRENTLY recipe above.
        if let Ok(Some(row)) = manager
            .get_connection()
            .query_one_raw(Statement::from_string(
                manager.get_database_backend(),
                // Narrowed to an ordinary table in the schema this connection
                // actually resolves. `relname` alone can match a partition or a
                // same-named table in another schema, and `query_one_raw` takes
                // whichever row comes first — a number describing the wrong
                // relation is worse than none, since this line is the operator's
                // signal for whether to abort and build CONCURRENTLY.
                "SELECT reltuples::bigint AS estimate FROM pg_class \
                 WHERE relname = 'metric_anomalies' \
                   AND relkind = 'r' \
                   AND pg_table_is_visible(oid)",
            ))
            .await
            && let Ok(estimate) = row.try_get::<i64>("", "estimate")
        {
            // `reltuples` is -1, not 0, for a table that has never been analyzed
            // (PG14+) — which is the *ordinary* case here, since this migration
            // runs moments after the one creating the table on a fresh install.
            // Reported as "unknown" so the line an operator is meant to act on
            // doesn't read as a broken probe.
            let estimated_rows = if estimate < 0 {
                "unknown (never analyzed)".to_string()
            } else {
                estimate.to_string()
            };
            tracing::info!(
                estimated_rows,
                "creating {INDEX}; writes to metric_anomalies block until the build completes"
            );
        }

        manager
            .get_connection()
            .execute_unprepared(&format!(
                "CREATE INDEX IF NOT EXISTS {INDEX} \
                 ON metric_anomalies (workspace_id, status, (COALESCE(event_id, id)))"
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&format!("DROP INDEX IF EXISTS {INDEX}"))
            .await?;
        Ok(())
    }
}
