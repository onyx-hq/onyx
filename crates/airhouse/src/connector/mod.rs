//! Airhouse connector implementation.
//!
//! Airhouse speaks the PostgreSQL wire protocol but executes SQL in the DuckDB
//! dialect under the hood. This connector connects via `tokio_postgres` but:
//!
//! 1. Reports `SqlDialect::DuckDb` so solvers generate DuckDB-flavoured SQL.
//! 2. Uses the **simple query protocol** (`simple_query`) for every server call
//!    instead of the extended/prepared-statement protocol that `PostgresConnector`
//!    uses. Airhouse's extended-protocol column metadata is not fully compatible
//!    with `tokio_postgres` (row-index access fails with "invalid column `0`"),
//!    so we stick to `simple_query` which returns every value as a text string
//!    keyed by column name — a much smaller protocol surface.
//! 3. Uses DuckDB-native introspection (`information_schema.columns` with
//!    DuckDB semantics, `::VARCHAR` casts, `DOUBLE` type, `STDDEV_POP`).
//!
//! # WARNING: No TLS
//!
//! This connector uses [`tokio_postgres::NoTls`] — all traffic (credentials
//! **and** query data) is sent in plaintext. This is intentional for the
//! current deployment model where Airhouse runs on a private network segment
//! that is unreachable from the public internet. **Do not expose this connector
//! over an untrusted network without adding TLS.** A `tls: bool` field in the
//! config is the tracked follow-up for operators who need transport encryption.

mod typed;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

use agentic_connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, QueryFailedDetails,
    ResultSummary, SchemaColumnInfo, SchemaInfo, SchemaTableInfo, SqlDialect, normalize_sql,
};
use agentic_core::result::{
    BoxedRowStream, CellValue, ColumnSpec, QueryResult, QueryRow, TypedRowError, TypedRowStream,
    TypedValue,
};

use self::typed::{describe_type_to_typed, parse_cell};

// ── Error helpers ─────────────────────────────────────────────────────────────

/// Build a structured [`QueryFailedDetails`] from a `tokio_postgres` error.
///
/// Airhouse speaks pgwire, so `as_db_error()` carries DuckDB's error message
/// alongside the SQLSTATE-equivalent code (and, when available, `DETAIL` /
/// `HINT` / `POSITION`). Surfacing those as separate fields lets the IDE
/// render the error as a structured block. Mirrors the postgres connector's
/// `pg_query_failed`; intentionally duplicated to avoid making the helper
/// public on `agentic-connector` (which would force its `postgres` feature
/// onto every consumer).
fn airhouse_query_failed(sql: &str, e: &tokio_postgres::Error) -> QueryFailedDetails {
    let sql = sql.to_string();
    if let Some(db) = e.as_db_error() {
        QueryFailedDetails {
            sql,
            message: db.message().to_string(),
            code: Some(db.code().code().to_string()),
            detail: db.detail().map(|s| s.to_string()),
            hint: db.hint().map(|s| s.to_string()),
            position: db.position().and_then(|p| match p {
                tokio_postgres::error::ErrorPosition::Original(n) => Some(*n),
                tokio_postgres::error::ErrorPosition::Internal { .. } => None,
            }),
        }
    } else {
        QueryFailedDetails {
            sql,
            message: e.to_string(),
            ..Default::default()
        }
    }
}

// ── Value helpers ─────────────────────────────────────────────────────────────

/// Parse a text cell from Airhouse (all simple-query values come back as text)
/// into a [`CellValue`].
fn text_to_cell(opt: Option<&str>) -> CellValue {
    match opt {
        None => CellValue::Null,
        Some(s) => {
            if let Ok(n) = s.parse::<i64>() {
                CellValue::Number(n as f64)
            } else if let Ok(n) = s.parse::<f64>() {
                CellValue::Number(n)
            } else {
                CellValue::Text(s.to_string())
            }
        }
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Format a `tokio_postgres::Error` so the SQLSTATE + server message
/// (when present) survive the trip into `ConnectorError::ConnectionError`.
///
/// `Display` on `tokio_postgres::Error` collapses to terse strings like
/// `"connection error"` / `"db error"` which throws away the server's
/// `ErrorResponse` payload — exactly the bit operators need to debug
/// SCRAM mismatches, missing tenants, or transport failures. This walks
/// the source chain instead.
fn format_pg_error(e: &tokio_postgres::Error) -> String {
    use std::error::Error;

    if let Some(db_err) = e.as_db_error() {
        // Server-sent ErrorResponse: SQLSTATE + message is the actionable
        // bit. Include the high-level kind so operators can tell whether
        // the error was raised during connect, a query, etc.
        return format!(
            "{} [SQLSTATE {}] {}",
            e,
            db_err.code().code(),
            db_err.message()
        );
    }
    // No DbError → typically a transport / IO failure. Concatenate the
    // source chain so the underlying io::Error message shows up.
    let mut out = e.to_string();
    let mut src: Option<&dyn Error> = e.source();
    while let Some(next) = src {
        out.push_str(": ");
        out.push_str(&next.to_string());
        src = next.source();
    }
    out
}

// ── Connector ─────────────────────────────────────────────────────────────────

/// Airhouse-backed connector: pgwire transport, DuckDB dialect.
pub struct AirhouseConnector {
    client: Arc<tokio::sync::Mutex<Client>>,
    cached_schema: SchemaInfo,
    /// `false` once the pgwire connection driver task has ended (the
    /// connection was closed or errored — e.g. the Airhouse DP restarted).
    /// Lets a connector pool detect a dead connection on checkout without a
    /// round-trip or contending on the query mutex.
    alive: Arc<AtomicBool>,
}

impl AirhouseConnector {
    /// Connect to an Airhouse instance and pre-fetch the database schema.
    pub async fn new(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<Self, ConnectorError> {
        let mut config = tokio_postgres::Config::new();
        config.host(host);
        config.port(port);
        config.user(user);
        config.password(password);
        config.dbname(database);

        let (client, connection) = config
            .connect(NoTls)
            .await
            .map_err(|e| ConnectorError::ConnectionError(format_pg_error(&e)))?;

        let alive = Arc::new(AtomicBool::new(true));
        let alive_for_driver = Arc::clone(&alive);
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!("airhouse connection driver error: {e}");
            }
            // The driver future resolves only when the connection is gone
            // (clean close or error). Mark the connector dead so the pool
            // rebuilds instead of handing out a defunct connection.
            alive_for_driver.store(false, Ordering::Release);
        });

        let cached_schema = fetch_schema(&client).await?;

        Ok(Self {
            client: Arc::new(tokio::sync::Mutex::new(client)),
            cached_schema,
            alive,
        })
    }

    /// True while the underlying pgwire connection is still open. Cheap,
    /// non-blocking, no round-trip — safe to call on every pool checkout.
    pub fn is_live(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

// ── DatabaseConnector impl ────────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for AirhouseConnector {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::DuckDb
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %agentic_connector::telemetry::span_name(sql, "airhouse"),
            otel.kind = "client",
            db.system.name = "airhouse",
            db.operation.name = %agentic_connector::telemetry::operation_name(sql),
            db.query.text = %agentic_connector::telemetry::query_text(sql),
            oxy.db.method = "execute_query",
            oxy.db.sample_limit = sample_limit,
        )
    )]
    async fn execute_query(
        &self,
        sql: &str,
        sample_limit: u64,
    ) -> Result<ExecutionResult, ConnectorError> {
        let sql = normalize_sql(sql);

        match classify(sql) {
            // DDL/DML: execute directly, return empty results.
            StatementKind::DdlDml => {
                return self
                    .client
                    .lock()
                    .await
                    .simple_query(sql)
                    .await
                    .map(|_| ExecutionResult {
                        result: QueryResult {
                            columns: vec![],
                            rows: vec![],
                            total_row_count: 0,
                            truncated: false,
                        },
                        summary: ResultSummary {
                            row_count: 0,
                            columns: vec![],
                        },
                    })
                    .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)));
            }
            // Statement-form (SHOW, EXPLAIN, …): execute directly, capture
            // all rows verbatim with text columns. Cannot be wrapped in
            // `CREATE TEMP TABLE … AS (…)` or `SELECT … FROM (…)`.
            StatementKind::Statement => {
                let messages = self
                    .client
                    .lock()
                    .await
                    .simple_query(sql)
                    .await
                    .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)))?;
                return Ok(execution_result_from_messages(&messages, sample_limit));
            }
            StatementKind::Subquery => {}
        }

        // Subquery DQL: introspect + count + sample + profile WITHOUT
        // materialising the user's result into a `CREATE TEMP TABLE`.
        //
        // Airhouse backs every connection with a server-side DuckDB session,
        // and `CREATE TEMP TABLE _agentic_tmp AS (...)` materialised the FULL
        // result set into that session's memory for the duration of the call —
        // a large result could spike (and, until the DROP, pin) the tenant's
        // DuckDB session memory. Instead we wrap the query as a subquery
        // `(sql) AS _q` (the same shape `execute_query_full` already uses) so
        // nothing is held server-side between statements.
        //
        // Cost: the user query is planned once (DESCRIBE) and executed twice
        // (count+stats in one pass, sample in another) instead of materialised
        // once and re-scanned. For a non-deterministic query the sample may not
        // perfectly match the stats — acceptable, and already the behaviour of
        // the paged `execute_query_full` path.
        let client = self.client.lock().await;

        // 1. Column names + types via DuckDB's DESCRIBE of the raw query.
        //    A failed DESCRIBE here is the user's query failing — surface their
        //    SQL (not the DESCRIBE wrapper) in the structured error.
        let describe_sql = format!("DESCRIBE {sql}");
        let describe_messages = client
            .simple_query(&describe_sql)
            .await
            .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)))?;

        let mut column_names: Vec<String> = Vec::new();
        let mut column_types: Vec<String> = Vec::new();
        for msg in &describe_messages {
            if let SimpleQueryMessage::Row(row) = msg {
                let name = row
                    .get("column_name")
                    .ok_or_else(|| {
                        ConnectorError::query_failed(
                            describe_sql.clone(),
                            "DESCRIBE row missing column_name".to_string(),
                        )
                    })?
                    .to_string();
                let ty = row.get("column_type").unwrap_or_default().to_string();
                column_names.push(name);
                column_types.push(ty);
            }
        }
        let col_count = column_names.len();

        // 2. Total row count + per-column stats in a SINGLE pass over the
        //    subquery. Each column contributes 6 aliased expressions; TRY_CAST
        //    to DOUBLE returns NULL for non-numeric columns so mean/std_dev come
        //    back NULL naturally. The dedicated `_agentic_n` alias holds the row
        //    count so it can't collide with a user column named `n`.
        const COUNT_ALIAS: &str = "_agentic_n";
        let stats_select = if col_count == 0 {
            format!("SELECT COUNT(*) AS {COUNT_ALIAS}")
        } else {
            let exprs: String = column_names
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let q = quote_ident(col);
                    format!(
                        "(COUNT(*) - COUNT({q}))::VARCHAR AS c{i}_nc, \
                         COUNT(DISTINCT {q})::VARCHAR AS c{i}_dc, \
                         MIN({q})::VARCHAR AS c{i}_mn, \
                         MAX({q})::VARCHAR AS c{i}_mx, \
                         AVG(TRY_CAST({q} AS DOUBLE))::VARCHAR AS c{i}_avg, \
                         STDDEV_POP(TRY_CAST({q} AS DOUBLE))::VARCHAR AS c{i}_sd"
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("SELECT COUNT(*) AS {COUNT_ALIAS}, {exprs}")
        };
        let stats_sql = format!("{stats_select} FROM ({sql}) AS _q");
        let stats_messages = client
            .simple_query(&stats_sql)
            .await
            .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)))?;
        let stats_row = stats_messages.iter().find_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        });
        let total_row_count = stats_row
            .and_then(|r| r.get(COUNT_ALIAS).and_then(|s| s.parse::<u64>().ok()))
            .unwrap_or(0);

        let col_stats: Vec<ColumnStats> = column_names
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let r = stats_row.as_ref();
                // SimpleQueryRow::get requires &str, not &String.
                let k_nc = format!("c{i}_nc");
                let k_dc = format!("c{i}_dc");
                let k_mn = format!("c{i}_mn");
                let k_mx = format!("c{i}_mx");
                let k_avg = format!("c{i}_avg");
                let k_sd = format!("c{i}_sd");
                let null_count = r
                    .and_then(|r| r.get(k_nc.as_str()).and_then(|s| s.parse().ok()))
                    .unwrap_or(0u64);
                let distinct_count = r
                    .and_then(|r| r.get(k_dc.as_str()).and_then(|s| s.parse().ok()))
                    .unwrap_or(0u64);
                let min_v = text_to_cell(r.and_then(|r| r.get(k_mn.as_str())));
                let max_v = text_to_cell(r.and_then(|r| r.get(k_mx.as_str())));
                let mean = r.and_then(|r| r.get(k_avg.as_str()).and_then(|s| s.parse().ok()));
                let std_dev = r.and_then(|r| r.get(k_sd.as_str()).and_then(|s| s.parse().ok()));
                ColumnStats {
                    name: col.clone(),
                    data_type: column_types.get(i).cloned(),
                    null_count,
                    distinct_count: Some(distinct_count),
                    min: Some(min_v),
                    max: Some(max_v),
                    mean,
                    std_dev,
                }
            })
            .collect();

        // 3. Sample rows — cast every column to VARCHAR for uniform decoding.
        let sample_rows: Vec<QueryRow> = if col_count == 0 {
            Vec::new()
        } else {
            let cast_cols: String = column_names
                .iter()
                .map(|c| {
                    let q = quote_ident(c);
                    format!("{q}::VARCHAR AS {q}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            let sample_sql = format!("SELECT {cast_cols} FROM ({sql}) AS _q LIMIT {sample_limit}");

            let messages = client
                .simple_query(&sample_sql)
                .await
                .map_err(|e| ConnectorError::query_failed(sample_sql.clone(), e.to_string()))?;

            let mut rows = Vec::new();
            for msg in &messages {
                if let SimpleQueryMessage::Row(r) = msg {
                    let cells = column_names
                        .iter()
                        .map(|c| text_to_cell(r.get(c.as_str())))
                        .collect();
                    rows.push(QueryRow(cells));
                }
            }
            rows
        };

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
            otel.name = %agentic_connector::telemetry::span_name(sql, "airhouse"),
            otel.kind = "client",
            db.system.name = "airhouse",
            db.operation.name = %agentic_connector::telemetry::operation_name(sql),
            db.query.text = %agentic_connector::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let sql = normalize_sql(sql);

        match classify(sql) {
            // DDL/DML: execute directly, no row stream.
            StatementKind::DdlDml => {
                return self
                    .client
                    .lock()
                    .await
                    .simple_query(sql)
                    .await
                    .map(|_| TypedRowStream::from_rows(vec![], vec![]))
                    .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)));
            }
            // Statement-form (SHOW, EXPLAIN, …): execute directly, capture
            // all rows in one shot with text columns. Cannot be wrapped in
            // `DESCRIBE (…)` or `SELECT … FROM (…)` for introspection.
            StatementKind::Statement => {
                let messages = self
                    .client
                    .lock()
                    .await
                    .simple_query(sql)
                    .await
                    .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)))?;
                return Ok(typed_row_stream_from_messages(&messages));
            }
            StatementKind::Subquery => {}
        }

        // Subquery DQL: introspect column types via DESCRIBE (no temp table),
        // then page through rows using a subquery wrapper. A failed DESCRIBE
        // here is the user's query failing — surface their SQL (not the
        // `DESCRIBE` wrapper) so the structured error block points at the
        // statement they actually typed.
        let describe_sql = format!("DESCRIBE {sql}");
        let columns = {
            let client = self.client.lock().await;
            let messages = client
                .simple_query(&describe_sql)
                .await
                .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)))?;
            let mut cols: Vec<ColumnSpec> = Vec::new();
            for msg in &messages {
                if let SimpleQueryMessage::Row(row) = msg {
                    let name = row
                        .get("column_name")
                        .ok_or_else(|| {
                            ConnectorError::query_failed(
                                describe_sql.clone(),
                                "DESCRIBE row missing column_name".to_string(),
                            )
                        })?
                        .to_string();
                    let ty_str = row.get("column_type").unwrap_or_default();
                    cols.push(ColumnSpec {
                        name,
                        data_type: describe_type_to_typed(ty_str),
                    });
                }
            }
            cols
        };

        if columns.is_empty() {
            return Ok(TypedRowStream::from_rows(vec![], vec![]));
        }

        let cast_cols: String = columns
            .iter()
            .map(|c| {
                let q = quote_ident(&c.name);
                format!("{q}::VARCHAR AS {q}")
            })
            .collect::<Vec<_>>()
            .join(", ");

        const PAGE_SIZE: usize = 1_000;
        let client_arc = Arc::clone(&self.client);
        let columns_for_stream = columns.clone();
        let owned_sql = sql.to_string();
        let stream: BoxedRowStream = Box::pin(async_stream::stream! {
            let columns = columns_for_stream;
            let mut offset = 0usize;
            loop {
                let client = client_arc.lock().await;
                let page_sql = format!(
                    "SELECT {cast_cols} FROM ({owned_sql}) AS _q LIMIT {PAGE_SIZE} OFFSET {offset}"
                );
                match client.simple_query(&page_sql).await {
                    Err(e) => {
                        yield Err(TypedRowError::DriverError(e.to_string()));
                        break;
                    }
                    Ok(messages) => {
                        let batch: Vec<_> = messages
                            .into_iter()
                            .filter_map(|msg| match msg {
                                SimpleQueryMessage::Row(row) => {
                                    Some(parse_airhouse_row(&row, &columns))
                                }
                                _ => None,
                            })
                            .collect();
                        let fetched = batch.len();
                        for row in batch {
                            yield row;
                        }
                        if fetched < PAGE_SIZE {
                            break;
                        }
                        offset += fetched;
                    }
                }
            }
        });

        Ok(TypedRowStream {
            columns,
            rows: stream,
            // TODO(large-query-guards): wire the agentic-connector `ResultCap`
            // backstop here too (airhouse is pgwire/DuckDB-dialect). Tracked as a
            // follow-up alongside Domo. None = does not signal truncation.
            truncated: None,
        })
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %agentic_connector::telemetry::span_name(sql, "airhouse"),
            otel.kind = "client",
            db.system.name = "airhouse",
            db.operation.name = %agentic_connector::telemetry::operation_name(sql),
            db.query.text = %agentic_connector::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full_untyped",
        )
    )]
    async fn execute_query_full_untyped(
        &self,
        sql: &str,
    ) -> Result<TypedRowStream, ConnectorError> {
        // World-model dashboard fast path. Its tiles already `::VARCHAR`-cast every
        // column and render text, so the typed `execute_query_full` path's
        // per-query `DESCRIBE` round-trip (column types, used only to re-cast to
        // VARCHAR and parse back) is pure overhead here — and it DOUBLES the
        // statements serialized over this connector's single client mutex, which
        // is the dashboard's bottleneck. Skip it: run the subquery ONCE and
        // surface all columns as `Text`, exactly like the `Statement` kind does.
        // Only relational subqueries short-circuit; DDL / statement-form have no
        // `DESCRIBE` to save, so delegate — and every non-opted-in caller keeps
        // the typed path via the trait default.
        let sql = normalize_sql(sql);
        if classify(sql) != StatementKind::Subquery {
            return self.execute_query_full(sql).await;
        }
        let wrapped = format!("SELECT * FROM ({sql}) AS _q");
        let messages = self
            .client
            .lock()
            .await
            .simple_query(&wrapped)
            .await
            .map_err(|e| ConnectorError::QueryFailed(airhouse_query_failed(sql, &e)))?;
        Ok(typed_row_stream_from_messages(&messages))
    }

    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        Ok(self.cached_schema.clone())
    }
}

/// What shape of statement is this — for picking the right execution path?
///
/// Three categories matter to the airhouse connector:
///
/// - `Subquery`: `SELECT` / `WITH` / `FROM` / `TABLE` / `VALUES`. Pure
///   relational expressions that can be wrapped in `CREATE TEMP TABLE AS
///   (…)`, `DESCRIBE (…)`, and `SELECT … FROM (…)`. Goes through the
///   typed/paged path.
/// - `Statement`: `SHOW` / `SUMMARIZE` / `DESCRIBE` / `EXPLAIN` / `PIVOT` /
///   `UNPIVOT`. Top-level DuckDB statements that **return rows** but are
///   **not** subquery-able — `DESCRIBE SHOW` and `SELECT * FROM (SHOW)` both
///   error in DuckDB. Executed directly, all columns surfaced as `Text`.
/// - `DdlDml`: `CREATE` / `INSERT` / `UPDATE` / `DELETE` / `ALTER` / `DROP` /
///   anything else. Executed directly, returns no rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatementKind {
    Subquery,
    Statement,
    DdlDml,
}

fn classify(sql: &str) -> StatementKind {
    let first = sql
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    match first.as_str() {
        "SELECT" | "WITH" | "FROM" | "TABLE" | "VALUES" => StatementKind::Subquery,
        "SHOW" | "SUMMARIZE" | "DESCRIBE" | "EXPLAIN" | "PIVOT" | "UNPIVOT" => {
            StatementKind::Statement
        }
        _ => StatementKind::DdlDml,
    }
}

/// Decode one row of the Airhouse simple-query result into [`TypedValue`]s.
///
/// Each column's string is parsed according to its pre-computed
/// [`ColumnSpec::data_type`]; NULL arrives as `None` and maps to
/// [`TypedValue::Null`].
fn parse_airhouse_row(
    row: &tokio_postgres::SimpleQueryRow,
    columns: &[ColumnSpec],
) -> Result<Vec<TypedValue>, TypedRowError> {
    let mut cells = Vec::with_capacity(columns.len());
    for col in columns {
        let cell = match row.get(col.name.as_str()) {
            None => TypedValue::Null,
            Some(text) => parse_cell(text, col)?,
        };
        cells.push(cell);
    }
    Ok(cells)
}

// ── Statement-form helpers ──────────────────────────────────────────────────
//
// `SHOW`, `EXPLAIN`, etc. return rows but cannot be wrapped in DESCRIBE or a
// subquery. The connector executes them directly via `simple_query` and
// these helpers shape the result. Column names come from the wire response;
// types are unknown so every cell is surfaced as `Text`.

fn collect_column_names(messages: &[SimpleQueryMessage]) -> Vec<String> {
    messages
        .iter()
        .find_map(|m| {
            if let SimpleQueryMessage::Row(r) = m {
                Some(
                    r.columns()
                        .iter()
                        .map(|c| c.name().to_string())
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn execution_result_from_messages(
    messages: &[SimpleQueryMessage],
    sample_limit: u64,
) -> ExecutionResult {
    let columns = collect_column_names(messages);

    let all_rows: Vec<QueryRow> = messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => {
                let cells = columns
                    .iter()
                    .map(|c| text_to_cell(r.get(c.as_str())))
                    .collect();
                Some(QueryRow(cells))
            }
            _ => None,
        })
        .collect();

    let total_row_count = all_rows.len() as u64;
    let truncated = total_row_count > sample_limit;
    let sample_rows = if truncated {
        all_rows.into_iter().take(sample_limit as usize).collect()
    } else {
        all_rows
    };

    // Statement-form results don't have a meaningful per-column statistics
    // story (no types, no aggregation semantics). Emit empty stats; the
    // sample rows + column names are the useful payload.
    let col_stats = columns
        .iter()
        .map(|name| ColumnStats {
            name: name.clone(),
            data_type: None,
            null_count: 0,
            distinct_count: None,
            min: None,
            max: None,
            mean: None,
            std_dev: None,
        })
        .collect();

    ExecutionResult {
        result: QueryResult {
            columns,
            rows: sample_rows,
            total_row_count,
            truncated,
        },
        summary: ResultSummary {
            row_count: total_row_count,
            columns: col_stats,
        },
    }
}

fn typed_row_stream_from_messages(messages: &[SimpleQueryMessage]) -> TypedRowStream {
    let column_names = collect_column_names(messages);
    let columns: Vec<ColumnSpec> = column_names
        .iter()
        .map(|name| ColumnSpec {
            name: name.clone(),
            data_type: agentic_core::result::TypedDataType::Text,
        })
        .collect();

    let rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(r) => Some(Ok(column_names
                .iter()
                .map(|c| match r.get(c.as_str()) {
                    None => TypedValue::Null,
                    Some(text) => TypedValue::Text(text.to_string()),
                })
                .collect())),
            _ => None,
        })
        .collect();

    TypedRowStream::from_rows(columns, rows)
}

// ── Schema pre-fetch ──────────────────────────────────────────────────────────

/// Query `information_schema.columns` via the simple query protocol and build a
/// [`SchemaInfo`]. Filters out internal DuckLake / pg_catalog tables and our
/// own `_agentic_%` temp tables.
async fn fetch_schema(client: &Client) -> Result<SchemaInfo, ConnectorError> {
    let schema_sql = "\
        SELECT table_schema, table_name, column_name, data_type \
        FROM information_schema.columns \
        WHERE table_schema NOT IN ('information_schema', 'pg_catalog', 'pg_toast', 'ducklake') \
          AND table_name NOT LIKE 'ducklake_%' \
          AND table_name NOT LIKE '_agentic_%' \
          AND table_name NOT LIKE 'airhouse_%' \
        ORDER BY table_schema, table_name, ordinal_position";

    let messages = client.simple_query(schema_sql).await.map_err(|e| {
        ConnectorError::ConnectionError(format!(
            "airhouse schema introspection failed: {}",
            format_pg_error(&e)
        ))
    })?;

    let mut map: HashMap<String, Vec<SchemaColumnInfo>> = HashMap::new();
    for msg in &messages {
        let SimpleQueryMessage::Row(row) = msg else {
            continue;
        };
        let table = match row.get("table_name") {
            Some(s) => s.to_string(),
            None => continue,
        };
        // airhouse (DuckLake) has no implicit search_path to the
        // dataset, so an unqualified `SELECT * FROM orders` errors.
        // Qualify every table as `"schema"."table"` here so every
        // consumer (IDE table click, SQL editor, agents) gets a
        // runnable reference. Quoting both parts also disambiguates
        // same-named tables across schemas (previously collided on the
        // bare-name map key).
        let schema = match row.get("table_schema") {
            Some(s) => s.to_string(),
            None => continue,
        };
        let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
        let column = match row.get("column_name") {
            Some(s) => s.to_string(),
            None => continue,
        };
        let data_type = row.get("data_type").unwrap_or_default().to_string();
        map.entry(qualified).or_default().push(SchemaColumnInfo {
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
    use super::{StatementKind, classify};

    #[test]
    fn select_family_is_subquery() {
        for sql in [
            "SELECT 1",
            "  select 1",
            "WITH t AS (SELECT 1) SELECT * FROM t",
            "FROM t SELECT *",
            "TABLE t",
            "VALUES (1, 2)",
        ] {
            assert_eq!(classify(sql), StatementKind::Subquery, "sql: {sql}");
        }
    }

    #[test]
    fn duckdb_result_producing_keywords_are_statement_form() {
        // These return rows but cannot be wrapped in `(…)` — DESCRIBE / temp
        // table / subquery wrappings all reject them. Need to be executed
        // directly via the `Statement` path.
        for sql in [
            "SHOW TABLES",
            "show databases",
            "SHOW SCHEMAS",
            "SUMMARIZE my_table",
            "DESCRIBE my_table",
            "EXPLAIN SELECT 1",
            "PIVOT t ON year USING sum(amount)",
            "UNPIVOT t ON a, b",
        ] {
            assert_eq!(classify(sql), StatementKind::Statement, "sql: {sql}");
        }
    }

    #[test]
    fn ddl_dml_is_ddldml() {
        for sql in [
            "CREATE TABLE t (id INT)",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET id = 2",
            "DELETE FROM t",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN c INT",
            "",
        ] {
            assert_eq!(classify(sql), StatementKind::DdlDml, "sql: {sql}");
        }
    }
}
