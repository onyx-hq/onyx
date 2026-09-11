//! BigQuery connector implementation.
//!
//! Uses `gcp_bigquery_client` to run queries via the BigQuery Jobs API.
//! Authentication is via a service account key file.
//!
//! # Query flow
//!
//! 1. Build a `QueryRequest` with `use_legacy_sql: false`.
//! 2. POST via `client.job().query(project_id, request)` — returns `QueryResponse`.
//! 3. Parse column names from `response.schema.fields`.
//! 4. Iterate rows with `ResultSet::new_from_query_response(response)`.
//! 5. For stats, run a second query per column with COUNT, MIN, MAX, AVG,
//!    STDDEV_POP.
//!
//! Schema is introspected from `INFORMATION_SCHEMA.COLUMNS` and cached at
//! construction time because `introspect_schema()` is synchronous.

#![cfg(feature = "bigquery")]

use std::collections::HashMap;

use async_trait::async_trait;
use gcp_bigquery_client::{Client, model::query_request::QueryRequest};

use agentic_core::result::{
    CellValue, ColumnSpec, QueryResult, QueryRow, TypedRowError, TypedRowStream, TypedValue,
};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::bigquery_typed::{bq_field_to_typed, decode_bq_row};
use crate::connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, ResultCap, ResultSummary,
    SchemaColumnInfo, SchemaInfo, SchemaTableInfo, SqlDialect, estimate_row_bytes,
    is_returning_statement, normalize_sql, plan_sql_script,
};

// ── Connector ─────────────────────────────────────────────────────────────────

/// BigQuery connector backed by the `gcp_bigquery_client` crate.
pub struct BigQueryConnector {
    client: Client,
    project_id: String,
    cached_schema: SchemaInfo,
    /// Set when schema pre-fetch fails so `introspect_schema` can surface the error.
    schema_error: Option<String>,
    /// In-pod memory backstop for `execute_query_full` (see [`ResultCap`]).
    result_cap: ResultCap,
}

impl BigQueryConnector {
    /// Create a new connector authenticated with a service-account JSON key file.
    pub async fn new(
        key_path: &str,
        project_id: String,
        datasets: Vec<String>,
    ) -> Result<Self, ConnectorError> {
        let client = Client::from_service_account_key_file(key_path)
            .await
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;

        let (cached_schema, schema_error) =
            match fetch_schema(&client, &project_id, &datasets).await {
                Ok(schema) => (schema, None),
                Err(e) => {
                    tracing::warn!(
                        "bigquery: schema prefetch failed ({e}); schema browsing unavailable"
                    );
                    (SchemaInfo::default(), Some(e.to_string()))
                }
            };

        Ok(Self {
            client,
            project_id,
            cached_schema,
            schema_error,
            result_cap: ResultCap::default(),
        })
    }

    /// Override the [`ResultCap`] memory backstop. Primarily for tests.
    pub fn with_result_cap(mut self, cap: ResultCap) -> Self {
        self.result_cap = cap;
        self
    }
}

// ── DatabaseConnector impl ────────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for BigQueryConnector {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::BigQuery
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "gcp.bigquery"),
            otel.kind = "client",
            db.system.name = "gcp.bigquery",
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
        // The COUNT step below wraps the SQL in `({sql})`, which is invalid
        // for a multi-statement script. Run leading statements as their own
        // jobs for side effects, then sample only the final statement.
        let script = plan_sql_script(sql);
        for stmt in &script.prefix {
            let req = QueryRequest {
                query: stmt.clone(),
                use_legacy_sql: false,
                timeout_ms: Some(180_000),
                ..Default::default()
            };
            self.client
                .job()
                .query(&self.project_id, req)
                .await
                .map_err(|e| ConnectorError::query_failed(stmt.clone(), e.to_string()))?;
        }

        let sql = normalize_sql(&script.final_stmt);

        if !is_returning_statement(sql) {
            if !sql.is_empty() {
                let req = QueryRequest {
                    query: sql.to_string(),
                    use_legacy_sql: false,
                    timeout_ms: Some(180_000),
                    ..Default::default()
                };
                self.client
                    .job()
                    .query(&self.project_id, req)
                    .await
                    .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
            }
            return Ok(ExecutionResult::empty());
        }

        // 1. Run the final query with a row limit.
        let request = QueryRequest {
            query: sql.to_string(),
            use_legacy_sql: false,
            max_results: Some(sample_limit as i32),
            timeout_ms: Some(180_000),
            ..Default::default()
        };

        let response = self
            .client
            .job()
            .query(&self.project_id, request)
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        // 2. Column names and types from the response schema.
        let fields_ref = response.schema.as_ref().and_then(|s| s.fields.as_ref());
        let column_names: Vec<String> = fields_ref
            .map(|fields| fields.iter().map(|f| f.name.clone()).collect())
            .unwrap_or_default();
        let column_types: Vec<Option<String>> = fields_ref
            .map(|fields| {
                fields
                    .iter()
                    .map(|f| {
                        serde_json::to_value(&f.r#type)
                            .ok()
                            .and_then(|v| v.as_str().map(|s| s.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let col_count = column_names.len();

        // 3. Iterate rows via ResultSet.
        let mut rs = gcp_bigquery_client::model::query_response::ResultSet::new_from_query_response(
            response,
        );
        let mut sample_rows: Vec<QueryRow> = Vec::new();
        while rs.next_row() {
            let cells = column_names
                .iter()
                .map(|col| bq_value_to_cell(&rs, col))
                .collect();
            sample_rows.push(QueryRow(cells));
        }

        // 4. Total row count via a separate COUNT query.
        let count_sql = format!("SELECT COUNT(*) AS _cnt FROM ({sql})");
        let count_request = QueryRequest {
            query: count_sql.clone(),
            use_legacy_sql: false,
            timeout_ms: Some(180_000),
            ..Default::default()
        };
        let count_response = self
            .client
            .job()
            .query(&self.project_id, count_request)
            .await
            .map_err(|e| ConnectorError::query_failed(count_sql.clone(), e.to_string()))?;

        let mut count_rs =
            gcp_bigquery_client::model::query_response::ResultSet::new_from_query_response(
                count_response,
            );
        let total_row_count: u64 = if count_rs.next_row() {
            count_rs
                .get_i64_by_name("_cnt")
                .ok()
                .flatten()
                .map(|n| n as u64)
                .unwrap_or(sample_rows.len() as u64)
        } else {
            sample_rows.len() as u64
        };

        // 5. Per-column stats.
        let mut col_stats: Vec<ColumnStats> = Vec::with_capacity(col_count);
        for (idx, col) in column_names.iter().enumerate() {
            let backtick_col = format!("`{}`", col.replace('`', "\\`"));
            let stat_sql = format!(
                "SELECT \
                    COUNTIF({backtick_col} IS NULL) AS nc, \
                    COUNT(DISTINCT {backtick_col}) AS dc, \
                    CAST(MIN({backtick_col}) AS STRING) AS mn, \
                    CAST(MAX({backtick_col}) AS STRING) AS mx, \
                    AVG(SAFE_CAST({backtick_col} AS FLOAT64)) AS avg_v, \
                    STDDEV_POP(SAFE_CAST({backtick_col} AS FLOAT64)) AS sd_v \
                 FROM ({sql})"
            );
            let stat_request = QueryRequest {
                query: stat_sql.clone(),
                use_legacy_sql: false,
                timeout_ms: Some(180_000),
                ..Default::default()
            };

            match self
                .client
                .job()
                .query(&self.project_id, stat_request)
                .await
            {
                Ok(stat_resp) => {
                    let mut stat_rs = gcp_bigquery_client::model::query_response::ResultSet::new_from_query_response(stat_resp);
                    if stat_rs.next_row() {
                        let null_count = stat_rs
                            .get_i64_by_name("nc")
                            .ok()
                            .flatten()
                            .map(|n| n as u64)
                            .unwrap_or(0);
                        let distinct_count = stat_rs
                            .get_i64_by_name("dc")
                            .ok()
                            .flatten()
                            .map(|n| n as u64)
                            .unwrap_or(0);
                        let min_v = stat_rs
                            .get_string_by_name("mn")
                            .ok()
                            .flatten()
                            .map(|s| parse_bq_cell_str(&s))
                            .unwrap_or(CellValue::Null);
                        let max_v = stat_rs
                            .get_string_by_name("mx")
                            .ok()
                            .flatten()
                            .map(|s| parse_bq_cell_str(&s))
                            .unwrap_or(CellValue::Null);
                        let mean = stat_rs.get_f64_by_name("avg_v").ok().flatten();
                        let std_dev = stat_rs.get_f64_by_name("sd_v").ok().flatten();
                        col_stats.push(ColumnStats {
                            name: col.clone(),
                            data_type: column_types.get(idx).cloned().flatten(),
                            null_count,
                            distinct_count: Some(distinct_count),
                            min: Some(min_v),
                            max: Some(max_v),
                            mean,
                            std_dev,
                        });
                    } else {
                        col_stats.push(empty_col_stats(col));
                    }
                }
                Err(_) => {
                    col_stats.push(empty_col_stats(col));
                }
            }
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
            otel.name = %crate::telemetry::span_name(sql, "gcp.bigquery"),
            otel.kind = "client",
            db.system.name = "gcp.bigquery",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let sql = normalize_sql(sql);
        // `execute_query_full` returns the full result, bounded only by the
        // `ResultCap` memory backstop below so an unbounded scan can't OOM the
        // pod (the hot IDE path is also bounded upstream by the `LIMIT 10000`
        // wrap). The `max_results` server-side cap is only used on the bounded
        // sample path (`execute_query`).
        let request = QueryRequest {
            query: sql.to_string(),
            use_legacy_sql: false,
            timeout_ms: Some(180_000),
            ..Default::default()
        };

        let response = self
            .client
            .job()
            .query(&self.project_id, request)
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        // Build `ColumnSpec`s directly from the BigQuery field schema
        // (preserves REPEATED mode + RECORD nesting via the type mapper).
        let columns: Vec<ColumnSpec> = response
            .schema
            .as_ref()
            .and_then(|s| s.fields.as_ref())
            .map(|fields| {
                fields
                    .iter()
                    .map(|f| ColumnSpec {
                        name: f.name.clone(),
                        data_type: bq_field_to_typed(f),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut rs = gcp_bigquery_client::model::query_response::ResultSet::new_from_query_response(
            response,
        );
        let cap = self.result_cap;
        let mut typed_rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = Vec::new();
        let mut bytes: u64 = 0;
        let mut truncated = false;
        while rs.next_row() {
            let decoded = decode_bq_row(&rs, &columns);
            if let Ok(values) = &decoded {
                bytes += estimate_row_bytes(values);
            }
            typed_rows.push(decoded);
            if cap.exceeded(typed_rows.len() as u64, bytes) {
                truncated = true;
                break;
            }
        }

        let stream = TypedRowStream::from_rows(columns, typed_rows);
        Ok(if truncated {
            stream.with_truncation(Arc::new(AtomicBool::new(true)))
        } else {
            stream
        })
    }

    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        if let Some(ref err) = self.schema_error {
            return Err(ConnectorError::ConnectionError(format!(
                "schema introspection failed: {err}"
            )));
        }
        Ok(self.cached_schema.clone())
    }
}

// ── Value helpers ─────────────────────────────────────────────────────────────

/// Read a column value from the current BigQuery `ResultSet` row into a
/// [`CellValue`], trying integer, float, and string in order.
fn bq_value_to_cell(
    rs: &gcp_bigquery_client::model::query_response::ResultSet,
    col: &str,
) -> CellValue {
    // Try integer first.
    if let Ok(Some(n)) = rs.get_i64_by_name(col) {
        return CellValue::Number(n as f64);
    }
    // Try float.
    if let Ok(Some(f)) = rs.get_f64_by_name(col) {
        return CellValue::Number(f);
    }
    // Fall back to string.
    match rs.get_string_by_name(col) {
        Ok(Some(s)) => parse_bq_cell_str(&s),
        _ => CellValue::Null,
    }
}

/// Parse a BigQuery string cell into a [`CellValue`], attempting numeric
/// conversion first.
fn parse_bq_cell_str(s: &str) -> CellValue {
    if let Ok(n) = s.parse::<f64>() {
        CellValue::Number(n)
    } else {
        CellValue::Text(s.to_string())
    }
}

/// Build an all-empty [`ColumnStats`] for columns where stats queries fail.
fn empty_col_stats(col: &str) -> ColumnStats {
    ColumnStats {
        name: col.to_string(),
        data_type: None,
        null_count: 0,
        distinct_count: None,
        min: None,
        max: None,
        mean: None,
        std_dev: None,
    }
}

// ── Schema pre-fetch ──────────────────────────────────────────────────────────

/// Fetch schema across all specified datasets.
///
/// Each dataset is queried separately and table names are prefixed with
/// `dataset.table_name` so tables from different datasets don't collide.
/// Returns an empty schema (no error) when `datasets` is empty.
async fn fetch_schema(
    client: &Client,
    project_id: &str,
    datasets: &[String],
) -> Result<SchemaInfo, ConnectorError> {
    if datasets.is_empty() {
        return Ok(SchemaInfo::default());
    }

    let mut map: HashMap<String, Vec<SchemaColumnInfo>> = HashMap::new();
    let mut errors: Vec<String> = Vec::new();

    for ds in datasets {
        let schema_sql = format!(
            "SELECT table_name, column_name, data_type \
             FROM `{project_id}.{ds}.INFORMATION_SCHEMA.COLUMNS` \
             ORDER BY table_name, ordinal_position"
        );

        let request = QueryRequest {
            query: schema_sql,
            use_legacy_sql: false,
            timeout_ms: Some(60_000),
            ..Default::default()
        };

        let response = match client.job().query(project_id, request).await {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("{ds}: {e}"));
                continue;
            }
        };

        let mut rs = gcp_bigquery_client::model::query_response::ResultSet::new_from_query_response(
            response,
        );
        while rs.next_row() {
            let table = match rs.get_string_by_name("table_name").ok().flatten() {
                Some(t) => t,
                None => continue,
            };
            let column = match rs.get_string_by_name("column_name").ok().flatten() {
                Some(c) => c,
                None => continue,
            };
            let data_type = rs
                .get_string_by_name("data_type")
                .ok()
                .flatten()
                .unwrap_or_default();

            // Prefix table name with dataset so tables from different datasets
            // are distinct and generate correct fully-qualified SQL.
            let qualified = format!("{ds}.{table}");
            map.entry(qualified).or_default().push(SchemaColumnInfo {
                name: column,
                data_type,
                min: None,
                max: None,
                sample_values: vec![],
            });
        }
    }

    // Surface errors only if every dataset failed.
    if map.is_empty() && !errors.is_empty() {
        return Err(ConnectorError::ConnectionError(format!(
            "schema introspection failed: {}",
            errors.join("; ")
        )));
    }

    let tables: Vec<SchemaTableInfo> = map
        .into_iter()
        .map(|(name, columns)| SchemaTableInfo { name, columns })
        .collect();

    let join_keys = detect_join_keys(&tables);
    Ok(SchemaInfo { tables, join_keys })
}

// ── Join key detection ────────────────────────────────────────────────────────

/// Auto-detect join keys: any column ending in `_id` shared across two tables.
fn detect_join_keys(tables: &[SchemaTableInfo]) -> Vec<(String, String, String)> {
    let mut col_to_tables: HashMap<&str, Vec<&str>> = HashMap::new();
    for t in tables {
        for c in &t.columns {
            if c.name.ends_with("_id") {
                col_to_tables
                    .entry(c.name.as_str())
                    .or_default()
                    .push(t.name.as_str());
            }
        }
    }
    let mut keys = Vec::new();
    for (col, tbs) in col_to_tables {
        for i in 0..tbs.len() {
            for j in (i + 1)..tbs.len() {
                keys.push((tbs[i].to_string(), tbs[j].to_string(), col.to_string()));
            }
        }
    }
    keys
}
