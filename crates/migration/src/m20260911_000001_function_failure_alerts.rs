use sea_orm_migration::prelude::*;

/// Name each failed invocation's failure, and remember which ones ops was told about.
///
/// # Why
///
/// In 0.5.140–0.5.144 every `ctx.warehouse.insert` against ClickHouse failed,
/// every failure was written to `app_function_invocations.error`, and nobody
/// on the platform side knew until a customer said so. The rows were there;
/// nothing read them.
///
/// `failure_fingerprint` is a digest of the failure with its data taken out
/// (`custom_apps_functions::failure_signal`), so "has this function failed
/// this way before?" is an indexed lookup instead of a scan over message text.
/// Nullable, no backfill: a success has none, and rows from before this
/// migration were never classified. The alert holds a function's first week
/// quiet while such rows are still in its lookback, which is why it reads this
/// migration's `applied_at` (`custom_apps_functions::failure_alert`).
///
/// `app_function_failure_alerts` is the at-most-once record of a page: one row
/// per (app, function, fingerprint). A replica claims it with an upsert before
/// posting to Slack, so N replicas finalizing the same failure page once.
/// `delivered_at` stays NULL until the post succeeds; a claim left undelivered
/// by a replica that died goes stale and can be taken again. `outcome` says
/// whether the claim paged or was held back (`suppressed:<reason>`). Rows are
/// pruned past 30 days, so the table holds recent failure signals, not history.
///
/// # Locking
///
/// A nullable column with no default is a catalog change: it takes the table's
/// `ACCESS EXCLUSIVE` lock only long enough to write the catalog, and this
/// migration does nothing else to `app_function_invocations` inside its
/// transaction. The index that reads the whole table is built `CONCURRENTLY`
/// by the next migration, outside any transaction, so it never holds that
/// lock — the table only grows, and a plain build would block every
/// invocation for as long as it scans.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "ALTER TABLE app_function_invocations \
             ADD COLUMN IF NOT EXISTS failure_fingerprint TEXT",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS app_function_failure_alerts (\
               app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE, \
               function_name TEXT NOT NULL, \
               failure_fingerprint TEXT NOT NULL, \
               claimed_at TIMESTAMPTZ NOT NULL, \
               delivered_at TIMESTAMPTZ, \
               outcome TEXT NOT NULL DEFAULT 'paged', \
               PRIMARY KEY (app_id, function_name, failure_fingerprint)\
             )",
        )
        .await?;
        // The rate caps read the last few hours of pages, and pruning deletes
        // by age. Built here, on a table this migration just created.
        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_app_function_failure_alerts_claimed_at \
             ON app_function_failure_alerts (claimed_at)",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS app_function_failure_alerts")
            .await?;
        db.execute_unprepared(
            "ALTER TABLE app_function_invocations DROP COLUMN IF EXISTS failure_fingerprint",
        )
        .await?;
        Ok(())
    }
}
