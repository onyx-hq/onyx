//! Database connector trait, configuration, and backend implementations.
//!
//! # Backends
//!
//! | Feature      | Connector               | Crate                  |
//! |--------------|-------------------------|------------------------|
//! | `duckdb`     | [`DuckDbConnector`]     | `duckdb`               |
//! | `postgres`   | [`PostgresConnector`]   | `tokio-postgres`       |
//! | `clickhouse` | [`ClickHouseConnector`] | `reqwest` (HTTP API)   |
//! | `snowflake`  | [`SnowflakeConnector`]  | `snowflake-api`        |
//! | `bigquery`   | [`BigQueryConnector`]   | `gcp-bigquery-client`  |

pub mod config;
pub mod connector;
pub mod telemetry;
#[cfg(feature = "transactions")]
pub mod transaction;

#[cfg(feature = "duckdb")]
pub mod duckdb;

#[cfg(feature = "postgres")]
pub mod postgres;

/// Postgres implementation of [`transaction::SqlTransaction`] — the pinned
/// connection behind `ctx.tx()`. Gated on `postgres` only.
#[cfg(feature = "postgres")]
pub mod postgres_tx;

/// Shared typed-row helpers used by the `postgres` backend's
/// `execute_query_full`. Gated on `postgres` only.
#[cfg(feature = "postgres")]
mod postgres_typed;

/// Driver-error → author-readable, shared by BOTH Postgres paths. See the
/// module docs for why `format!("{e}")` on a `tokio_postgres::Error` loses
/// everything that identifies the failure.
#[cfg(feature = "postgres")]
mod pg_error;

#[cfg(feature = "mysql")]
pub mod mysql;

#[cfg(feature = "domo")]
pub mod domo;

#[cfg(feature = "clickhouse")]
pub mod clickhouse;

/// Typed-row helpers for the ClickHouse backend's `execute_query_full`.
/// Parses CH type strings (with `Nullable` / `LowCardinality` wrappers) and
/// JSONCompact cell values into `TypedValue`s.
#[cfg(feature = "clickhouse")]
mod clickhouse_typed;

#[cfg(feature = "snowflake")]
pub mod snowflake;

#[cfg(feature = "bigquery")]
pub mod bigquery;

/// Typed-row helpers for the BigQuery backend's `execute_query_full`:
/// `FieldType` → `TypedDataType` and `ResultSet` cell decoding.
#[cfg(feature = "bigquery")]
mod bigquery_typed;

// ── Config re-exports ─────────────────────────────────────────────────────────

pub use config::{
    BigQueryConfig, ClickHouseConfig, ConnectorConfig, DomoConfig, DuckDbConfig,
    DuckDbLoadStrategy, DuckDbRawConfig, DuckDbUrlConfig, MysqlConfig, PostgresConfig,
    SnowflakeAuth, SnowflakeConfig, SsoUrlCallback,
};

// ── Trait re-exports ──────────────────────────────────────────────────────────

pub use connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, QueryFailedDetails, ResultCap,
    ResultSummary, SchemaColumnInfo, SchemaInfo, SchemaTableInfo, SqlDialect, SqlScript,
    estimate_row_bytes, guard_row_stream, is_returning_statement, is_wrappable_select,
    normalize_sql, plan_sql_script, split_sql_statements, with_trailing_comment,
};

#[cfg(feature = "transactions")]
pub use transaction::{SqlTransaction, TxParams};

#[cfg(feature = "arrow")]
pub use connector::{ArrowQueryStream, AsArrowConnector};

// ── Connector re-exports ──────────────────────────────────────────────────────

#[cfg(feature = "duckdb")]
pub use duckdb::{DuckDbConnection, DuckDbConnector, LoadStrategy, TableInfo, TableSource};

#[cfg(feature = "postgres")]
pub use postgres::PostgresConnector;

#[cfg(feature = "mysql")]
pub use mysql::MysqlConnector;

#[cfg(feature = "domo")]
pub use domo::DomoConnector;

#[cfg(feature = "clickhouse")]
pub use clickhouse::ClickHouseConnector;

#[cfg(feature = "snowflake")]
pub use snowflake::SnowflakeConnector;

#[cfg(feature = "bigquery")]
pub use bigquery::BigQueryConnector;

// ── build_connector ───────────────────────────────────────────────────────────

/// Construct a `Box<dyn DatabaseConnector>` from a sync-compatible config.
///
/// Only [`ConnectorConfig::DuckDb`], [`ConnectorConfig::DuckDbRaw`], and
/// [`ConnectorConfig::DuckDbUrl`] are supported here (DuckDB opens
/// synchronously).  For all other variants use [`build_connector_async`].
///
/// Returns `Err(ConnectorError::ConnectionError)` if the requested backend is
/// not compiled in (missing feature flag) or if the backend itself fails to
/// initialise.
pub fn build_connector(cfg: ConnectorConfig) -> Result<Box<dyn DatabaseConnector>, ConnectorError> {
    match cfg {
        ConnectorConfig::DuckDb(c) => {
            #[cfg(feature = "duckdb")]
            {
                use crate::duckdb::{DuckDbConnector, LoadStrategy};

                let strategy = match c.load_strategy {
                    DuckDbLoadStrategy::View => LoadStrategy::View,
                    DuckDbLoadStrategy::Materialized => LoadStrategy::Materialized,
                };
                let connector = DuckDbConnector::from_directory(&c.data_dir, strategy)
                    .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;
                Ok(Box::new(connector))
            }
            #[cfg(not(feature = "duckdb"))]
            {
                let _ = c;
                Err(ConnectorError::ConnectionError(
                    "DuckDB support is not compiled in — enable the 'duckdb' feature on \
                     agentic-connector"
                        .into(),
                ))
            }
        }
        ConnectorConfig::DuckDbRaw(c) => {
            #[cfg(feature = "duckdb")]
            {
                use crate::duckdb::DuckDbConnector;
                use ::duckdb::Connection;

                let conn = Connection::open_in_memory()
                    .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;
                for stmt in &c.init_statements {
                    conn.execute_batch(stmt)
                        .map_err(|e| ConnectorError::query_failed(stmt.clone(), e.to_string()))?;
                }
                Ok(Box::new(DuckDbConnector::new(conn)))
            }
            #[cfg(not(feature = "duckdb"))]
            {
                let _ = c;
                Err(ConnectorError::ConnectionError(
                    "DuckDB support is not compiled in — enable the 'duckdb' feature on \
                     agentic-connector"
                        .into(),
                ))
            }
        }
        ConnectorConfig::DuckDbUrl(c) => {
            #[cfg(feature = "duckdb")]
            {
                use crate::duckdb::DuckDbConnector;
                use ::duckdb::Connection;

                let conn = Connection::open(&c.url)
                    .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;
                Ok(Box::new(DuckDbConnector::new(conn)))
            }
            #[cfg(not(feature = "duckdb"))]
            {
                let _ = c;
                Err(ConnectorError::ConnectionError(
                    "DuckDB support is not compiled in — enable the 'duckdb' feature on \
                     agentic-connector"
                        .into(),
                ))
            }
        }
        other => Err(ConnectorError::ConnectionError(format!(
            "use build_connector_async for {:?}",
            std::mem::discriminant(&other)
        ))),
    }
}

/// Construct a `Box<dyn DatabaseConnector>` from any config, including those
/// that require async connection setup (Postgres, ClickHouse, Snowflake, BigQuery).
pub async fn build_connector_async(
    cfg: ConnectorConfig,
) -> Result<Box<dyn DatabaseConnector>, ConnectorError> {
    match cfg {
        ConnectorConfig::DuckDb(_)
        | ConnectorConfig::DuckDbRaw(_)
        | ConnectorConfig::DuckDbUrl(_) => {
            // DuckDB variants open synchronously — delegate to spawn_blocking
            // so we don't block the async runtime.
            let result: Result<Box<dyn DatabaseConnector>, ConnectorError> =
                tokio::task::spawn_blocking(move || build_connector(cfg))
                    .await
                    .map_err(|e| {
                        ConnectorError::ConnectionError(format!("task join error: {e}"))
                    })?;
            result
        }

        #[cfg(feature = "postgres")]
        ConnectorConfig::Postgres(c) | ConnectorConfig::Redshift(c) => {
            let conn = PostgresConnector::with_sslmode(
                &c.host,
                c.port,
                &c.user,
                &c.password,
                &c.database,
                c.sslmode.as_deref(),
            );
            Ok(Box::new(conn))
        }

        #[cfg(feature = "mysql")]
        ConnectorConfig::Mysql(c) => {
            let conn =
                MysqlConnector::new(&c.host, c.port, &c.user, &c.password, &c.database).await?;
            Ok(Box::new(conn))
        }

        #[cfg(feature = "domo")]
        ConnectorConfig::Domo(c) => {
            let conn = DomoConnector::new(c.base_url, c.developer_token, c.dataset_id).await?;
            Ok(Box::new(conn))
        }

        #[cfg(feature = "clickhouse")]
        ConnectorConfig::ClickHouse(c) => {
            let conn = ClickHouseConnector::new(c.url, c.user, c.password, c.database).await?;
            Ok(Box::new(conn))
        }

        #[cfg(feature = "snowflake")]
        ConnectorConfig::Snowflake(c) => {
            let conn = SnowflakeConnector::new(
                c.account,
                c.username,
                c.auth,
                c.role,
                c.warehouse,
                c.database,
                c.schema,
            )
            .await?;
            Ok(Box::new(conn))
        }

        #[cfg(feature = "bigquery")]
        ConnectorConfig::BigQuery(c) => {
            let conn = BigQueryConnector::new(&c.key_path, c.project_id, c.datasets).await?;
            Ok(Box::new(conn))
        }

        #[allow(unreachable_patterns)]
        other => Err(ConnectorError::ConnectionError(format!(
            "backend not compiled in for {:?}",
            std::mem::discriminant(&other)
        ))),
    }
}

/// Build multiple named connectors from resolved configs.
///
/// Individual connector failures are logged and skipped — only successfully
/// opened connectors appear in the returned map.  An empty map is perfectly
/// valid (the solver will catch it as `ConfigError::NoDatabases`).
pub async fn build_named_connectors(
    configs: Vec<(String, ConnectorConfig)>,
) -> std::collections::HashMap<String, std::sync::Arc<dyn DatabaseConnector>> {
    let mut result = std::collections::HashMap::new();
    for (name, config) in configs {
        match build_connector_async(config).await {
            Ok(connector) => {
                result.insert(name, std::sync::Arc::from(connector));
            }
            Err(e) => {
                tracing::warn!(connector = %name, "skipping connector: {e}");
            }
        }
    }
    result
}
