//! Database connector abstraction.
//!
//! The FSM sends SQL to a [`DatabaseConnector`] and gets back bounded
//! results + summary stats in a single call. The database does the heavy
//! lifting. Rust holds only a capped sample.
//!
//! # Schema introspection
//!
//! Every connector that supports it can implement [`DatabaseConnector::introspect_schema`]
//! to return a vendor-neutral [`SchemaInfo`].  Callers (e.g. `AgentConfig::build_solver`)
//! use this to populate a `SchemaCatalog` with real column types, MIN/MAX bounds,
//! and sample values without knowing which database is behind the trait object.
//! Connectors that do not implement it return an empty [`SchemaInfo`] by default.

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentic_core::result::{
    BoxedRowStream, CellValue, QueryResult, TypedRowError, TypedRowStream, TypedValue,
};

// ── Dialect ───────────────────────────────────────────────────────────────────

/// The SQL dialect spoken by a connector.
///
/// Used by the solver to inject dialect-specific instructions into the LLM
/// system prompt (e.g. "Use DuckDB SQL syntax").  Each connector returns its
/// own variant from [`DatabaseConnector::dialect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    DuckDb,
    Sqlite,
    Postgres,
    BigQuery,
    Snowflake,
    /// Any vendor not covered by the variants above.  The inner string is a
    /// human-readable label used only in prompts.
    Other(&'static str),
}

impl SqlDialect {
    /// The ClickHouse dialect, named once.
    ///
    /// `Other`'s inner string is a prompt label, so anything that depends on
    /// *which* vendor a dialect is must go through this constant rather than
    /// matching the label itself — otherwise relabelling silently changes
    /// behaviour with no compile error. [`ClickHouseConnector::dialect`]
    /// returns it.
    ///
    /// [`ClickHouseConnector::dialect`]: crate::clickhouse::ClickHouseConnector
    pub const CLICKHOUSE: SqlDialect = SqlDialect::Other("ClickHouse");

    /// Whether this engine parses `INSERT … ON CONFLICT (…) DO UPDATE SET …`.
    ///
    /// That spelling is Postgres's, which DuckDB and SQLite adopted; no other
    /// engine here parses it. ClickHouse fails it least legibly: `ON CONFLICT`
    /// lands in the `VALUES` row stream and is refused as a row it could not
    /// parse (`Code: 27`). Redshift reports [`Self::Postgres`], because it
    /// shares the wire protocol, but does not parse it either — there the
    /// engine still refuses the statement itself.
    pub fn parses_on_conflict(self) -> bool {
        matches!(self, Self::Postgres | Self::DuckDb | Self::Sqlite)
    }

    /// A concise, human-readable name for prompt injection.
    pub fn as_str(self) -> &'static str {
        match self {
            SqlDialect::DuckDb => "DuckDB",
            SqlDialect::Sqlite => "SQLite",
            SqlDialect::Postgres => "PostgreSQL",
            SqlDialect::BigQuery => "BigQuery",
            SqlDialect::Snowflake => "Snowflake",
            SqlDialect::Other(s) => s,
        }
    }
}

impl fmt::Display for SqlDialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Schema introspection types ────────────────────────────────────────────────

/// Metadata about a single column as reported by the database.
#[derive(Debug, Clone, Default)]
pub struct SchemaColumnInfo {
    /// Column name (original case, as returned by the database).
    pub name: String,
    /// Database-native type string (e.g. `"INTEGER"`, `"VARCHAR"`, `"DOUBLE"`).
    pub data_type: String,
    /// Minimum value in this column (`None` if unavailable or all-NULL).
    pub min: Option<CellValue>,
    /// Maximum value in this column (`None` if unavailable or all-NULL).
    pub max: Option<CellValue>,
    /// Up to 5 distinct non-NULL sample values from this column.
    pub sample_values: Vec<CellValue>,
}

/// Metadata about a single table or view as reported by the database.
#[derive(Debug, Clone, Default)]
pub struct SchemaTableInfo {
    /// Table or view name (original case).
    pub name: String,
    pub columns: Vec<SchemaColumnInfo>,
}

/// Full database schema description returned by [`DatabaseConnector::introspect_schema`].
///
/// This is a vendor-neutral representation that callers convert into their own
/// catalog types (e.g. `SchemaCatalog::from_schema_info`).
#[derive(Debug, Clone, Default)]
pub struct SchemaInfo {
    pub tables: Vec<SchemaTableInfo>,
    /// Auto-detected or pre-declared join keys: `(table_a, table_b, join_column)`.
    pub join_keys: Vec<(String, String, String)>,
}

/// Per-column aggregate statistics computed by the database.
#[derive(Debug, Clone)]
pub struct ColumnStats {
    pub name: String,
    /// Database-native type name (e.g. "INTEGER", "VARCHAR", "TIMESTAMP").
    /// `None` when the connector cannot determine the type.
    pub data_type: Option<String>,
    pub null_count: u64,
    pub distinct_count: Option<u64>,
    pub min: Option<CellValue>,
    pub max: Option<CellValue>,
    pub mean: Option<f64>,
    pub std_dev: Option<f64>,
}

/// Summary statistics for a query result, computed by the database.
#[derive(Debug, Clone)]
pub struct ResultSummary {
    pub row_count: u64,
    pub columns: Vec<ColumnStats>,
}

/// Combined result of a connector execution: bounded rows + stats.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Bounded sample of rows.
    pub result: QueryResult,
    /// Per-column statistics computed by the database.
    pub summary: ResultSummary,
}

impl ExecutionResult {
    /// An empty result with no columns and no rows.
    ///
    /// Returned when a statement runs purely for its side effects (DDL/DML
    /// with no result set) so callers get a well-formed, zero-row result
    /// instead of a parser error from trying to sample non-returning SQL.
    pub fn empty() -> Self {
        ExecutionResult {
            result: QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                total_row_count: 0,
                truncated: false,
            },
            summary: ResultSummary {
                row_count: 0,
                columns: Vec::new(),
            },
        }
    }
}

/// Structured details for a failed query. Connectors that have access to
/// vendor-specific metadata (SQLSTATE, hints, position) populate the optional
/// fields; bare driver errors are surfaced via [`ConnectorError::query_failed`]
/// and leave the extras empty.
#[derive(Debug, Default, Clone)]
pub struct QueryFailedDetails {
    /// The SQL that produced the error. Echoed back to clients so chained
    /// connector layers (e.g. agentic temp-table wrapping) don't hide what
    /// was actually executed.
    pub sql: String,
    /// Required human-readable error message.
    pub message: String,
    /// Vendor error code or SQLSTATE if the driver exposes one
    /// (Postgres `42703`, Snowflake `100072`, MySQL `1054`, …).
    pub code: Option<String>,
    /// Additional context lines from the server (Postgres `DETAIL`, etc.).
    pub detail: Option<String>,
    /// Server-side suggestion (Postgres `HINT`).
    pub hint: Option<String>,
    /// 1-based character offset into `sql` where the error was detected
    /// (Postgres `POSITION`). UI uses this to highlight the offending token.
    pub position: Option<u32>,
}

#[derive(Debug)]
pub enum ConnectorError {
    QueryFailed(QueryFailedDetails),
    ConnectionError(String),
    Other(String),
}

impl ConnectorError {
    /// Build a [`ConnectorError::QueryFailed`] from just the SQL and a
    /// message. Connectors that can extract richer metadata should construct
    /// `QueryFailed(QueryFailedDetails { … })` directly.
    pub fn query_failed(sql: impl Into<String>, message: impl Into<String>) -> Self {
        Self::QueryFailed(QueryFailedDetails {
            sql: sql.into(),
            message: message.into(),
            ..Default::default()
        })
    }
}

impl fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueryFailed(d) => {
                if let Some(code) = &d.code {
                    write!(f, "query failed: [{code}] {}", d.message)?;
                } else {
                    write!(f, "query failed: {}", d.message)?;
                }
                if let Some(detail) = &d.detail {
                    write!(f, " — {detail}")?;
                }
                if let Some(hint) = &d.hint {
                    write!(f, " (hint: {hint})")?;
                }
                write!(f, "\nSQL: {}", d.sql)
            }
            Self::ConnectionError(msg) => write!(f, "connection error: {msg}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ConnectorError {}

impl From<TypedRowError> for ConnectorError {
    fn from(err: TypedRowError) -> Self {
        ConnectorError::Other(err.to_string())
    }
}

/// `sql` with `tag` appended as a sqlcommenter comment on its own line, so a
/// statement ending in a `--` comment cannot swallow it.
///
/// `*` and `/` are dropped from the tag. A well-formed tag has neither — its
/// values are URL-encoded — and without them it can neither close the comment
/// early nor open a nested one, which Postgres honours.
pub fn with_trailing_comment(sql: &str, tag: &str) -> String {
    let mut out = String::with_capacity(sql.len() + tag.len() + 5);
    out.push_str(sql.trim_end());
    out.push_str("\n/*");
    out.extend(tag.chars().filter(|c| !matches!(c, '*' | '/')));
    out.push_str("*/");
    out
}

/// Strip trailing whitespace and semicolons from a SQL string.
///
/// Backends wrap user SQL in subqueries like `CREATE TEMP TABLE t AS ({sql})`
/// or `SELECT ... FROM ({sql}) q`. A trailing `;` makes those statements
/// syntactically invalid, so every backend should call this before wrapping.
pub fn normalize_sql(sql: &str) -> &str {
    sql.trim_end().trim_end_matches(';').trim_end()
}

// ── Multi-statement scripts ───────────────────────────────────────────────────

/// A SQL script split into the statements that must run for their side
/// effects ([`prefix`]) and the single trailing statement whose result the
/// caller cares about ([`final_stmt`]).
///
/// `execute_query` exists to *sample* a result set, so every backend wraps
/// the user SQL in something like `CREATE TEMP TABLE _t AS ({sql})` or
/// `SELECT * FROM ({sql})`. That wrapping is only valid for a single
/// SELECT-family statement — a DDL/DML script
/// (`CREATE TABLE …; CREATE INDEX …; SELECT …`) substituted into the
/// parentheses produces a parser error (`syntax error at or near "CREATE"`).
///
/// [`plan_sql_script`] lets a connector run the leading statements directly
/// and reserve the wrap/sample path for [`final_stmt`].
///
/// [`prefix`]: SqlScript::prefix
/// [`final_stmt`]: SqlScript::final_stmt
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlScript {
    /// Leading statements to execute for their side effects (may be empty).
    pub prefix: Vec<String>,
    /// The trailing statement whose result the caller wants. Never empty
    /// for non-empty input (falls back to the trimmed input).
    pub final_stmt: String,
}

impl SqlScript {
    /// `true` when there is more than one statement (i.e. [`prefix`] is
    /// non-empty).
    ///
    /// [`prefix`]: SqlScript::prefix
    pub fn is_multi_statement(&self) -> bool {
        !self.prefix.is_empty()
    }
}

/// Split a SQL string into top-level statements, ignoring `;` that appears
/// inside string literals, quoted identifiers, line/block comments, and
/// Postgres dollar-quoted bodies. Each returned statement is trimmed; empty
/// statements (e.g. trailing `;` or comment-only segments) are dropped.
pub fn split_sql_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            // String literal / quoted identifier — consume until the
            // matching close quote, treating a doubled quote as an escape.
            '\'' | '"' | '`' => {
                let quote = c;
                i += 1;
                while i < chars.len() {
                    if chars[i] == quote {
                        if i + 1 < chars.len() && chars[i + 1] == quote {
                            i += 2; // escaped quote ("" / '')
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            // Line comment — skip to end of line.
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            // Block comment — skip to closing */.
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            // Postgres dollar-quoted string: $tag$ ... $tag$ (tag may be empty).
            '$' => {
                if let Some(tag_len) = dollar_tag_len(&chars[i..]) {
                    let tag: Vec<char> = chars[i..i + tag_len].to_vec();
                    i += tag_len;
                    while i < chars.len() {
                        if chars[i] == '$' && chars[i..].starts_with(tag.as_slice()) {
                            i += tag.len();
                            break;
                        }
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            ';' => {
                let stmt: String = chars[start..i].iter().collect();
                let trimmed = stmt.trim();
                if !is_blank_statement(trimmed) {
                    statements.push(trimmed.to_string());
                }
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }

    let tail: String = chars[start..].iter().collect();
    let tail = tail.trim();
    if !is_blank_statement(tail) {
        statements.push(tail.to_string());
    }
    statements
}

/// `true` when `stmt` carries no executable SQL — only whitespace and/or
/// comments. Such segments (a trailing `-- comment`, a stray `;;`) must be
/// dropped, not executed.
fn is_blank_statement(stmt: &str) -> bool {
    strip_leading_noise(stmt).is_empty()
}

/// If `chars` begins with a Postgres dollar-quote opening tag (`$$` or
/// `$ident$`), return the tag length in chars; otherwise `None`.
fn dollar_tag_len(chars: &[char]) -> Option<usize> {
    debug_assert_eq!(chars.first(), Some(&'$'));
    let mut j = 1;
    while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if j < chars.len() && chars[j] == '$' {
        Some(j + 1)
    } else {
        None
    }
}

/// Best-effort check for whether `stmt` produces a result set the caller
/// can sample. Leading comments/whitespace are stripped, then the first
/// keyword is matched. Non-returning statements (DDL/DML without
/// `RETURNING`, `SET`, etc.) should be executed for side effects only.
pub fn is_returning_statement(stmt: &str) -> bool {
    let body = strip_leading_noise(stmt);
    let kw: String = body
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        kw.as_str(),
        "SELECT"
            | "WITH"
            | "VALUES"
            | "TABLE"
            | "FROM" // DuckDB `FROM tbl` shorthand
            | "SHOW"
            | "DESCRIBE"
            | "DESC"
            | "PRAGMA"
            | "EXPLAIN"
            | "SUMMARIZE"
            | "CALL"
            | "PIVOT"
            | "UNPIVOT"
    )
}

/// Best-effort check for whether `stmt` is safe to wrap as a derived table —
/// i.e. `SELECT * FROM ( <stmt> ) AS alias LIMIT n` is valid SQL.
///
/// This is deliberately **narrower** than [`is_returning_statement`]. Several
/// statements return rows yet are *not* legal subqueries: `SHOW`, `DESCRIBE`/
/// `DESC`, `PRAGMA`, `EXPLAIN`, `SUMMARIZE`, `CALL`, `PIVOT`/`UNPIVOT`, and the
/// DuckDB `TABLE`/`FROM` shorthands. Wrapping any of those produces a parse
/// error in every dialect, and connectors like DuckDB/Postgres run them
/// unwrapped today — so a row-cap that wraps must gate on THIS, not on
/// `is_returning_statement`, or it regresses introspection queries.
///
/// Only `SELECT` and `WITH` are included: they are the statements that can
/// produce an unbounded table-scan result needing a cap and are universally
/// valid as derived tables. (`VALUES` is excluded — it yields tiny literal
/// rows that never need a cap and its syntax varies by dialect.) A `WITH` that
/// leads a write (`WITH x AS (…) INSERT …`) is a rare exception that wraps to a
/// parse error rather than executing — a safe failure, and consistent with how
/// the custom-app `/query` proxy already classifies `WITH` as read-shaped.
pub fn is_wrappable_select(stmt: &str) -> bool {
    let body = strip_leading_noise(stmt);
    let kw: String = body
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(kw.as_str(), "SELECT" | "WITH")
}

// ── Result memory backstop (ResultCap) ───────────────────────────────────────

/// Last-resort, in-pod ceiling on what a single query may materialize, enforced
/// *inside* the connector beneath the soft row `LIMIT` that ad-hoc surfaces
/// inject (`cap_ide_result_rows`, the `/query` proxy, `sample_limit`).
///
/// Why a second layer: the soft row cap bounds the row *count*, but not bytes —
/// 10 000 rows of a multi-megabyte `TEXT`/`JSON` column still peak in the
/// gigabytes — and some callers (`world_model_graph`) bypass the wrap entirely.
/// When a result exceeds either bound the connector **stops reading and returns
/// the rows gathered so far, flagged truncated** (via [`TypedRowStream::with_truncation`]).
/// It never errors and never OOM-kills the (multi-tenant) pod. This mirrors
/// ClickHouse's `result_overflow_mode=break` (graceful partial) over `throw`.
///
/// There is no env-var knob: the defaults are a `const` floor and tests inject a
/// tiny cap via each connector's `with_result_cap`, mirroring ClickHouse's
/// existing `with_max_result_bytes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultCap {
    /// Stop after this many rows have been gathered.
    pub max_rows: u64,
    /// Stop once the gathered rows' estimated in-pod size reaches this many bytes.
    pub max_bytes: u64,
}

impl ResultCap {
    /// 256 MiB — matches ClickHouse's `MAX_RESULT_BYTES`. With the ~3x
    /// materialization amplification (driver buffer + typed rows + Arrow) this
    /// keeps the transient peak well under 1 GiB.
    pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
    /// 1,000,000 rows — generous; `max_bytes` is the real guard. Bounds the
    /// row-count dimension so a narrow-but-enormous scan (e.g. one `INT` column)
    /// still stops in finite time/disk.
    pub const DEFAULT_MAX_ROWS: u64 = 1_000_000;

    /// `true` once the running totals reach either bound. Called after each row
    /// is gathered; `>=` so the cap is the last row kept.
    pub fn exceeded(&self, rows: u64, bytes: u64) -> bool {
        rows >= self.max_rows || bytes >= self.max_bytes
    }
}

impl Default for ResultCap {
    fn default() -> Self {
        Self {
            max_rows: Self::DEFAULT_MAX_ROWS,
            max_bytes: Self::DEFAULT_MAX_BYTES,
        }
    }
}

/// Cheap in-pod memory proxy for one typed row — the sum of its cells' sizes.
/// Not an exact wire size: fixed-width scalars use their Rust width and
/// variable-width cells (`Text`/`Bytes`/`Decimal`/`Json`) use their content
/// length. Good enough to bound the materialization peak without re-serializing.
pub fn estimate_row_bytes(row: &[TypedValue]) -> u64 {
    row.iter().map(estimate_value_bytes).sum()
}

fn estimate_value_bytes(v: &TypedValue) -> u64 {
    match v {
        TypedValue::Null | TypedValue::Bool(_) => 1,
        TypedValue::Int32(_) | TypedValue::Date(_) => 4,
        TypedValue::Int64(_) | TypedValue::Float64(_) | TypedValue::Timestamp(_) => 8,
        TypedValue::Text(s) | TypedValue::Decimal(s) => s.len() as u64,
        TypedValue::Bytes(b) => b.len() as u64,
        // `to_string().len()` sizes a JSON cell without naming the `serde_json`
        // crate (an *optional* dep of this crate) — it uses the value's inherent
        // `Display`. JSON cells are rare on the hot path, so the per-row
        // allocation is acceptable for a backstop accounting estimate.
        TypedValue::Json(j) => j.to_string().len() as u64,
    }
}

/// Wrap a streaming connector's row stream so it stops yielding once `cap` is
/// reached, setting `flag` to signal truncation. Used by connectors whose
/// driver streams rows lazily (e.g. MySQL `fetch`): the eager connectors enforce
/// the cap in their collection loop instead. The row that crosses the threshold
/// is yielded before the stream ends, so the partial result includes it.
pub fn guard_row_stream(
    mut inner: BoxedRowStream,
    cap: ResultCap,
    flag: Arc<AtomicBool>,
) -> BoxedRowStream {
    use futures::StreamExt;
    let stream = async_stream::stream! {
        let mut rows: u64 = 0;
        let mut bytes: u64 = 0;
        while let Some(item) = inner.next().await {
            match item {
                Ok(row) => {
                    rows += 1;
                    bytes += estimate_row_bytes(&row);
                    yield Ok(row);
                    if cap.exceeded(rows, bytes) {
                        flag.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                // Surface driver errors unchanged; they terminate the stream.
                Err(e) => yield Err(e),
            }
        }
    };
    Box::pin(stream)
}

/// Strip leading whitespace and SQL comments so the first real keyword can
/// be inspected.
fn strip_leading_noise(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.find('\n').map(|n| &rest[n + 1..]).unwrap_or("");
            s = s.trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map(|n| &rest[n + 2..]).unwrap_or("");
            s = s.trim_start();
        } else {
            return s;
        }
    }
}

/// Plan a (possibly multi-statement) SQL string into leading side-effect
/// statements plus the final statement to sample. For single-statement
/// input the prefix is empty and `final_stmt` is the (semicolon-stripped)
/// input.
pub fn plan_sql_script(sql: &str) -> SqlScript {
    let mut statements = split_sql_statements(sql);
    match statements.len() {
        0 => SqlScript {
            prefix: Vec::new(),
            final_stmt: sql.trim().to_string(),
        },
        _ => {
            let final_stmt = statements.pop().expect("len checked > 0");
            SqlScript {
                prefix: statements,
                final_stmt,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_sql_strips_trailing_semicolon() {
        assert_eq!(normalize_sql("SELECT 1;"), "SELECT 1");
        assert_eq!(normalize_sql("SELECT 1 ;"), "SELECT 1");
        assert_eq!(normalize_sql("SELECT 1;\n"), "SELECT 1");
        assert_eq!(normalize_sql("SELECT 1"), "SELECT 1");
        assert_eq!(normalize_sql("SELECT 1\nLIMIT 100;"), "SELECT 1\nLIMIT 100");
    }

    #[test]
    fn wrappable_select_is_narrower_than_returning() {
        // The only statements safe to wrap as a derived table.
        assert!(is_wrappable_select("SELECT 1"));
        assert!(is_wrappable_select(
            "  with t as (select 1) select * from t"
        ));
        assert!(is_wrappable_select("-- pick\nSELECT 1"));
        assert!(is_wrappable_select("/* c */ SELECT 1"));

        // Returning, but NOT valid as a derived table — wrapping would be a
        // parse error, so these must be excluded even though
        // `is_returning_statement` accepts them.
        for s in [
            "SHOW TABLES",
            "DESCRIBE t",
            "DESC t",
            "PRAGMA database_list",
            "EXPLAIN SELECT 1",
            "SUMMARIZE t",
            "CALL pragma_version()",
            "PIVOT t ON x USING sum(y)",
            "UNPIVOT t ON a, b",
            "TABLE t",
            "FROM t",
            "VALUES (1), (2)",
        ] {
            assert!(is_returning_statement(s), "precondition: {s} is returning");
            assert!(!is_wrappable_select(s), "{s} must not be wrappable");
        }

        // DDL/DML are neither returning nor wrappable.
        for s in [
            "CREATE TABLE t (a INT)",
            "INSERT INTO t VALUES (1)",
            "SET x = 1",
        ] {
            assert!(!is_wrappable_select(s), "{s} must not be wrappable");
        }
    }

    #[test]
    fn result_cap_exceeded_on_rows_or_bytes() {
        let cap = ResultCap {
            max_rows: 10,
            max_bytes: 1_000,
        };
        assert!(!cap.exceeded(9, 999));
        assert!(cap.exceeded(10, 0), "row bound trips");
        assert!(cap.exceeded(0, 1_000), "byte bound trips");
        assert!(cap.exceeded(10, 1_000));
    }

    #[test]
    fn result_cap_default_is_the_clickhouse_floor() {
        let cap = ResultCap::default();
        assert_eq!(cap.max_bytes, 256 * 1024 * 1024);
        assert_eq!(cap.max_rows, 1_000_000);
    }

    #[test]
    fn estimate_row_bytes_sums_cell_sizes() {
        let row = vec![
            TypedValue::Int64(1),             // 8
            TypedValue::Text("hello".into()), // 5
            TypedValue::Null,                 // 1
            TypedValue::Bytes(vec![0u8; 16]), // 16
        ];
        assert_eq!(estimate_row_bytes(&row), 8 + 5 + 1 + 16);
    }

    #[tokio::test]
    async fn guard_row_stream_stops_at_row_cap_and_flags() {
        use futures::StreamExt;
        let rows: Vec<Result<Vec<TypedValue>, TypedRowError>> =
            (0..100).map(|i| Ok(vec![TypedValue::Int64(i)])).collect();
        let inner: BoxedRowStream = Box::pin(futures::stream::iter(rows));
        let cap = ResultCap {
            max_rows: 3,
            max_bytes: u64::MAX,
        };
        let flag = Arc::new(AtomicBool::new(false));
        let mut guarded = guard_row_stream(inner, cap, flag.clone());

        let mut count = 0;
        while let Some(item) = guarded.next().await {
            item.unwrap();
            count += 1;
        }
        assert_eq!(count, 3, "stops at the row cap (keeps the boundary row)");
        assert!(flag.load(Ordering::Relaxed), "flags truncation");
    }

    #[tokio::test]
    async fn guard_row_stream_passes_through_under_cap() {
        use futures::StreamExt;
        let rows: Vec<Result<Vec<TypedValue>, TypedRowError>> =
            (0..5).map(|i| Ok(vec![TypedValue::Int64(i)])).collect();
        let inner: BoxedRowStream = Box::pin(futures::stream::iter(rows));
        let flag = Arc::new(AtomicBool::new(false));
        let mut guarded = guard_row_stream(inner, ResultCap::default(), flag.clone());

        let mut count = 0;
        while let Some(item) = guarded.next().await {
            item.unwrap();
            count += 1;
        }
        assert_eq!(count, 5);
        assert!(
            !flag.load(Ordering::Relaxed),
            "no truncation when fully under the cap"
        );
    }

    #[test]
    fn split_single_statement_no_split() {
        assert_eq!(split_sql_statements("SELECT 1"), vec!["SELECT 1"]);
        assert_eq!(split_sql_statements("SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(split_sql_statements("  SELECT 1 ;  \n"), vec!["SELECT 1"]);
    }

    #[test]
    fn split_multi_statement() {
        let s = split_sql_statements(
            "CREATE TABLE t (a INT);\nCREATE INDEX i ON t (a);\nSELECT 'done' AS status;",
        );
        assert_eq!(
            s,
            vec![
                "CREATE TABLE t (a INT)",
                "CREATE INDEX i ON t (a)",
                "SELECT 'done' AS status",
            ]
        );
    }

    #[test]
    fn split_ignores_semicolons_in_literals_and_comments() {
        // Semicolons inside string literals, quoted identifiers, and
        // comments must NOT split. Leading comments are preserved as part
        // of the following statement (harmless to every SQL parser, and
        // safer than rewriting the user's SQL).
        let s = split_sql_statements(
            "INSERT INTO t VALUES ('a;b', 'c''; still');\n\
             -- a comment with ; semicolon\n\
             /* block ; comment */\n\
             SELECT col AS \"weird;name\";",
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], "INSERT INTO t VALUES ('a;b', 'c''; still')");
        assert!(s[1].ends_with("SELECT col AS \"weird;name\""));
        assert!(is_returning_statement(&s[1]));
    }

    #[test]
    fn split_ignores_semicolons_in_dollar_quotes() {
        let s = split_sql_statements(
            "CREATE FUNCTION f() RETURNS int AS $$ BEGIN; RETURN 1; END; $$ LANGUAGE plpgsql;\n\
             SELECT f();",
        );
        assert_eq!(s.len(), 2);
        assert!(s[0].contains("BEGIN; RETURN 1; END;"));
        assert_eq!(s[1], "SELECT f()");
    }

    #[test]
    fn split_drops_trailing_and_blank_segments() {
        assert_eq!(
            split_sql_statements("SELECT 1;;\n  ;\n-- trailing comment\n"),
            vec!["SELECT 1"]
        );
        assert_eq!(split_sql_statements("   ;  ; "), Vec::<String>::new());
    }

    #[test]
    fn returning_statement_classification() {
        assert!(is_returning_statement("SELECT 1"));
        assert!(is_returning_statement(
            "  with x as (select 1) select * from x"
        ));
        assert!(is_returning_statement(
            "-- lead\n/* c */ FROM my_table SELECT *"
        ));
        assert!(is_returning_statement("VALUES (1),(2)"));
        assert!(is_returning_statement("summarize my_table"));
        assert!(!is_returning_statement("CREATE TABLE t (a INT)"));
        assert!(!is_returning_statement("INSERT INTO t VALUES (1)"));
        assert!(!is_returning_statement("CREATE INDEX i ON t (a)"));
        assert!(!is_returning_statement("SET search_path = 'x'"));
    }

    #[test]
    fn plan_single_vs_multi() {
        let single = plan_sql_script("SELECT 1;");
        assert!(!single.is_multi_statement());
        assert_eq!(single.prefix, Vec::<String>::new());
        assert_eq!(single.final_stmt, "SELECT 1");

        let multi = plan_sql_script(
            "CREATE TABLE t (a INT);\nCREATE INDEX i ON t (a);\nSELECT 'ready' AS status;",
        );
        assert!(multi.is_multi_statement());
        assert_eq!(
            multi.prefix,
            vec!["CREATE TABLE t (a INT)", "CREATE INDEX i ON t (a)"]
        );
        assert_eq!(multi.final_stmt, "SELECT 'ready' AS status");
    }

    #[test]
    fn plan_empty_input_falls_back() {
        let p = plan_sql_script("   ");
        assert!(!p.is_multi_statement());
        assert_eq!(p.final_stmt, "");
    }
}

/// Abstraction over database/warehouse query execution.
///
/// A single `execute_query` call returns bounded rows AND summary stats.
/// The connector decides how to do this efficiently — options include:
/// - Temp table: `CREATE TEMP TABLE _t AS (sql)`, then query _t for sample + stats
/// - Two queries: COUNT + LIMIT (acceptable for fast queries)
/// - Single pass with cursor: stream rows, compute stats incrementally, stop at limit
///
/// Connectors MUST enforce `sample_limit` — never return unbounded rows.
///
/// Connectors that support schema discovery should also implement
/// [`introspect_schema`] so callers can build a [`SchemaInfo`] without knowing
/// the underlying database technology.
///
/// [`introspect_schema`]: DatabaseConnector::introspect_schema
#[async_trait]
pub trait DatabaseConnector: Send + Sync {
    /// The SQL dialect this connector speaks.
    ///
    /// Used by the solver to inject dialect-specific instructions into the LLM
    /// prompts.  Every implementation must return a stable value — the solver
    /// reads it once at query time and does not cache it separately.
    fn dialect(&self) -> SqlDialect;

    /// Execute `sql`, return bounded rows + summary stats.
    ///
    /// `sample_limit`: max rows to include in `result.rows`.
    /// `result.total_row_count` must reflect the actual full count.
    /// `summary` must cover the full result set, not just the sample.
    async fn execute_query(
        &self,
        sql: &str,
        sample_limit: u64,
    ) -> Result<ExecutionResult, ConnectorError>;

    /// Execute `sql` and return the full result as a row-oriented stream
    /// with native column types preserved — no truncation, no stat
    /// computation.
    ///
    /// This is the path used by callers that persist results to Parquet or
    /// render them in a typed data grid (e.g. the Dev Portal SQL IDE).
    /// Connectors that do not support full-row streaming return
    /// `ConnectorError::Other("full streaming not supported")` via the
    /// default implementation.
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let _ = sql;
        Err(ConnectorError::Other(
            "full row streaming not supported by this connector".into(),
        ))
    }

    /// Like [`execute_query_full`], but for callers that do NOT need native
    /// column types and will render/stringify every value as text anyway — e.g.
    /// the world-model dashboard, whose tiles already `::VARCHAR`-cast every
    /// column. A connector MAY skip type introspection on this path (airhouse's
    /// per-query `DESCRIBE` round-trip) and surface all columns as `Text`,
    /// halving the statements it serializes per query. The typed [`execute_query_full`]
    /// stays the path for the SQL IDE / agentic callers that need real types.
    ///
    /// Default: delegate to [`execute_query_full`], so connectors without a fast
    /// path keep full typing and nothing regresses.
    ///
    /// [`execute_query_full`]: DatabaseConnector::execute_query_full
    async fn execute_query_full_untyped(
        &self,
        sql: &str,
    ) -> Result<TypedRowStream, ConnectorError> {
        self.execute_query_full(sql).await
    }

    /// Open a multi-statement transaction on a pinned connection.
    ///
    /// Every other method here is one statement, which is all analytics needs.
    /// This is the seam for callers that must write several statements
    /// atomically — see [`crate::transaction`] for why that is a distinct
    /// capability rather than a flag on `execute_query`.
    ///
    /// Default: unsupported. Only backends that can genuinely pin a session and
    /// honour `BEGIN`/`COMMIT` override this — a connector that faked it by
    /// running the statements independently would report success on a
    /// half-applied write, which is worse than refusing.
    #[cfg(feature = "transactions")]
    async fn begin_transaction(
        &self,
    ) -> Result<Box<dyn crate::transaction::SqlTransaction>, ConnectorError> {
        Err(crate::transaction::unsupported(self.dialect().as_str()))
    }

    /// Opt-in Arrow zero-copy extension.
    ///
    /// Backends whose drivers natively produce Arrow (`DuckDbConnector`,
    /// `SnowflakeConnector`) override this to return `Some(self)`. Consumers
    /// that write Parquet can use the returned trait object to skip the
    /// row → Arrow conversion step. Defaults to `None`; the caller then
    /// falls back to [`execute_query_full`].
    ///
    /// [`execute_query_full`]: DatabaseConnector::execute_query_full
    #[cfg(feature = "arrow")]
    fn as_arrow(&self) -> Option<&dyn AsArrowConnector> {
        None
    }

    /// Execute a statement and discard any result rows.
    ///
    /// Used for DDL (`CREATE SCHEMA`, `CREATE TABLE`, `INSERT`, etc.) that must
    /// **not** be wrapped in `CREATE TEMP TABLE AS (...)`.  The default
    /// implementation calls [`execute_query`] with `sample_limit = 0` and
    /// discards the result, which works for SELECT-safe connectors. Connectors
    /// that use a temp-table wrapper (e.g. DuckDB) **must** override this to
    /// execute the statement directly.
    async fn execute_statement(&self, sql: &str) -> Result<(), ConnectorError> {
        self.execute_query(sql, 0).await.map(|_| ())
    }

    /// Execute a statement that carries an audit tag, discarding any rows.
    ///
    /// `tag` is the body of a sqlcommenter comment — `key='value'` pairs with
    /// URL-encoded values — naming who ran the statement. Where it goes is the
    /// connector's decision, because only the connector knows what its engine
    /// reads as SQL. The default is the sqlcommenter convention,
    /// [`with_trailing_comment`]: Postgres and DuckDB parse it as a comment,
    /// and Airhouse can lift it into a commit message from the end of the
    /// statement. An engine that reads bytes after the SQL as data, or that has
    /// a channel beside the SQL for exactly this, overrides it.
    ///
    /// A trailing `;` is dropped before the comment goes on: connectors that
    /// split scripts (`plan_sql_script`) would otherwise read `…;\n/*tag*/` as
    /// a statement plus a comment-only one, and discard the tag with the
    /// latter. In a multi-statement script only the last statement carries it.
    ///
    /// Hand the statement over untouched. Decorating it first is the bug this
    /// method exists to prevent: the caller cannot know what the engine will
    /// make of the decoration.
    async fn execute_statement_tagged(&self, sql: &str, tag: &str) -> Result<(), ConnectorError> {
        self.execute_statement(&with_trailing_comment(normalize_sql(sql), tag))
            .await
    }

    /// Prepare for schema introspection.
    ///
    /// Connectors with lazy connections (e.g. Postgres) override this to open
    /// the connection and pre-fetch the schema.  The default is a no-op for
    /// connectors that connect eagerly at construction time.
    async fn prepare_schema(&self) -> Result<(), ConnectorError> {
        Ok(())
    }

    /// Return a vendor-neutral description of the database schema.
    ///
    /// The default implementation returns an empty [`SchemaInfo`] so
    /// connectors that do not support introspection remain valid trait
    /// objects.  Connectors that do support it should override this method
    /// and return tables, columns, types, MIN/MAX bounds, and sample values.
    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        Ok(SchemaInfo::default())
    }
}

// ── Arrow extension ─────────────────────────────────────────────────────────

/// Opt-in Arrow zero-copy extension for backends whose drivers natively produce
/// Arrow record batches.
///
/// Consumers who need typed Parquet (e.g. the Dev Portal SQL IDE) first check
/// [`DatabaseConnector::as_arrow`]; if it returns `Some`, they can pipe batches
/// directly to a Parquet writer without the row → Arrow conversion that
/// [`DatabaseConnector::execute_query_full`] would otherwise require.
///
/// Only compiled under the `arrow` feature so row-based backends (Postgres,
/// MySQL, DOMO) don't pull in `arrow` transitively.
#[cfg(feature = "arrow")]
#[async_trait]
pub trait AsArrowConnector: Send + Sync {
    /// Execute `sql` and stream the full result as Arrow `RecordBatch`es.
    async fn execute_query_arrow(&self, sql: &str) -> Result<ArrowQueryStream, ConnectorError>;
}

/// Full, strongly-typed query result as a stream of Arrow `RecordBatch`es.
///
/// Returned from [`AsArrowConnector::execute_query_arrow`]. The `'static`
/// bound on the stream makes it easy to forward through tokio tasks and HTTP
/// handlers without lifetime plumbing.
#[cfg(feature = "arrow")]
pub struct ArrowQueryStream {
    /// Arrow schema for every batch in the stream.
    pub schema: arrow::datatypes::SchemaRef,
    /// Stream of record batches preserving input row order.
    pub batches:
        futures::stream::BoxStream<'static, Result<arrow::array::RecordBatch, ConnectorError>>,
}
