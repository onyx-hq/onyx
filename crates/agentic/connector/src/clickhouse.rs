//! ClickHouse connector implementation via the HTTP API.
//!
//! ClickHouse exposes an HTTP interface (default port 8123).  Each query is
//! a POST request with the SQL in the body and ` FORMAT JSONCompact` appended.
//! Responses look like:
//!
//! ```json
//! {"meta":[{"name":"col","type":"Int64"}],"data":[[1],[2]],"rows":2}
//! ```
//!
//! Because ClickHouse does not support ANSI temporary tables, all temp-table
//! operations are replaced by subqueries:
//!
//! - Count:  `SELECT count() FROM ({sql})`
//! - Sample: `SELECT * FROM ({sql}) LIMIT {n} FORMAT JSONCompact`
//! - Stats:  per-column aggregation inside `FROM ({sql})`
//!
//! Schema is introspected from `system.columns` on `prepare_schema` and cached
//! from then on — never at construction, which every query would otherwise pay.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use agentic_core::result::{
    CellValue, ColumnSpec, QueryResult, QueryRow, TypedRowError, TypedRowStream, TypedValue,
};

use crate::clickhouse_typed::{ch_type_to_typed, parse_ch_cell};
use crate::connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, ResultSummary,
    SchemaColumnInfo, SchemaInfo, SchemaTableInfo, SqlDialect, SqlScript, is_returning_statement,
    normalize_sql, plan_sql_script,
};

// ── HTTP response types ────────────────────────────────────────────────────────

/// Parsed ClickHouse JSONCompact response.
#[derive(Debug, Deserialize)]
struct ChResponse {
    meta: Vec<ChMeta>,
    data: Vec<Vec<Value>>,
    #[allow(dead_code)]
    #[serde(default)]
    rows: u64,
}

#[derive(Debug, Deserialize)]
struct ChMeta {
    name: String,
    #[serde(default)]
    r#type: Option<String>,
}

// ── Type classification ────────────────────────────────────────────────────────

/// Broad category used to pick the right avg/stddev expression for a column.
///
/// ClickHouse's `toFloat64OrNull` only accepts `String` input. Wrapping an
/// already-numeric column (e.g. the result of `SUM(...)`) produces:
/// `Illegal type Float64 of first argument of function toFloat64OrNull`.
/// So numeric columns must use `avg` / `stddevPop` directly.
enum TypeCategory {
    /// Numeric types — avg/stddevPop can read the column directly.
    Numeric,
    /// String types — use toFloat64OrNull so non-numeric strings become NULL.
    String,
    /// Everything else (Date, DateTime, UUID, …) — skip mean/stddev.
    Other,
}

/// Classify a ClickHouse column type string into a [`TypeCategory`].
///
/// Handles bare types (`Float64`, `Int32`) plus `Nullable(...)` and
/// `LowCardinality(...)` wrappers, including arbitrary nesting like
/// `LowCardinality(Nullable(String))` or `Nullable(LowCardinality(FixedString(16)))`.
fn clickhouse_type_category(raw: &str) -> TypeCategory {
    // Repeatedly strip Nullable(...) and LowCardinality(...) wrappers until the
    // inner type stands alone. ClickHouse allows them in either order.
    let mut inner = raw.trim();
    loop {
        let stripped = inner
            .strip_prefix("Nullable(")
            .or_else(|| inner.strip_prefix("LowCardinality("))
            .and_then(|s| s.strip_suffix(')'));
        match stripped {
            Some(s) => inner = s.trim(),
            None => break,
        }
    }

    // Strip parenthesized precision/scale (e.g. "Decimal(18, 4)" → "Decimal",
    // "FixedString(16)" → "FixedString").
    let base = inner.split('(').next().unwrap_or(inner).trim();

    match base {
        "Float32" | "Float64" | "Int8" | "Int16" | "Int32" | "Int64" | "Int128" | "Int256"
        | "UInt8" | "UInt16" | "UInt32" | "UInt64" | "UInt128" | "UInt256" | "Decimal"
        | "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => TypeCategory::Numeric,
        "String" | "FixedString" => TypeCategory::String,
        _ => TypeCategory::Other,
    }
}

// ── Value converter ────────────────────────────────────────────────────────────

/// Convert a `serde_json::Value` cell from a JSONCompact row into a [`CellValue`].
fn json_to_cell(v: &Value) -> CellValue {
    match v {
        Value::Null => CellValue::Null,
        Value::Number(n) => CellValue::Number(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => CellValue::Number(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            // ClickHouse returns numbers as strings for many types.
            if let Ok(n) = s.parse::<f64>() {
                CellValue::Number(n)
            } else {
                CellValue::Text(s.clone())
            }
        }
        other => CellValue::Text(other.to_string()),
    }
}

// ── Connector ─────────────────────────────────────────────────────────────────

/// ClickHouse connector that speaks the HTTP JSON API.
pub struct ClickHouseConnector {
    client: reqwest::Client,
    url: String,
    user: String,
    password: String,
    database: String,
    /// Filled by [`prepare_schema`](DatabaseConnector::prepare_schema), NOT by
    /// [`new`](Self::new). Fetching it eagerly cost a full `system.columns`
    /// scan on every connector build, and connectors are built per request —
    /// so every query, `SELECT 1` included, paid for a catalog read it then
    /// threw away. The SQL execution paths never call `introspect_schema`; the
    /// two that do — the schema browser and the analytics agent's
    /// `list_tables` / `describe_table` — await `prepare_schema` first.
    ///
    /// `None` until `prepare_schema` has run, so "never prepared" is
    /// distinguishable from "prepared, and the credential sees no tables":
    /// a missed `prepare_schema` then errors instead of answering "no tables".
    cached_schema: std::sync::RwLock<Option<SchemaInfo>>,
    /// Set when the pre-fetch fails so `introspect_schema` can surface it.
    schema_error: std::sync::RwLock<Option<String>>,
    /// Per-result byte ceiling sent with every query (see `MAX_RESULT_BYTES`).
    /// A field rather than a constant so tests can force the overflow guard to
    /// trip on a small result.
    max_result_bytes: u64,
}

impl ClickHouseConnector {
    /// Build a connector that talks to ClickHouse over its HTTP interface.
    ///
    /// Does no I/O: schema is fetched lazily by `prepare_schema`, the same
    /// posture `PostgresConnector` takes. `url` should be the base URL
    /// including scheme and port, e.g. `http://localhost:8123`.
    ///
    /// Because it does no I/O, `new` is **no longer a connectivity check**: a
    /// bad URL or credential now first surfaces at query time (or at
    /// `prepare_schema`), not at connector build. Still `async` and still
    /// fallible so every call site keeps compiling — and so a future version
    /// can go back to doing work here.
    pub async fn new(
        url: String,
        user: String,
        password: String,
        database: String,
    ) -> Result<Self, ConnectorError> {
        Ok(Self {
            client: reqwest::Client::new(),
            url,
            user,
            password,
            database,
            cached_schema: std::sync::RwLock::new(None),
            schema_error: std::sync::RwLock::new(None),
            max_result_bytes: MAX_RESULT_BYTES,
        })
    }

    /// Override the per-result byte ceiling (default [`MAX_RESULT_BYTES`]).
    /// Primarily for tests that need to trip the overflow guard on a small
    /// result; production constructs via [`new`](Self::new) with the default.
    pub fn with_max_result_bytes(mut self, bytes: u64) -> Self {
        self.max_result_bytes = bytes;
        self
    }

    /// Execute a SQL string against ClickHouse via HTTP, returning the parsed
    /// JSONCompact response.
    async fn http_query(&self, sql: &str) -> Result<ChResponse, ConnectorError> {
        http_query(
            &self.client,
            &self.url,
            &self.user,
            &self.password,
            &self.database,
            self.max_result_bytes,
            sql,
        )
        .await
    }

    /// Execute a side-effect statement (DDL/DML) via HTTP, discarding any
    /// body. Unlike [`http_query`](Self::http_query) this does not append
    /// `FORMAT JSONCompact`, which would make a `CREATE`/`INSERT`
    /// statement a syntax error. `settings` travel as URL parameters, beside
    /// the SQL rather than inside it.
    async fn http_exec(&self, sql: &str, settings: &[(&str, &str)]) -> Result<(), ConnectorError> {
        let mut request = self.client.post(&self.url);
        if !settings.is_empty() {
            request = request.query(settings);
        }
        let response = request
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .header("X-ClickHouse-Database", &self.database)
            .body(sql.to_string())
            .send()
            .await
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ConnectorError::query_failed(
                sql.to_string(),
                format!("HTTP {status}: {text}"),
            ));
        }
        Ok(())
    }

    /// Run a script's leading statements for their side effects, and its final
    /// statement too when that one returns no rows. Hands the final statement
    /// back when it does return rows, for the caller to sample.
    async fn run_side_effects<'s>(
        &self,
        script: &'s SqlScript,
        settings: &[(&str, &str)],
    ) -> Result<Option<&'s str>, ConnectorError> {
        for stmt in &script.prefix {
            self.http_exec(stmt, settings).await?;
        }
        let stmt = normalize_sql(&script.final_stmt);
        if is_returning_statement(stmt) {
            return Ok(Some(stmt));
        }
        if !stmt.is_empty() {
            self.http_exec(stmt, settings).await?;
        }
        Ok(None)
    }
}

// ── HTTP helper ────────────────────────────────────────────────────────────────

/// Server-side ceiling on the bytes ClickHouse will assemble for a single
/// result set. Sent as a query-string setting on every result-returning POST,
/// paired with `result_overflow_mode=break` so ClickHouse stops and returns a
/// *partial* result instead of streaming an unbounded payload back.
///
/// Why this matters: the connector buffers the entire HTTP body
/// (`response.text()`), JSON-parses it into `Vec<Vec<Value>>`, then
/// re-materializes every cell into typed rows — roughly 3x the payload live at
/// once. An uncapped `SELECT col FROM huge_table` therefore peaks in the
/// gigabytes and OOM-kills the (multi-tenant) pod, taking every other tenant on
/// the replica down with it. 256 MiB of result keeps the transient peak well
/// under 1 GiB even with the 3x amplification. This is a last-resort floor: the
/// hot ad-hoc paths also inject an outer row `LIMIT`, so it only bites callers
/// that bypass that (or genuinely wide rows).
const MAX_RESULT_BYTES: u64 = 256 * 1024 * 1024;

/// ClickHouse HTTP settings (passed as URL query params) that bound a single
/// result's memory footprint. `result_overflow_mode=break` makes ClickHouse
/// stop and return a *partial* result on overflow rather than throwing, so a
/// runaway scan degrades gracefully instead of failing or OOM-killing the pod.
fn result_guard_params(max_result_bytes: u64) -> [(&'static str, String); 2] {
    [
        ("max_result_bytes", max_result_bytes.to_string()),
        ("result_overflow_mode", "break".to_string()),
    ]
}

/// POST `sql` to the ClickHouse HTTP endpoint and parse the JSONCompact response.
async fn http_query(
    client: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    database: &str,
    max_result_bytes: u64,
    sql: &str,
) -> Result<ChResponse, ConnectorError> {
    let body = format!("{sql} FORMAT JSONCompact");

    let response = client
        .post(url)
        // Memory guard: cap the assembled result and break (return partial)
        // rather than throw, so a runaway scan degrades gracefully instead of
        // OOM-killing the pod. See `MAX_RESULT_BYTES` / `result_guard_params`.
        .query(&result_guard_params(max_result_bytes))
        .header("X-ClickHouse-User", user)
        .header("X-ClickHouse-Key", password)
        .header("X-ClickHouse-Database", database)
        .body(body.clone())
        .send()
        .await
        .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(ConnectorError::query_failed(
            sql.to_string(),
            format!("HTTP {status}: {text}"),
        ));
    }

    let text = response
        .text()
        .await
        .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

    serde_json::from_str::<ChResponse>(&text).map_err(|e| {
        ConnectorError::query_failed(
            sql.to_string(),
            format!("JSON parse error: {e}\nResponse: {text}"),
        )
    })
}

// ── DatabaseConnector impl ────────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for ClickHouseConnector {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::CLICKHOUSE
    }

    /// The tag travels as the `log_comment` setting, never in the SQL.
    ///
    /// ClickHouse reads everything after `VALUES` or `FORMAT` as the row
    /// stream, so the sqlcommenter default — a trailing comment — is parsed as
    /// one more row and the insert is refused whole (`Code: 27 … expected '('
    /// before: '/*oxy.app=…'`). That failed every `ctx.warehouse.insert` in
    /// 0.5.140–0.5.144. A comment in front instead works only while every
    /// caller classifies every statement correctly; a comment-prefixed INSERT
    /// defeated the first attempt. `log_comment` is ClickHouse's channel for
    /// exactly this: the SQL goes byte for byte, and the tag lands in
    /// `system.query_log.log_comment`, queryable without parsing the statement.
    ///
    /// A final statement that returns rows runs untagged, the way
    /// `execute_statement` always ran it: reads leave no audit record, and
    /// what reaches the engine for one is this connector's count and sample
    /// queries, not the statement the caller wrote.
    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "clickhouse"),
            otel.kind = "client",
            db.system.name = "clickhouse",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_statement_tagged",
        )
    )]
    async fn execute_statement_tagged(&self, sql: &str, tag: &str) -> Result<(), ConnectorError> {
        let script = plan_sql_script(sql);
        match self
            .run_side_effects(&script, &[("log_comment", tag)])
            .await?
        {
            Some(read) => self.execute_query(read, 0).await.map(|_| ()),
            None => Ok(()),
        }
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "clickhouse"),
            otel.kind = "client",
            db.system.name = "clickhouse",
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
        // The count/sample steps wrap the SQL in `({sql})`; a
        // multi-statement script there is a parser error. Run leading
        // statements for side effects, sample only the final statement.
        let script = plan_sql_script(sql);
        let Some(sql) = self.run_side_effects(&script, &[]).await? else {
            return Ok(ExecutionResult::empty());
        };

        // 1. Total row count via subquery.
        let count_sql = format!("SELECT count() FROM ({sql})");
        let count_resp = self.http_query(&count_sql).await?;
        let total_row_count: u64 = count_resp
            .data
            .first()
            .and_then(|r| r.first())
            .and_then(|v| match v {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(0);

        // 2. Sample rows.
        let sample_sql = format!("SELECT * FROM ({sql}) LIMIT {sample_limit}");
        let sample_resp = self.http_query(&sample_sql).await?;

        let column_names: Vec<String> = sample_resp.meta.iter().map(|m| m.name.clone()).collect();
        let column_types: Vec<Option<String>> =
            sample_resp.meta.iter().map(|m| m.r#type.clone()).collect();
        let col_count = column_names.len();

        let sample_rows: Vec<QueryRow> = sample_resp
            .data
            .iter()
            .map(|row| {
                let cells = (0..col_count)
                    .map(|i| row.get(i).map(json_to_cell).unwrap_or(CellValue::Null))
                    .collect();
                QueryRow(cells)
            })
            .collect();

        // 3. Per-column stats.
        let mut col_stats: Vec<ColumnStats> = Vec::with_capacity(col_count);
        for (idx, col) in column_names.iter().enumerate() {
            let quoted = format!("\"{}\"", col.replace('"', "\\\""));
            let col_type = column_types
                .get(idx)
                .and_then(|t| t.as_deref())
                .unwrap_or("");
            // toFloat64OrNull only accepts String in ClickHouse. Route by type:
            // - Numeric: avg/stddevPop work directly on numeric columns and
            //   skip NULLs natively.
            // - String:  wrap in toFloat64OrNull so non-numeric strings become
            //   NULL; avg/stddevPop then skip them.
            // - Other:   dates, UUIDs, etc. — emit NULL for mean/stddev.
            let (avg_expr, sd_expr) = match clickhouse_type_category(col_type) {
                TypeCategory::Numeric => (format!("avg({quoted})"), format!("stddevPop({quoted})")),
                TypeCategory::String => (
                    format!("avg(toFloat64OrNull({quoted}))"),
                    format!("stddevPop(toFloat64OrNull({quoted}))"),
                ),
                TypeCategory::Other => (
                    "CAST(NULL AS Nullable(Float64))".to_string(),
                    "CAST(NULL AS Nullable(Float64))".to_string(),
                ),
            };
            let stat_sql = format!(
                "SELECT \
                    countIf(isNull({quoted})) AS nc, \
                    uniqExact({quoted}) AS dc, \
                    toString(min({quoted})) AS mn, \
                    toString(max({quoted})) AS mx, \
                    {avg_expr} AS avg_v, \
                    {sd_expr} AS sd_v \
                 FROM ({sql})"
            );

            let stat_resp = self.http_query(&stat_sql).await?;
            let stat_row = stat_resp.data.first();

            let null_count: u64 = stat_row
                .and_then(|r| r.first())
                .and_then(|v| match v {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => s.parse().ok(),
                    _ => None,
                })
                .unwrap_or(0);
            let distinct_count: u64 = stat_row
                .and_then(|r| r.get(1))
                .and_then(|v| match v {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => s.parse().ok(),
                    _ => None,
                })
                .unwrap_or(0);
            let min_v = stat_row
                .and_then(|r| r.get(2))
                .map(json_to_cell)
                .unwrap_or(CellValue::Null);
            let max_v = stat_row
                .and_then(|r| r.get(3))
                .map(json_to_cell)
                .unwrap_or(CellValue::Null);
            let mean = stat_row.and_then(|r| r.get(4)).and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            });
            let std_dev = stat_row.and_then(|r| r.get(5)).and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            });

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
            otel.name = %crate::telemetry::span_name(sql, "clickhouse"),
            otel.kind = "client",
            db.system.name = "clickhouse",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let sql = normalize_sql(sql);
        // One request: `SELECT * FROM (user_sql) FORMAT JSONCompact`.
        // The response carries per-column `meta.type` strings (Nullable,
        // LowCardinality, composites all included) and a row-major `data`
        // array of JSON values, which `parse_ch_cell` decodes typed.
        let full_sql = format!("SELECT * FROM ({sql})");
        let resp = self.http_query(&full_sql).await?;

        let columns: Vec<ColumnSpec> = resp
            .meta
            .iter()
            .map(|m| ColumnSpec {
                name: m.name.clone(),
                data_type: ch_type_to_typed(m.r#type.as_deref().unwrap_or("")),
            })
            .collect();
        let col_count = columns.len();

        let typed_rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = resp
            .data
            .iter()
            .map(|row| {
                let mut cells = Vec::with_capacity(col_count);
                for (idx, col) in columns.iter().enumerate() {
                    let v = row.get(idx).unwrap_or(&Value::Null);
                    cells.push(parse_ch_cell(v, col)?);
                }
                Ok(cells)
            })
            .collect();

        Ok(TypedRowStream::from_rows(columns, typed_rows))
    }

    /// Fetch and cache the schema. Idempotent: a second call is a no-op once
    /// the first has populated the cache.
    async fn prepare_schema(&self) -> Result<(), ConnectorError> {
        if self
            .cached_schema
            .read()
            .map(|s| s.is_some())
            .unwrap_or(false)
        {
            return Ok(());
        }
        // Two concurrent callers can both pass the check above and both scan.
        // Harmless: the scan is an idempotent read and the last write wins.
        match fetch_schema(
            &self.client,
            &self.url,
            &self.user,
            &self.password,
            &self.database,
        )
        .await
        {
            Ok(info) => {
                *self
                    .cached_schema
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = Some(info);
                *self.schema_error.write().unwrap_or_else(|e| e.into_inner()) = None;
                Ok(())
            }
            Err(e) => {
                *self.schema_error.write().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// A cached schema wins over a recorded error, deliberately.
    ///
    /// `schema_error` is a *fallback explanation* for an absent cache, not a
    /// veto over a present one. Checking it first would let a stale failure
    /// outlive the success that replaced it: `prepare_schema` early-returns
    /// once the cache is `Some`, so nothing clears an error written after a
    /// successful prepare — interleave a failing preparer with a succeeding
    /// one and the connector would report failure for the rest of its life
    /// while holding a perfectly good schema.
    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        if let Some(info) = self
            .cached_schema
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(info);
        }
        if let Ok(err_guard) = self.schema_error.read()
            && let Some(ref err) = *err_guard
        {
            return Err(ConnectorError::ConnectionError(format!(
                "schema introspection failed: {err}"
            )));
        }
        Err(ConnectorError::ConnectionError(
            "schema not prepared: call prepare_schema() before introspect_schema()".to_string(),
        ))
    }
}

// ── Schema pre-fetch ──────────────────────────────────────────────────────────

/// Query `system.columns` and build a [`SchemaInfo`].
async fn fetch_schema(
    client: &reqwest::Client,
    url: &str,
    user: &str,
    password: &str,
    database: &str,
) -> Result<SchemaInfo, ConnectorError> {
    // Escape single quotes in the database name.
    let db_escaped = database.replace('\'', "\\'");
    let schema_sql = format!(
        "SELECT table, name, type \
         FROM system.columns \
         WHERE database = '{db_escaped}' \
         ORDER BY table, position"
    );

    // Schema introspection is small; the default ceiling is plenty.
    let resp = http_query(
        client,
        url,
        user,
        password,
        database,
        MAX_RESULT_BYTES,
        &schema_sql,
    )
    .await?;

    let mut map: HashMap<String, Vec<SchemaColumnInfo>> = HashMap::new();
    for row in &resp.data {
        let table = match row.first() {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let column = match row.get(1) {
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        let data_type = match row.get(2) {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        map.entry(table).or_default().push(SchemaColumnInfo {
            name: column,
            data_type,
            min: None,
            max: None,
            sample_values: vec![],
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag travels beside the statement and the statement goes as written,
    /// checked at the wire so it needs no server. `clickhouse_tagged_tests`
    /// proves a real ClickHouse lands the rows and logs the tag; this keeps
    /// the contract in the unit run, where a relapse to decorating the SQL
    /// fails in seconds.
    #[tokio::test]
    async fn a_tagged_statement_sends_its_tag_as_log_comment_and_its_sql_untouched() {
        use wiremock::matchers::{body_string, method, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sql = r#"INSERT INTO "receipts" ("a", "b") VALUES (1, 'x'), (2, 'y')"#;
        let tag = "oxy.app='receiving',oxy.fn='submit',oxy.invocation='0af7651916cd'";
        Mock::given(method("POST"))
            .and(query_param("log_comment", tag))
            .and(body_string(sql))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let conn = ClickHouseConnector::new(
            server.uri(),
            "u".to_string(),
            "p".to_string(),
            "db".to_string(),
        )
        .await
        .expect("new does no I/O and cannot fail");
        conn.execute_statement_tagged(sql, tag)
            .await
            .expect("the server answers only a tag beside untouched SQL");
    }

    /// `new` does no I/O, so an un-prepared connector must say so rather than
    /// answer "no tables" — the difference between a loud misconfiguration and
    /// a silently empty warehouse.
    #[tokio::test]
    async fn introspect_before_prepare_is_an_error_not_an_empty_schema() {
        let conn = ClickHouseConnector::new(
            "http://127.0.0.1:1".to_string(),
            "u".to_string(),
            "p".to_string(),
            "db".to_string(),
        )
        .await
        .expect("new does no I/O and cannot fail");

        let err = DatabaseConnector::introspect_schema(&conn)
            .expect_err("un-prepared introspection must error");
        assert!(err.to_string().contains("schema not prepared"), "got {err}");
    }

    /// A cached schema must win over a recorded error: `prepare_schema` never
    /// clears `schema_error` once the cache is populated, so checking the error
    /// first would let a stale failure outlive the success that replaced it.
    #[tokio::test]
    async fn a_cached_schema_wins_over_a_recorded_error() {
        let conn = ClickHouseConnector::new(
            "http://127.0.0.1:1".to_string(),
            "u".to_string(),
            "p".to_string(),
            "db".to_string(),
        )
        .await
        .unwrap();

        *conn.schema_error.write().unwrap() = Some("transient failure".to_string());
        *conn.cached_schema.write().unwrap() = Some(SchemaInfo::default());

        let info = DatabaseConnector::introspect_schema(&conn)
            .expect("a populated cache answers even with an error recorded");
        assert!(info.tables.is_empty());
    }

    #[test]
    fn result_guard_caps_bytes_and_breaks() {
        let params = result_guard_params(MAX_RESULT_BYTES);
        // The byte cap must be present and non-zero (the pod OOM backstop).
        assert_eq!(params[0].0, "max_result_bytes");
        assert_eq!(params[0].1, MAX_RESULT_BYTES.to_string());
        assert!(MAX_RESULT_BYTES > 0);
        // Must be `break` (partial result), NOT `throw` — flipping this turns
        // graceful degradation into hard query failures for large scans.
        assert_eq!(params[1].0, "result_overflow_mode");
        assert_eq!(params[1].1, "break");
    }

    fn is_numeric(t: &str) -> bool {
        matches!(clickhouse_type_category(t), TypeCategory::Numeric)
    }
    fn is_string(t: &str) -> bool {
        matches!(clickhouse_type_category(t), TypeCategory::String)
    }
    fn is_other(t: &str) -> bool {
        matches!(clickhouse_type_category(t), TypeCategory::Other)
    }

    #[test]
    fn type_category_numerics() {
        assert!(is_numeric("Float64"));
        assert!(is_numeric("Float32"));
        assert!(is_numeric("Int32"));
        assert!(is_numeric("Int64"));
        assert!(is_numeric("Int128"));
        assert!(is_numeric("UInt64"));
        assert!(is_numeric("UInt32"));
        assert!(is_numeric("Decimal(18, 4)"));
        assert!(is_numeric("Decimal128(38, 10)"));
    }

    #[test]
    fn type_category_nullable_numerics() {
        assert!(is_numeric("Nullable(Float64)"));
        assert!(is_numeric("Nullable(Int32)"));
        assert!(is_numeric("Nullable(UInt64)"));
        assert!(is_numeric("Nullable(Decimal(10, 2))"));
    }

    #[test]
    fn type_category_strings() {
        assert!(is_string("String"));
        assert!(is_string("FixedString"));
        // Realistic forms returned by ClickHouse: FixedString always carries a
        // length, and both kinds are commonly Nullable.
        assert!(is_string("FixedString(36)"));
        assert!(is_string("Nullable(FixedString(16))"));
        assert!(is_string("Nullable(String)"));
    }

    #[test]
    fn type_category_low_cardinality() {
        // LowCardinality is a dictionary-encoding wrapper; the inner type
        // determines the category. Common in production for low-cardinality
        // string columns (status, country, etc.) and occasionally numerics.
        assert!(is_string("LowCardinality(String)"));
        assert!(is_string("LowCardinality(FixedString(16))"));
        assert!(is_numeric("LowCardinality(Float64)"));
        assert!(is_numeric("LowCardinality(Int32)"));
        // Either nesting order is legal in ClickHouse.
        assert!(is_string("LowCardinality(Nullable(String))"));
        assert!(is_string("Nullable(LowCardinality(String))"));
        assert!(is_numeric("Nullable(LowCardinality(Float64))"));
    }

    #[test]
    fn type_category_other() {
        assert!(is_other("Date"));
        assert!(is_other("Date32"));
        assert!(is_other("DateTime"));
        assert!(is_other("DateTime64(3)"));
        assert!(is_other("UUID"));
        assert!(is_other("Bool"));
        assert!(is_other(""));
        assert!(is_other("Array(String)"));
    }
}
