use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, Statement};

/// The index behind "has this function failed this way before?", built
/// without locking `app_function_invocations`.
///
/// `custom_apps_functions::failure_alert` looks a failure up by (app,
/// function, fingerprint) and time, once per failed invocation. Partial,
/// because successes are nearly every row and are never looked up here.
///
/// **Concurrently, outside a transaction.** The table gains a row per
/// invocation and nothing prunes it, so a plain `CREATE INDEX` — which holds a
/// `SHARE` lock for its whole scan, even for a predicate no row matches yet —
/// would block every function invocation for as long as the scan takes.
/// `CONCURRENTLY` refuses to run inside a transaction, hence
/// `use_transaction() = Some(false)`.
///
/// **That opt-out is real since sea-orm-migration 2.0** (#2900). 1.x ran a
/// whole `Migrator::up` inside one Postgres transaction
/// (`exec_with_connection`), so no migration could build concurrently — which
/// older comments here still say. 2.0's `exec_up_with` gives each migration its
/// own transaction unless `use_transaction()` says otherwise, and boot
/// (`cli/commands/serve.rs`) and the test template both call `Migrator::up` on
/// a plain connection, not a transaction. Verified: a test database built that
/// way records this migration and holds the index with `indisvalid = true`.
///
/// Building concurrently still waits for transactions already touching
/// `app_function_invocations` to finish, so a boot migration can pause behind a
/// long one. It never blocks the writers.
///
/// A concurrent build that fails leaves an `INVALID` index behind, which
/// `IF NOT EXISTS` would then happily skip forever. So a leftover invalid
/// index is dropped first, and a rerun of the deploy repairs it.
#[derive(DeriveMigrationName)]
pub struct Migration;

const INDEX: &str = "idx_app_function_invocations_failure";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    fn use_transaction(&self) -> Option<bool> {
        Some(false)
    }

    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let invalid = db
            .query_one_raw(Statement::from_string(
                manager.get_database_backend(),
                format!(
                    "SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                     WHERE c.relname = '{INDEX}' AND pg_table_is_visible(c.oid) \
                       AND NOT i.indisvalid"
                ),
            ))
            .await?;
        if invalid.is_some() {
            db.execute_unprepared(&format!("DROP INDEX CONCURRENTLY IF EXISTS {INDEX}"))
                .await?;
        }
        db.execute_unprepared(&format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS {INDEX} \
             ON app_function_invocations (app_id, function_name, failure_fingerprint, created_at) \
             WHERE failure_fingerprint IS NOT NULL"
        ))
        .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(&format!("DROP INDEX CONCURRENTLY IF EXISTS {INDEX}"))
            .await?;
        Ok(())
    }
}
