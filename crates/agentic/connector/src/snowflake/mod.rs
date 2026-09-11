//! Snowflake connector implementation.
//!
//! Uses `snowflake_api::SnowflakeApi` for authentication and query execution.
//! Because `SnowflakeApi` is not `Sync`, a new API instance is created per
//! query via the `connect()` helper.
//!
//! Snowflake returns results in Arrow format (`QueryResult::Arrow`) or JSON
//! (`QueryResult::Json`).  Arrow batches are decoded using the
//! `arrow_to_cell` helper which handles all common Arrow types.
//!
//! Schema is introspected from `INFORMATION_SCHEMA.COLUMNS` and cached at
//! construction time because `introspect_schema()` is synchronous.

#![cfg(feature = "snowflake")]

mod conversion;
mod schema;
mod typed;

use async_trait::async_trait;
use snowflake_api::{QueryResult as SnowflakeQueryResult, SnowflakeApi};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agentic_core::result::{
    CellValue, ColumnSpec, QueryResult, QueryRow, TypedRowError, TypedRowStream, TypedValue,
};

use crate::config::{SnowflakeAuth, SsoUrlCallback};
use crate::connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, ResultCap, ResultSummary,
    SchemaInfo, SqlDialect, estimate_row_bytes, is_returning_statement, normalize_sql,
    plan_sql_script,
};

use conversion::{arrow_to_cell, json_value_to_cell};
use schema::{StatRow, build_multi_stat_sql, decode_stat_rows, extract_count, fetch_schema};
use typed::{arrow_dtype_to_typed, arrow_to_typed};

pub struct SnowflakeConnector {
    account: String,
    username: String,
    auth: SnowflakeAuth,
    role: Option<String>,
    warehouse: String,
    database: Option<String>,
    schema_str: Option<String>,
    cached_schema: SchemaInfo,
    /// In-pod memory backstop for the row-building loops (see [`ResultCap`]).
    result_cap: ResultCap,
}

impl SnowflakeConnector {
    /// Create a new connector and pre-fetch the database schema.
    pub async fn new(
        account: String,
        username: String,
        auth: SnowflakeAuth,
        role: Option<String>,
        warehouse: String,
        database: Option<String>,
        schema_str: Option<String>,
    ) -> Result<Self, ConnectorError> {
        // Authenticate once to validate credentials.
        let api = build_api(
            &account,
            &username,
            &auth,
            role.as_deref(),
            &warehouse,
            database.as_deref(),
            schema_str.as_deref(),
        )?;
        api.authenticate()
            .await
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;

        let cached_schema = fetch_schema(
            &account,
            &username,
            &auth,
            role.as_deref(),
            &warehouse,
            database.as_deref(),
            schema_str.as_deref(),
        )
        .await
        .unwrap_or_default();

        Ok(Self {
            account,
            username,
            auth,
            role,
            warehouse,
            database,
            schema_str,
            cached_schema,
            result_cap: ResultCap::default(),
        })
    }

    /// Override the [`ResultCap`] memory backstop. Primarily for tests.
    pub fn with_result_cap(mut self, cap: ResultCap) -> Self {
        self.result_cap = cap;
        self
    }

    /// Create and authenticate a fresh [`SnowflakeApi`] for a single query.
    async fn connect(&self) -> Result<SnowflakeApi, ConnectorError> {
        let api = build_api(
            &self.account,
            &self.username,
            &self.auth,
            self.role.as_deref(),
            &self.warehouse,
            self.database.as_deref(),
            self.schema_str.as_deref(),
        )?;
        api.authenticate()
            .await
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;
        Ok(api)
    }
}

/// Build a [`SnowflakeApi`] from resolved credentials without authenticating.
fn build_api(
    account: &str,
    username: &str,
    auth: &SnowflakeAuth,
    role: Option<&str>,
    warehouse: &str,
    database: Option<&str>,
    schema_str: Option<&str>,
) -> Result<SnowflakeApi, ConnectorError> {
    match auth {
        SnowflakeAuth::Password { password } => SnowflakeApi::with_password_auth(
            account,
            Some(warehouse),
            database,
            schema_str,
            username,
            role,
            password,
        )
        .map_err(|e| ConnectorError::ConnectionError(e.to_string())),
        SnowflakeAuth::Browser {
            timeout_secs,
            cache_dir,
            sso_url_callback,
        } => {
            let callback = sso_url_callback.as_ref().map(|SsoUrlCallback(cb)| {
                std::sync::Arc::clone(cb) as std::sync::Arc<dyn Fn(String) + Send + Sync>
            });
            SnowflakeApi::with_externalbrowser_auth_full(
                account,
                Some(warehouse),
                database,
                schema_str,
                username,
                role,
                *timeout_secs,
                true,
                cache_dir.clone(),
                callback,
            )
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))
        }
    }
}

// ── DatabaseConnector impl ────────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for SnowflakeConnector {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Snowflake
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "snowflake"),
            otel.kind = "client",
            db.system.name = "snowflake",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query",
            oxy.db.sample_limit = sample_limit,
        )
    )]
    async fn execute_query(
        &self,
        sql: &str,
        sample_limit: u64,
    ) -> Result<ExecutionResult, ConnectorError> {
        // Snowflake `exec` runs one statement; the per-column stats step
        // below also wraps the SQL in `({sql})`. Run any leading
        // statements for their side effects, then sample only the final
        // statement.
        let script = plan_sql_script(sql);
        let api = self.connect().await?;

        for stmt in &script.prefix {
            api.exec(stmt)
                .await
                .map_err(|e| ConnectorError::query_failed(stmt.clone(), e.to_string()))?;
        }

        let sql = normalize_sql(&script.final_stmt);

        if !is_returning_statement(sql) {
            if !sql.is_empty() {
                api.exec(sql)
                    .await
                    .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
            }
            return Ok(ExecutionResult::empty());
        }

        let sf_result = api
            .exec(sql)
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        let (column_names, column_types, mut sample_rows) = match sf_result {
            SnowflakeQueryResult::Arrow(batches) => {
                let (columns, types) = batches
                    .first()
                    .map(|b| {
                        let cols = b
                            .schema()
                            .fields()
                            .iter()
                            .map(|f| f.name().clone())
                            .collect::<Vec<_>>();
                        let tys = b
                            .schema()
                            .fields()
                            .iter()
                            .map(|f| Some(format!("{}", f.data_type())))
                            .collect::<Vec<_>>();
                        (cols, tys)
                    })
                    .unwrap_or_default();

                // Build only up to `sample_limit` rows — `total_row_count` comes
                // from a separate stats/count query below, so we never need to
                // materialize the full result just to sample it. (The driver's
                // `exec` above already buffered the batches; this bounds the
                // second copy.)
                let mut rows: Vec<QueryRow> = Vec::new();
                'arrow_sample: for batch in &batches {
                    for row_idx in 0..batch.num_rows() {
                        if rows.len() as u64 >= sample_limit {
                            break 'arrow_sample;
                        }
                        let cells = (0..batch.num_columns())
                            .map(|col_idx| arrow_to_cell(batch.column(col_idx).as_ref(), row_idx))
                            .collect();
                        rows.push(QueryRow(cells));
                    }
                }
                (columns, types, rows)
            }
            SnowflakeQueryResult::Json(json_rows) => {
                // JsonResult.value is an array-of-arrays: [[v1, v2], [v3, v4], ...]
                // JsonResult.schema is Vec<FieldSchema> with column names.
                let columns: Vec<String> =
                    json_rows.schema.iter().map(|f| f.name.clone()).collect();
                let types: Vec<Option<String>> = vec![None; columns.len()];

                let rows = json_rows
                    .value
                    .as_array()
                    .map(|outer| {
                        outer
                            .iter()
                            .filter_map(|r| r.as_array())
                            // Only the sample is needed; `total_row_count` is
                            // sourced separately below.
                            .take(sample_limit as usize)
                            .map(|vals| {
                                let cells = vals.iter().map(json_value_to_cell).collect();
                                QueryRow(cells)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (columns, types, rows)
            }
            SnowflakeQueryResult::Empty => (Vec::new(), Vec::new(), Vec::new()),
        };

        sample_rows.truncate(sample_limit as usize);

        let col_count = column_names.len();

        // Per-column stats + total row count via a single round-trip.
        // Previously this was one `connect()` + `exec()` per column — 20 columns
        // meant 20 sequential authentication round-trips to Snowflake, plus a
        // 21st for the total row count. The new query wraps the user's SQL in a
        // CTE (so the source is scanned / planned once), UNION ALLs one
        // aggregate SELECT per column, and carries the total row count on every
        // row via a scalar subquery.
        let col_stats: Vec<ColumnStats>;
        let total_row_count: u64;

        if col_count == 0 {
            col_stats = Vec::new();
            let count_sql = format!("SELECT COUNT(*) FROM ({sql})");
            let count_api = self.connect().await?;
            let count_result = count_api
                .exec(&count_sql)
                .await
                .map_err(|e| ConnectorError::query_failed(count_sql.clone(), e.to_string()))?;
            total_row_count = extract_count(count_result);
        } else {
            let types: Vec<Option<&str>> = column_names
                .iter()
                .enumerate()
                .map(|(idx, _)| column_types.get(idx).and_then(|t| t.as_deref()))
                .collect();
            let stats_sql = build_multi_stat_sql(&column_names, &types, sql);
            let stats_api = self.connect().await?;
            let stats_result = stats_api
                .exec(&stats_sql)
                .await
                .map_err(|e| ConnectorError::query_failed(stats_sql.clone(), e.to_string()))?;

            let mut slots: Vec<Option<StatRow>> = (0..col_count).map(|_| None).collect();
            let mut total_from_stats: u64 = 0;
            for row in decode_stat_rows(stats_result) {
                total_from_stats = row.total;
                let idx = row.col_idx as usize;
                if idx < col_count {
                    slots[idx] = Some(row);
                }
            }
            total_row_count = total_from_stats;

            col_stats = column_names
                .iter()
                .enumerate()
                .map(|(idx, col)| {
                    let row = slots[idx].take();
                    ColumnStats {
                        name: col.clone(),
                        data_type: column_types.get(idx).cloned().flatten(),
                        null_count: row.as_ref().map(|r| r.null_count).unwrap_or(0),
                        distinct_count: row.as_ref().map(|r| r.distinct_count),
                        min: Some(
                            row.as_ref()
                                .map(|r| r.min.clone())
                                .unwrap_or(CellValue::Null),
                        ),
                        max: Some(
                            row.as_ref()
                                .map(|r| r.max.clone())
                                .unwrap_or(CellValue::Null),
                        ),
                        mean: row.as_ref().and_then(|r| r.mean),
                        std_dev: row.as_ref().and_then(|r| r.std_dev),
                    }
                })
                .collect();
        }

        let truncated = (sample_rows.len() as u64) < total_row_count;
        Ok(ExecutionResult {
            result: QueryResult {
                columns: column_names,
                rows: sample_rows,
                total_row_count,
                truncated,
            },
            summary: ResultSummary {
                row_count: total_row_count,
                columns: col_stats,
            },
        })
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "snowflake"),
            otel.kind = "client",
            db.system.name = "snowflake",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let sql = normalize_sql(sql);
        let api = self.connect().await?;
        let sf_result = api
            .exec(sql)
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        // ResultCap backstop on the row-building loops. NOTE: `api.exec` above
        // already materialized the full result (the snowflake-api driver has no
        // streaming exec), so this bounds the *typed-row* copy + downstream
        // Parquet/disk and flags truncation — it does not prevent the driver's
        // own materialization. The hot IDE path is bounded upstream by the
        // `LIMIT 10000` wrap; pushing a server-side `ROWS_PER_RESULTSET` is the
        // follow-up to bound `exec` itself. See the large-query design note.
        let cap = self.result_cap;
        let mut truncated = false;

        // Arrow is the common case — Snowflake's driver decodes to
        // `Vec<RecordBatch>` directly. JSON fallback mirrors the existing
        // `execute_query` behaviour (coerce everything through
        // `json_value_to_cell`).
        let (columns, typed_rows): (Vec<ColumnSpec>, Vec<Result<Vec<TypedValue>, TypedRowError>>) =
            match sf_result {
                SnowflakeQueryResult::Arrow(batches) => {
                    let columns: Vec<ColumnSpec> = batches
                        .first()
                        .map(|b| {
                            b.schema()
                                .fields()
                                .iter()
                                .map(|f| ColumnSpec {
                                    name: f.name().clone(),
                                    data_type: arrow_dtype_to_typed(f.data_type()),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let column_types: Vec<agentic_core::result::TypedDataType> =
                        columns.iter().map(|c| c.data_type.clone()).collect();

                    let mut rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = Vec::new();
                    let mut bytes: u64 = 0;
                    'arrow: for batch in &batches {
                        for row_idx in 0..batch.num_rows() {
                            let cells: Vec<TypedValue> = (0..batch.num_columns())
                                .map(|col_idx| {
                                    arrow_to_typed(
                                        batch.column(col_idx).as_ref(),
                                        row_idx,
                                        &column_types[col_idx],
                                    )
                                })
                                .collect();
                            bytes += estimate_row_bytes(&cells);
                            rows.push(Ok(cells));
                            if cap.exceeded(rows.len() as u64, bytes) {
                                truncated = true;
                                break 'arrow;
                            }
                        }
                    }
                    (columns, rows)
                }
                SnowflakeQueryResult::Json(json_rows) => {
                    // JSON path: no column-type metadata, so everything is
                    // `Text` (or `Null`) — matches the fidelity users already
                    // get from the bounded-sample path.
                    let columns: Vec<ColumnSpec> = json_rows
                        .schema
                        .iter()
                        .map(|f| ColumnSpec {
                            name: f.name.clone(),
                            data_type: agentic_core::result::TypedDataType::Text,
                        })
                        .collect();
                    let mut rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = Vec::new();
                    let mut bytes: u64 = 0;
                    if let Some(outer) = json_rows.value.as_array() {
                        'json: for r in outer.iter().filter_map(|r| r.as_array()) {
                            let cells: Vec<TypedValue> = r
                                .iter()
                                .map(|v| match v {
                                    serde_json::Value::Null => TypedValue::Null,
                                    serde_json::Value::String(s) => TypedValue::Text(s.clone()),
                                    other => TypedValue::Text(other.to_string()),
                                })
                                .collect();
                            bytes += estimate_row_bytes(&cells);
                            rows.push(Ok(cells));
                            if cap.exceeded(rows.len() as u64, bytes) {
                                truncated = true;
                                break 'json;
                            }
                        }
                    }
                    (columns, rows)
                }
                SnowflakeQueryResult::Empty => (Vec::new(), Vec::new()),
            };

        let stream = TypedRowStream::from_rows(columns, typed_rows);
        Ok(if truncated {
            stream.with_truncation(Arc::new(AtomicBool::new(true)))
        } else {
            stream
        })
    }

    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        Ok(self.cached_schema.clone())
    }

    #[cfg(feature = "arrow")]
    fn as_arrow(&self) -> Option<&dyn crate::connector::AsArrowConnector> {
        Some(self)
    }
}

// ── AsArrowConnector impl (feature = "arrow") ────────────────────────────────

#[cfg(feature = "arrow")]
#[async_trait]
impl crate::connector::AsArrowConnector for SnowflakeConnector {
    async fn execute_query_arrow(
        &self,
        sql: &str,
    ) -> Result<crate::connector::ArrowQueryStream, ConnectorError> {
        let sql = normalize_sql(sql);
        let api = self.connect().await?;
        let sf_result = api
            .exec(sql)
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        match sf_result {
            SnowflakeQueryResult::Arrow(batches) => {
                let schema = batches.first().map(|b| b.schema()).ok_or_else(|| {
                    ConnectorError::Other(
                        "snowflake returned an Arrow result with zero batches".into(),
                    )
                })?;
                Ok(crate::connector::ArrowQueryStream {
                    schema,
                    batches: Box::pin(futures::stream::iter(batches.into_iter().map(Ok))),
                })
            }
            SnowflakeQueryResult::Json(_) => Err(ConnectorError::Other(
                "snowflake returned a JSON result; Arrow path unavailable for this query".into(),
            )),
            SnowflakeQueryResult::Empty => Err(ConnectorError::Other(
                "snowflake returned no rows; cannot infer Arrow schema".into(),
            )),
        }
    }
}

// ── Helper functions ──────────────────────────────────────────────────────────
