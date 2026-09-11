//! MySQL / MariaDB connector implementation via `sqlx-mysql`.
//!
//! Mirrors the Postgres backend's shape:
//!
//! 1. `execute_query`: materialise the user SQL into a temp table, count,
//!    sample with `LIMIT n` cast to `CHAR` for uniform string decoding,
//!    compute per-column stats.
//! 2. `execute_query_full`: same temp-table introspection, but build a
//!    per-column SELECT that leaves natively-decodable types untouched and
//!    casts the rest (`DECIMAL`, `TIME`, `YEAR`, `BIT`, `ENUM`, `SET`,
//!    `GEOMETRY`, …) to `CHAR`. Rows decode through `sqlx::Row::try_get::<T>`
//!    with the right Rust type per column.
//! 3. `introspect_schema`: `information_schema.columns`, cached at
//!    construction time.
//!
//! Every query here goes through sqlx 0.9's `AssertSqlSafe`. That is an
//! assertion, not a check — sqlx validates nothing. It is sound here because
//! running caller-supplied SQL *is* this connector's contract, and the only
//! values interpolated around that SQL are internal constants (`tmp` is the
//! fixed `_agentic_tmp`) or identifiers quoted out of `information_schema`.
//! Do not read a wrapped call as "sqlx checked this".

#![cfg(feature = "mysql")]

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{AssertSqlSafe, Column, Executor, Row, SqlSafeStr, TypeInfo};

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agentic_core::result::{
    BoxedRowStream, CellValue, ColumnSpec, QueryResult, QueryRow, TypedDataType, TypedRowError,
    TypedRowStream, TypedValue,
};

use crate::connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, ResultCap, ResultSummary,
    SchemaColumnInfo, SchemaInfo, SchemaTableInfo, SqlDialect, guard_row_stream,
    is_returning_statement, normalize_sql, plan_sql_script,
};

// ── Value / type helpers ────────────────────────────────────────────────────

/// Parse a `CHAR`-casted MySQL string cell into a [`CellValue`]. Numeric
/// strings become `Number`; everything else stays `Text`.
fn text_to_cell(opt: Option<String>) -> CellValue {
    match opt {
        None => CellValue::Null,
        Some(s) => {
            if let Ok(n) = s.parse::<i64>() {
                CellValue::Number(n as f64)
            } else if let Ok(n) = s.parse::<f64>() {
                CellValue::Number(n)
            } else {
                CellValue::Text(s)
            }
        }
    }
}

/// Map a sqlx-mysql type-info name (e.g. `"INT"`, `"BIGINT UNSIGNED"`,
/// `"VARCHAR"`, `"DATETIME"`) to a [`TypedDataType`].
pub(crate) fn mysql_type_to_typed(name: &str) -> TypedDataType {
    let up = name.to_ascii_uppercase();
    let trimmed = up.trim();

    match trimmed {
        "BOOLEAN" => TypedDataType::Bool,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "TINYINT UNSIGNED"
        | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" => TypedDataType::Int32,
        "BIGINT" | "INT UNSIGNED" => TypedDataType::Int64,
        // BIGINT UNSIGNED can exceed i64 — route through Decimal (string)
        // so callers preserve the full range.
        "BIGINT UNSIGNED" => TypedDataType::Decimal {
            precision: 20,
            scale: 0,
        },
        "FLOAT" | "DOUBLE" | "REAL" => TypedDataType::Float64,
        "DECIMAL" | "NUMERIC" => TypedDataType::Decimal {
            precision: 38,
            scale: 0,
        },
        "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            TypedDataType::Text
        }
        "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BIT" => {
            TypedDataType::Bytes
        }
        "DATE" => TypedDataType::Date,
        "DATETIME" | "TIMESTAMP" => TypedDataType::Timestamp,
        "JSON" => TypedDataType::Json,
        _ => TypedDataType::Unknown,
    }
}

/// Build the `SELECT` fragment for one column. Columns whose native MySQL
/// type sqlx decodes cleanly pass through as bare identifiers; everything
/// else is cast to `CHAR` so the row decoder always sees a string.
///
/// BINARY, VARBINARY, and BLOB types are included in the cast group because
/// MySQL's binary protocol reports `information_schema` text columns (e.g.
/// `table_name`) as BLOB even though they contain UTF-8 text.
fn select_expr_for_mysql_type(quoted_col: &str, typname: &str) -> String {
    let up = typname.to_ascii_uppercase();
    match up.trim() {
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
        | "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "FLOAT" | "DOUBLE" | "REAL" | "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT"
        | "LONGTEXT" | "DATE" | "DATETIME" | "TIMESTAMP" | "JSON" => quoted_col.to_string(),
        // Everything else — DECIMAL, BIGINT UNSIGNED, TIME, YEAR, BIT, ENUM,
        // SET, GEOMETRY, BINARY, VARBINARY, TINYBLOB, BLOB, MEDIUMBLOB,
        // LONGBLOB — is cast to CHAR.
        _ => format!("CAST({quoted_col} AS CHAR)"),
    }
}

/// Decode one row against a pre-computed list of column specs.
fn decode_row(row: &MySqlRow, columns: &[ColumnSpec]) -> Result<Vec<TypedValue>, TypedRowError> {
    let mut out = Vec::with_capacity(columns.len());
    for (idx, col) in columns.iter().enumerate() {
        out.push(decode_cell(row, idx, col)?);
    }
    Ok(out)
}

fn decode_cell(row: &MySqlRow, idx: usize, col: &ColumnSpec) -> Result<TypedValue, TypedRowError> {
    fn mapping_err(col: &ColumnSpec, err: impl std::fmt::Display) -> TypedRowError {
        TypedRowError::TypeMappingError {
            column: col.name.clone(),
            native_type: format!("{:?}", col.data_type),
            message: err.to_string(),
        }
    }

    match &col.data_type {
        TypedDataType::Bool => match row.try_get::<Option<bool>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Bool(v)),
            Ok(None) => Ok(TypedValue::Null),
            // Fallback: TINYINT(1) sometimes decodes as i8.
            Err(_) => match row.try_get::<Option<i8>, _>(idx) {
                Ok(Some(v)) => Ok(TypedValue::Bool(v != 0)),
                Ok(None) => Ok(TypedValue::Null),
                Err(e) => Err(mapping_err(col, e)),
            },
        },
        TypedDataType::Int32 => {
            // sqlx-mysql decodes TINYINT/SMALLINT/MEDIUMINT/INT into their
            // native widths; try i32 first, fall back through smaller widths.
            match row.try_get::<Option<i32>, _>(idx) {
                Ok(Some(v)) => Ok(TypedValue::Int32(v)),
                Ok(None) => Ok(TypedValue::Null),
                Err(_) => match row.try_get::<Option<i16>, _>(idx) {
                    Ok(Some(v)) => Ok(TypedValue::Int32(v as i32)),
                    Ok(None) => Ok(TypedValue::Null),
                    Err(_) => match row.try_get::<Option<i8>, _>(idx) {
                        Ok(Some(v)) => Ok(TypedValue::Int32(v as i32)),
                        Ok(None) => Ok(TypedValue::Null),
                        Err(_) => match row.try_get::<Option<u32>, _>(idx) {
                            // INT UNSIGNED can be up to 2^32-1; widen
                            // defensively.
                            Ok(Some(v)) => i32::try_from(v)
                                .map(TypedValue::Int32)
                                .or(Ok(TypedValue::Int64(v as i64))),
                            Ok(None) => Ok(TypedValue::Null),
                            Err(e) => Err(mapping_err(col, e)),
                        },
                    },
                },
            }
        }
        TypedDataType::Int64 => match row.try_get::<Option<i64>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Int64(v)),
            Ok(None) => Ok(TypedValue::Null),
            Err(_) => match row.try_get::<Option<u32>, _>(idx) {
                Ok(Some(v)) => Ok(TypedValue::Int64(v as i64)),
                Ok(None) => Ok(TypedValue::Null),
                Err(e) => Err(mapping_err(col, e)),
            },
        },
        TypedDataType::Float64 => match row.try_get::<Option<f64>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Float64(v)),
            Ok(None) => Ok(TypedValue::Null),
            Err(_) => match row.try_get::<Option<f32>, _>(idx) {
                Ok(Some(v)) => Ok(TypedValue::Float64(v as f64)),
                Ok(None) => Ok(TypedValue::Null),
                Err(e) => Err(mapping_err(col, e)),
            },
        },
        TypedDataType::Text => match row.try_get::<Option<String>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Text(v)),
            Ok(None) => Ok(TypedValue::Null),
            // MySQL's binary protocol reports some text columns (e.g. SHOW/DESCRIBE
            // results) as VARBINARY. Fall back to bytes and decode as UTF-8.
            Err(_) => match row.try_get::<Option<Vec<u8>>, _>(idx) {
                Ok(Some(v)) => Ok(TypedValue::Text(String::from_utf8_lossy(&v).into_owned())),
                Ok(None) => Ok(TypedValue::Null),
                Err(e) => Err(mapping_err(col, e)),
            },
        },
        TypedDataType::Bytes => match row.try_get::<Option<Vec<u8>>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Bytes(v)),
            Ok(None) => Ok(TypedValue::Null),
            Err(e) => Err(mapping_err(col, e)),
        },
        TypedDataType::Date => match row.try_get::<Option<chrono::NaiveDate>, _>(idx) {
            Ok(Some(d)) => {
                use chrono::Datelike;
                let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 valid");
                Ok(TypedValue::Date(
                    d.num_days_from_ce() - epoch.num_days_from_ce(),
                ))
            }
            Ok(None) => Ok(TypedValue::Null),
            Err(e) => Err(mapping_err(col, e)),
        },
        TypedDataType::Timestamp => {
            // DATETIME → NaiveDateTime (no timezone); TIMESTAMP sqlx
            // decodes as DateTime<Utc>. Try UTC first, fall back to naive.
            match row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(idx) {
                Ok(Some(ts)) => {
                    let micros = ts.timestamp() * 1_000_000 + (ts.timestamp_subsec_micros() as i64);
                    Ok(TypedValue::Timestamp(micros))
                }
                Ok(None) => Ok(TypedValue::Null),
                Err(_) => match row.try_get::<Option<chrono::NaiveDateTime>, _>(idx) {
                    Ok(Some(ts)) => {
                        let micros = ts.and_utc().timestamp() * 1_000_000
                            + (ts.and_utc().timestamp_subsec_micros() as i64);
                        Ok(TypedValue::Timestamp(micros))
                    }
                    Ok(None) => Ok(TypedValue::Null),
                    Err(e) => Err(mapping_err(col, e)),
                },
            }
        }
        TypedDataType::Decimal { .. } => match row.try_get::<Option<String>, _>(idx) {
            // `select_expr_for_mysql_type` casts DECIMAL to CHAR, so the
            // value arrives as a canonical decimal string.
            Ok(Some(v)) => Ok(TypedValue::Decimal(v)),
            Ok(None) => Ok(TypedValue::Null),
            Err(e) => Err(mapping_err(col, e)),
        },
        TypedDataType::Json => match row.try_get::<Option<serde_json::Value>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Json(v)),
            Ok(None) => Ok(TypedValue::Null),
            Err(e) => Err(mapping_err(col, e)),
        },
        TypedDataType::Unknown => match row.try_get::<Option<String>, _>(idx) {
            Ok(Some(v)) => Ok(TypedValue::Text(v)),
            Ok(None) => Ok(TypedValue::Null),
            Err(e) => Err(mapping_err(col, e)),
        },
    }
}

// ── Connector ─────────────────────────────────────────────────────────────────

/// MySQL / MariaDB connector backed by `sqlx::MySqlPool`.
pub struct MysqlConnector {
    pool: MySqlPool,
    cached_schema: SchemaInfo,
    /// In-pod memory backstop for `execute_query_full` (see [`ResultCap`]).
    result_cap: ResultCap,
}

impl MysqlConnector {
    /// Build a pool, probe the connection, and pre-fetch the database schema.
    pub async fn new(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        database: &str,
    ) -> Result<Self, ConnectorError> {
        let url = format!(
            "mysql://{}:{}@{}:{}/{}",
            urlencode(user),
            urlencode(password),
            host,
            port,
            urlencode(database),
        );

        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;

        let cached_schema = fetch_schema(&pool, database).await.unwrap_or_default();

        Ok(Self {
            pool,
            cached_schema,
            result_cap: ResultCap::default(),
        })
    }

    /// Override the [`ResultCap`] memory backstop. Primarily for tests that need
    /// to trip the guard on a small result.
    pub fn with_result_cap(mut self, cap: ResultCap) -> Self {
        self.result_cap = cap;
        self
    }
}

/// Minimal URL-encoder for MySQL DSN credentials. Handles the small set of
/// reserved characters that most commonly appear in passwords: `@`, `:`,
/// `/`, `%`, `?`, `#`, `&`, `=`, space. Everything else passes through.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '@' | ':' | '/' | '%' | '?' | '#' | '&' | '=' | ' ' => {
                for byte in c.to_string().as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            _ => out.push(c),
        }
    }
    out
}

// ── DatabaseConnector impl ────────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for MysqlConnector {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Other("MySQL")
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "mysql"),
            otel.kind = "client",
            db.system.name = "mysql",
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
        // The temp-table wrap below only accepts one SELECT-family
        // statement; a multi-statement DDL/DML script substituted into
        // `({sql})` is a parser error. Run leading statements for side
        // effects, sample only the final statement.
        let script = plan_sql_script(sql);
        let tmp = "_agentic_tmp";

        // Acquire one connection for the whole method. MySQL TEMPORARY TABLE
        // are session-scoped — dispatching queries through the pool would risk
        // each query landing on a different connection where the temp table
        // does not exist.
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;

        for stmt in &script.prefix {
            sqlx::query(AssertSqlSafe(stmt.clone()))
                .execute(&mut *conn)
                .await
                .map_err(|e| ConnectorError::query_failed(stmt.clone(), e.to_string()))?;
        }

        let sql = normalize_sql(&script.final_stmt);

        if !is_returning_statement(sql) {
            if !sql.is_empty() {
                sqlx::query(AssertSqlSafe(sql))
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
            }
            return Ok(ExecutionResult::empty());
        }

        // 1. Drop any leftover temp table from a previous (failed) execution.
        sqlx::query(AssertSqlSafe(format!(
            "DROP TEMPORARY TABLE IF EXISTS {tmp}"
        )))
        .execute(&mut *conn)
        .await
        .ok();

        // 2. Materialise.
        sqlx::query(AssertSqlSafe(format!(
            "CREATE TEMPORARY TABLE {tmp} AS ({sql})"
        )))
        .execute(&mut *conn)
        .await
        .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        // 3. Column names / types from information_schema — scoped to
        //    `_agentic_tmp` in the current DB.
        let info_sql = "\
            SELECT column_name, data_type \
            FROM information_schema.columns \
            WHERE table_schema = DATABASE() \
              AND table_name = ? \
            ORDER BY ordinal_position";
        let info_rows: Vec<(String, String)> = sqlx::query_as(info_sql)
            .bind(tmp)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| ConnectorError::query_failed(info_sql.to_string(), e.to_string()))?;
        let column_names: Vec<String> = info_rows.iter().map(|(n, _)| n.clone()).collect();
        let column_types: Vec<String> = info_rows.iter().map(|(_, t)| t.clone()).collect();

        // 4. Total row count.
        let count_sql = format!("SELECT COUNT(*) AS n FROM {tmp}");
        let total_row_count: u64 = sqlx::query_scalar::<_, i64>(AssertSqlSafe(count_sql.clone()))
            .fetch_one(&mut *conn)
            .await
            .map(|n| n as u64)
            .map_err(|e| ConnectorError::query_failed(count_sql.clone(), e.to_string()))?;

        // 5. Sample rows — cast every column to CHAR.
        let col_count = column_names.len();
        let sample_rows: Vec<QueryRow> = if col_count == 0 {
            Vec::new()
        } else {
            let cast_cols: String = column_names
                .iter()
                .map(|c| format!("CAST(`{}` AS CHAR)", c.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(", ");
            let sample_sql = format!("SELECT {cast_cols} FROM {tmp} LIMIT {sample_limit}");
            let rows = sqlx::query(AssertSqlSafe(sample_sql.clone()))
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| ConnectorError::query_failed(sample_sql.clone(), e.to_string()))?;
            rows.iter()
                .map(|r| {
                    let cells = (0..col_count)
                        .map(|i| {
                            let v: Option<String> = r.try_get(i).ok().flatten();
                            text_to_cell(v)
                        })
                        .collect();
                    QueryRow(cells)
                })
                .collect()
        };

        // 6. Per-column stats — single batched query instead of 2N round-trips.
        //
        // Each column contributes 6 aliased expressions. CAST(col AS DOUBLE)
        // returns NULL for non-numeric columns so mean/std_dev are NULL
        // naturally; no per-column error handling needed.
        let col_stats: Vec<ColumnStats> = if col_count == 0 {
            Vec::new()
        } else {
            let exprs: String = column_names
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let q = format!("`{}`", col.replace('`', "``"));
                    format!(
                        "COUNT(*) - COUNT({q}) AS c{i}_nc, \
                         COUNT(DISTINCT {q}) AS c{i}_dc, \
                         CAST(MIN({q}) AS CHAR) AS c{i}_mn, \
                         CAST(MAX({q}) AS CHAR) AS c{i}_mx, \
                         AVG(CAST({q} AS DOUBLE)) AS c{i}_avg, \
                         STDDEV_POP(CAST({q} AS DOUBLE)) AS c{i}_sd"
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let stats_sql = format!("SELECT {exprs} FROM {tmp}");
            let stats_row = sqlx::query(AssertSqlSafe(stats_sql.clone()))
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| ConnectorError::query_failed(stats_sql.clone(), e.to_string()))?;

            column_names
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let null_count: u64 = stats_row
                        .try_get::<i64, _>(format!("c{i}_nc").as_str())
                        .map(|n| n as u64)
                        .unwrap_or(0);
                    let distinct_count: u64 = stats_row
                        .try_get::<i64, _>(format!("c{i}_dc").as_str())
                        .map(|n| n as u64)
                        .unwrap_or(0);
                    let min_v = text_to_cell(
                        stats_row
                            .try_get::<Option<String>, _>(format!("c{i}_mn").as_str())
                            .ok()
                            .flatten(),
                    );
                    let max_v = text_to_cell(
                        stats_row
                            .try_get::<Option<String>, _>(format!("c{i}_mx").as_str())
                            .ok()
                            .flatten(),
                    );
                    let mean = stats_row
                        .try_get::<Option<f64>, _>(format!("c{i}_avg").as_str())
                        .ok()
                        .flatten();
                    let std_dev = stats_row
                        .try_get::<Option<f64>, _>(format!("c{i}_sd").as_str())
                        .ok()
                        .flatten();
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
                .collect()
        };

        // 7. Cleanup (fire-and-forget).
        let _ = sqlx::query(AssertSqlSafe(format!(
            "DROP TEMPORARY TABLE IF EXISTS {tmp}"
        )))
        .execute(&mut *conn)
        .await;

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
            otel.name = %crate::telemetry::span_name(sql, "mysql"),
            otel.kind = "client",
            db.system.name = "mysql",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let sql = normalize_sql(sql);
        use sqlx::Statement;

        // Meta-statements like SHOW/DESCRIBE/EXPLAIN produce result sets but
        // cannot be used as derived-table subqueries. Detect them early so we
        // can adjust both the column type mapping and the execution strategy.
        let first_kw = sql
            .trim_start()
            .split_ascii_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let wrappable = matches!(first_kw.as_str(), "SELECT" | "WITH" | "TABLE" | "VALUES");

        // Use the prepared-statement protocol to get column type info without
        // creating a temp table. Falls back to execute() for DDL / DML that
        // MySQL can't prepare (CREATE TABLE, INSERT without RETURNING, etc.).
        let stmt = match (&self.pool)
            .prepare(AssertSqlSafe(sql).into_sql_str())
            .await
        {
            Ok(s) => s,
            Err(_) => {
                return match sqlx::query(AssertSqlSafe(sql)).execute(&self.pool).await {
                    Ok(_) => Ok(TypedRowStream::from_rows(vec![], vec![])),
                    Err(e) => Err(ConnectorError::query_failed(sql.to_string(), e.to_string())),
                };
            }
        };

        let mysql_cols = stmt.columns();
        if mysql_cols.is_empty() {
            // Statement prepared but returns no result set (e.g. DML).
            sqlx::query(AssertSqlSafe(sql))
                .execute(&self.pool)
                .await
                .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
            return Ok(TypedRowStream::from_rows(vec![], vec![]));
        }

        let mysql_typnames: Vec<String> = mysql_cols
            .iter()
            .map(|c| c.type_info().name().to_string())
            .collect();

        // Build cast expressions and column specs in one pass so that
        // binary-like types (BINARY, VARBINARY, BLOB family) that are cast to
        // CHAR in the wrapper query are advertised as Text — not Bytes —
        // ensuring decode_cell uses try_get::<String>.
        //
        // For meta-statements (non-wrappable), override all types to Text
        // because MySQL's binary protocol reports information_schema columns
        // as BLOB even though they contain UTF-8 text.
        let mut columns: Vec<ColumnSpec> = Vec::with_capacity(mysql_cols.len());
        let mut cast_exprs: Vec<String> = Vec::with_capacity(mysql_cols.len());
        for (c, tn) in mysql_cols.iter().zip(mysql_typnames.iter()) {
            let quoted = format!("`{}`", c.name().replace('`', "``"));
            let expr = select_expr_for_mysql_type(&quoted, tn);
            let native = mysql_type_to_typed(tn);
            let data_type = if !wrappable {
                TypedDataType::Text
            } else if matches!(native, TypedDataType::Bytes) && expr != quoted {
                // Was cast to CHAR — decode as Text, not raw bytes.
                TypedDataType::Text
            } else {
                native
            };
            cast_exprs.push(expr);
            columns.push(ColumnSpec {
                name: c.name().to_string(),
                data_type,
            });
        }

        // Inline subquery with per-column casts — no temp table needed.
        // Stream rows via `fetch` so we never buffer the full result set in
        // memory before the first byte leaves the server.
        use agentic_core::result::TypedRowError;
        use futures::StreamExt as _;

        let exec_sql = if wrappable {
            format!("SELECT {} FROM ({sql}) __q", cast_exprs.join(", "))
        } else {
            sql.to_string()
        };
        let pool = self.pool.clone();
        let stream_columns = columns.clone();
        let row_stream = async_stream::stream! {
            let mut fetch = sqlx::query(AssertSqlSafe(exec_sql)).fetch(&pool);
            while let Some(result) = fetch.next().await {
                yield match result {
                    Ok(row) => decode_row(&row, &stream_columns),
                    Err(e) => Err(TypedRowError::TypeMappingError {
                        column: String::new(),
                        native_type: String::new(),
                        message: e.to_string(),
                    }),
                };
            }
        };
        // MySQL streams lazily via `fetch`, so the per-batch Parquet flush
        // already bounds memory — but wrap it in the shared `ResultCap` guard
        // for uniform behavior with the eager connectors: stop at the byte/row
        // ceiling and flag truncation so a runaway scan never streams unbounded.
        let flag = Arc::new(AtomicBool::new(false));
        let inner: BoxedRowStream = Box::pin(row_stream);
        let guarded = guard_row_stream(inner, self.result_cap, flag.clone());
        Ok(TypedRowStream {
            columns,
            rows: guarded,
            truncated: Some(flag),
        })
    }

    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        Ok(self.cached_schema.clone())
    }
}

// ── Schema pre-fetch ──────────────────────────────────────────────────────────

async fn fetch_schema(pool: &MySqlPool, database: &str) -> Result<SchemaInfo, ConnectorError> {
    // Explicit CASTs are required: MySQL's binary protocol reports
    // information_schema string columns as BLOB, so sqlx would fail to
    // decode them as (String, String, String) without the cast.
    let schema_sql = "\
        SELECT CAST(table_name AS CHAR), CAST(column_name AS CHAR), CAST(data_type AS CHAR) \
        FROM information_schema.columns \
        WHERE table_schema = ? \
        ORDER BY table_name, ordinal_position";

    let rows: Vec<(String, String, String)> = sqlx::query_as(schema_sql)
        .bind(database)
        .fetch_all(pool)
        .await
        .map_err(|e| {
            ConnectorError::ConnectionError(format!("schema introspection failed: {e}"))
        })?;

    // BTreeMap preserves alphabetical table order across iterations.
    let mut map: BTreeMap<String, Vec<SchemaColumnInfo>> = BTreeMap::new();
    for (table, column, data_type) in rows {
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

// Suppress unused-import warnings when the typed-row helpers are the only
// consumers of sqlx's `Column` / `TypeInfo` — they show up in
// `select_expr_for_mysql_type`'s call sites but not at module level.
#[allow(dead_code)]
fn _col_type_info_is_used(row: &MySqlRow) {
    let _ = row
        .columns()
        .first()
        .map(|c| c.type_info().name().to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_mapping_basic() {
        assert_eq!(mysql_type_to_typed("INT"), TypedDataType::Int32);
        assert_eq!(mysql_type_to_typed("BIGINT"), TypedDataType::Int64);
        assert_eq!(mysql_type_to_typed("DOUBLE"), TypedDataType::Float64);
        assert_eq!(mysql_type_to_typed("VARCHAR"), TypedDataType::Text);
        assert_eq!(mysql_type_to_typed("BLOB"), TypedDataType::Bytes);
        assert_eq!(mysql_type_to_typed("DATE"), TypedDataType::Date);
        assert_eq!(mysql_type_to_typed("DATETIME"), TypedDataType::Timestamp);
        assert_eq!(mysql_type_to_typed("JSON"), TypedDataType::Json);
        assert_eq!(mysql_type_to_typed("BOOLEAN"), TypedDataType::Bool);
        assert!(matches!(
            mysql_type_to_typed("DECIMAL"),
            TypedDataType::Decimal { .. }
        ));
    }

    #[test]
    fn type_mapping_unsigned() {
        assert_eq!(mysql_type_to_typed("INT UNSIGNED"), TypedDataType::Int64);
        // BIGINT UNSIGNED → Decimal (full 64-bit unsigned range).
        assert!(matches!(
            mysql_type_to_typed("BIGINT UNSIGNED"),
            TypedDataType::Decimal { .. }
        ));
    }

    #[test]
    fn select_expr_passes_through_natives() {
        assert_eq!(select_expr_for_mysql_type("`x`", "INT"), "`x`");
        assert_eq!(select_expr_for_mysql_type("`x`", "JSON"), "`x`");
        assert_eq!(select_expr_for_mysql_type("`x`", "DATETIME"), "`x`");
    }

    #[test]
    fn select_expr_casts_tricky_types() {
        assert_eq!(
            select_expr_for_mysql_type("`x`", "DECIMAL"),
            "CAST(`x` AS CHAR)"
        );
        assert_eq!(
            select_expr_for_mysql_type("`x`", "TIME"),
            "CAST(`x` AS CHAR)"
        );
        assert_eq!(
            select_expr_for_mysql_type("`x`", "BIGINT UNSIGNED"),
            "CAST(`x` AS CHAR)"
        );
        assert_eq!(
            select_expr_for_mysql_type("`x`", "ENUM"),
            "CAST(`x` AS CHAR)"
        );
    }

    #[test]
    fn urlencode_escapes_reserved_chars() {
        assert_eq!(urlencode("normal"), "normal");
        assert_eq!(urlencode("pa:ss@word"), "pa%3Ass%40word");
        assert_eq!(urlencode("has space"), "has%20space");
    }
}
