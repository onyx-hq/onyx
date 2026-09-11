//! DuckDB connector implementation.
//!
//! Uses the temp-table pattern:
//! 1. `CREATE OR REPLACE TEMP TABLE _agentic_tmp AS ({sql})` — execute once.
//! 2. `SELECT COUNT(*) FROM _agentic_tmp` — total row count.
//! 3. `SELECT * FROM _agentic_tmp LIMIT {sample_limit}` — bounded sample.
//! 4. Per-column: `COUNT()-COUNT(col), COUNT(DISTINCT col), MIN, MAX,
//!    AVG(TRY_CAST(col AS DOUBLE)), STDDEV_POP(TRY_CAST(col AS DOUBLE))`.
//! 5. `DROP TABLE IF EXISTS _agentic_tmp` — cleanup.
//!
//! File loading registers Parquet/CSV files as temp views or materialized temp
//! tables via `from_directory()` / `from_files()`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use async_trait::async_trait;
use duckdb::{Connection, types::Value};
use slugify::slugify;

use agentic_core::result::{CellValue, QueryResult, QueryRow};

use agentic_core::result::{ColumnSpec, TypedRowError, TypedRowStream, TypedValue};

use crate::connector::{
    ColumnStats, ConnectorError, DatabaseConnector, ExecutionResult, ResultCap, ResultSummary,
    SchemaColumnInfo, SchemaInfo, SchemaTableInfo, estimate_row_bytes, is_returning_statement,
    normalize_sql, plan_sql_script,
};

// Re-export Connection so callers / integration tests can construct connections
// without adding `duckdb` as a separate direct dependency.
pub use duckdb::Connection as DuckDbConnection;

// ── Load strategy & metadata ──────────────────────────────────────────────────

/// Controls whether a file is loaded lazily (view) or eagerly (temp table).
#[derive(Debug, Clone, Copy, Default)]
pub enum LoadStrategy {
    /// `CREATE TEMP VIEW` — zero memory, re-reads the file on each query.
    ///
    /// Good for large files or one-shot queries.
    #[default]
    View,
    /// `CREATE TEMP TABLE AS SELECT *` — materialized in DuckDB's in-process
    /// memory.  Good for small files or repeated queries.
    Materialized,
}

/// Metadata about a table / view registered with the connector.
#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    /// `(column_name, data_type)` pairs returned by `DESCRIBE`.
    pub columns: Vec<(String, String)>,
    pub source: TableSource,
}

/// Where a [`TableInfo`] came from.
#[derive(Debug, Clone)]
pub enum TableSource {
    File {
        path: PathBuf,
        strategy: LoadStrategy,
    },
    /// Already existed in the DuckDB connection before we touched it.
    Native,
}

mod conversion;
mod schema;

use conversion::{
    describe_type_to_typed, duckdb_to_cell, duckdb_to_cell_opt, duckdb_value_to_typed,
};
use schema::{describe_query, describe_table, detect_join_keys, parse_summarize_cell};

pub struct DuckDbConnector {
    conn: Arc<Mutex<Connection>>,
    /// Tables / views registered during construction.
    loaded_tables: Vec<TableInfo>,
    /// In-pod memory backstop for `execute_query_full` (see [`ResultCap`]).
    result_cap: ResultCap,
}

// ── Constructors ──────────────────────────────────────────────────────────────

impl DuckDbConnector {
    /// Wrap an existing, already-configured DuckDB connection.
    pub fn new(conn: Connection) -> Self {
        ensure_icu(&conn);
        Self {
            conn: Arc::new(Mutex::new(conn)),
            loaded_tables: Vec::new(),
            result_cap: ResultCap::default(),
        }
    }

    /// Override the [`ResultCap`] memory backstop. Primarily for tests that need
    /// to trip the guard on a small result.
    pub fn with_result_cap(mut self, cap: ResultCap) -> Self {
        self.result_cap = cap;
        self
    }

    /// Fresh in-memory DuckDB instance with no pre-loaded tables.
    pub fn in_memory() -> Result<Self, ConnectorError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;
        Ok(Self::new(conn))
    }

    /// Scan `dir` for `*.csv` and `*.parquet` files and register each as a
    /// view or temp table according to `strategy`.
    ///
    /// When two files share the same stem (e.g. `orders.csv` and
    /// `orders.parquet`) only the Parquet file is registered.
    pub fn from_directory(dir: &Path, strategy: LoadStrategy) -> Result<Self, ConnectorError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;

        // Set file_search_path so queries referencing CSV/Parquet filenames
        // (e.g. `FROM 'cardio_4_4.csv'`) resolve to this directory.
        if let Ok(abs_dir) = dir.canonicalize() {
            let search_path_sql = format!("SET file_search_path = '{}'", abs_dir.display());
            let _ = conn.execute_batch(&search_path_sql);
        }

        // Collect candidates: stem → (abs_path, is_parquet).
        // Prefer Parquet over CSV on collision.
        let mut candidates: HashMap<String, (PathBuf, bool)> = HashMap::new();
        let entries = std::fs::read_dir(dir)
            .map_err(|e| ConnectorError::ConnectionError(format!("cannot read directory: {e}")))?;

        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();
            if ext != "csv" && ext != "parquet" {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let is_parquet = ext == "parquet";
            candidates
                .entry(stem)
                .and_modify(|e| {
                    if is_parquet {
                        *e = (path.clone(), true);
                    }
                })
                .or_insert((path, is_parquet));
        }

        let pairs: Vec<(PathBuf, LoadStrategy)> = candidates
            .into_values()
            .map(|(p, _)| (p, strategy))
            .collect();
        let file_refs: Vec<(&Path, LoadStrategy)> =
            pairs.iter().map(|(p, s)| (p.as_path(), *s)).collect();

        Self::from_files_with_conn(conn, &file_refs)
    }

    /// Register an explicit list of files, each with its own load strategy.
    ///
    /// # Example
    /// ```ignore
    /// DuckDbConnector::from_files(&[
    ///     (Path::new("small.csv"),     LoadStrategy::Materialized),
    ///     (Path::new("large.parquet"), LoadStrategy::View),
    /// ])
    /// ```
    pub fn from_files(files: &[(&Path, LoadStrategy)]) -> Result<Self, ConnectorError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| ConnectorError::ConnectionError(e.to_string()))?;
        Self::from_files_with_conn(conn, files)
    }

    // Shared registration logic used by both `from_directory` and `from_files`.
    fn from_files_with_conn(
        conn: Connection,
        files: &[(&Path, LoadStrategy)],
    ) -> Result<Self, ConnectorError> {
        ensure_icu(&conn);
        let mut loaded_tables: Vec<TableInfo> = Vec::with_capacity(files.len());

        for (path, strategy) in files {
            let abs = path.canonicalize().map_err(|e| {
                ConnectorError::ConnectionError(format!("cannot resolve {}: {e}", path.display()))
            })?;
            let raw_stem = abs
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed");
            let name = normalize_table_name(raw_stem);
            let full_name = abs
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string().replace('"', "\"\""));
            let ext = abs
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .unwrap_or_default();

            let path_str = abs.display().to_string().replace('\'', "''");
            let src_expr = match ext.as_str() {
                "parquet" => format!("read_parquet('{path_str}')"),
                "csv" => format!("read_csv_auto('{path_str}')"),
                _ => format!("'{path_str}'"),
            };

            let create_sql = match strategy {
                LoadStrategy::View => {
                    format!(r#"CREATE OR REPLACE TEMP VIEW "{name}" AS SELECT * FROM {src_expr}"#)
                }
                LoadStrategy::Materialized => {
                    format!(r#"CREATE OR REPLACE TEMP TABLE "{name}" AS SELECT * FROM {src_expr}"#)
                }
            };

            conn.execute_batch(&create_sql)
                .map_err(|e| ConnectorError::query_failed(create_sql.clone(), e.to_string()))?;

            // Also expose the file under its full name (e.g. `oxymart.csv`) so
            // semantic-model views that declare `table: "oxymart.csv"` resolve
            // without falling through to DuckDB's file-replacement scan, which
            // does not honor `file_search_path` for quoted identifiers.
            if let Some(full) = full_name.as_deref()
                && full != name
            {
                let alias_sql =
                    format!(r#"CREATE OR REPLACE TEMP VIEW "{full}" AS SELECT * FROM "{name}""#);
                conn.execute_batch(&alias_sql)
                    .map_err(|e| ConnectorError::query_failed(alias_sql.clone(), e.to_string()))?;
            }

            // Also register a schema-qualified alias `"<stem>"."<ext>"`. The
            // semantic-model compiler renders `table: stores.parquet` as an
            // UNQUOTED `FROM stores.parquet`, which DuckDB parses as
            // schema.table; the single-identifier alias above only matches the
            // quoted form, so the unquoted reference otherwise depends on the
            // file-replacement scan (environment-sensitive; silently empty for
            // Parquet in some setups). The qualified view makes it resolve to a
            // real catalog object. Mirrors the oxy-core local pool registration.
            let stem_q = raw_stem.replace('"', "\"\"");
            let ext_q = ext.replace('"', "\"\"");
            let _ = conn.execute_batch(&format!(r#"CREATE SCHEMA IF NOT EXISTS "{stem_q}""#));
            let qualified_sql =
                format!(r#"CREATE OR REPLACE VIEW "{stem_q}"."{ext_q}" AS SELECT * FROM "{name}""#);
            let _ = conn.execute_batch(&qualified_sql);

            let columns = describe_table(&conn, &name).map_err(|e| {
                ConnectorError::query_failed(format!("DESCRIBE \"{name}\""), e.to_string())
            })?;

            loaded_tables.push(TableInfo {
                name,
                columns,
                source: TableSource::File {
                    path: abs,
                    strategy: *strategy,
                },
            });
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            loaded_tables,
            result_cap: ResultCap::default(),
        })
    }

    /// Tables / views registered during construction.
    pub fn loaded_tables(&self) -> &[TableInfo] {
        &self.loaded_tables
    }
}

// ── Table naming ─────────────────────────────────────────────────────────────

/// Derive a DuckDB table name from a file stem that is safe to reference
/// unquoted. Spaces, hyphens, and other non-identifier characters are
/// collapsed to `_`, and a leading digit is prefixed with `_` so the name is
/// a valid bare identifier.
///
/// Downstream consumers (including LLM-generated `.view.yml` files) often
/// round-trip identifiers through normalization, so keeping table names to
/// `[A-Za-z_][A-Za-z0-9_]*` avoids silent rename drift.
fn normalize_table_name(stem: &str) -> String {
    let slug = slugify!(stem, separator = "_");
    if slug.is_empty() {
        return "unnamed".to_string();
    }
    if slug.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("_{slug}")
    } else {
        slug
    }
}

/// Install + load the DuckDB ICU extension on `conn`.
///
/// Mirrors what oxy-core's DuckDB connector does on every connection
/// (`crates/core/src/connector/duckdb.rs`). ICU registers the
/// timezone-aware date/time function overloads; without it DuckDB's
/// binder fails to resolve `date_diff`/`date_trunc` calls whose part is a
/// bare string literal in some bind paths (e.g. a non-aggregated
/// `date_diff('day', <group_key>::DATE, CURRENT_DATE::DATE)` projected
/// over a populated `GROUP BY`), surfacing as a confusing
/// `No function matches … (STRING_LITERAL, DATE, DATE)` with an empty
/// candidate list. The agentic connector historically skipped this, so
/// the same SQL worked through the classic agent / SQL-IDE path (oxy-core
/// connector) but failed through automation `execute_sql` (this connector).
///
/// Best-effort: the extension is bundled with libduckdb-sys so this
/// succeeds offline, but a failure is logged rather than fatal so bare
/// in-memory connectors used in tests/sandboxes still construct.
fn ensure_icu(conn: &Connection) {
    if let Err(e) = conn
        .execute_batch("INSTALL icu;")
        .and_then(|()| conn.execute_batch("LOAD icu;"))
    {
        tracing::warn!(
            error = %e,
            "DuckDB ICU extension failed to load; date/time functions with a \
             string-literal part (date_diff/date_trunc) may fail to bind"
        );
    }
}

// ── DatabaseConnector impl ────────────────────────────────────────────────────

#[async_trait]
impl DatabaseConnector for DuckDbConnector {
    fn dialect(&self) -> crate::connector::SqlDialect {
        crate::connector::SqlDialect::DuckDb
    }

    #[cfg(feature = "arrow")]
    fn as_arrow(&self) -> Option<&dyn crate::connector::AsArrowConnector> {
        Some(self)
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "duckdb"),
            otel.kind = "client",
            db.system.name = "duckdb",
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
        // `execute_query` samples a single result set, so the rows/stats
        // path below wraps the user SQL in `CREATE TEMP TABLE _t AS (sql)`.
        // That wrap is only valid for one SELECT-family statement — a
        // multi-statement DDL/DML script (e.g. an automation `execute_sql`
        // setup file: `CREATE TABLE …; CREATE INDEX …; SELECT …`) would
        // produce `Parser Error: syntax error at or near "CREATE"`. Run
        // any leading statements for their side effects first, then sample
        // only the final statement.
        let script = plan_sql_script(sql);
        let conn = self
            .conn
            .lock()
            .map_err(|e| ConnectorError::ConnectionError(format!("mutex poisoned: {e}")))?;

        for stmt in &script.prefix {
            conn.execute_batch(stmt)
                .map_err(|e| ConnectorError::query_failed(stmt.clone(), e.to_string()))?;
        }

        let sql = normalize_sql(&script.final_stmt);

        // The final statement doesn't return rows (DDL/DML such as a
        // trailing `CREATE INDEX` or `INSERT`): execute it for its side
        // effect and return a well-formed empty result instead of feeding
        // non-returning SQL into the sampling wrap.
        if !is_returning_statement(sql) {
            if !sql.is_empty() {
                conn.execute_batch(sql)
                    .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
            }
            return Ok(ExecutionResult::empty());
        }

        let tmp = "_agentic_tmp";

        // 1. Create the temp table once from the final (returning) query.
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {tmp};"))
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        conn.execute_batch(&format!("CREATE OR REPLACE TEMP TABLE {tmp} AS ({sql});"))
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;

        // 2. Total row count.
        let total_row_count: u64 = {
            let count_sql = format!("SELECT COUNT(*) FROM {tmp}");
            conn.query_row(&count_sql, [], |row| row.get::<_, i64>(0))
                .map_err(|e| ConnectorError::query_failed(count_sql, e.to_string()))?
                as u64
        };

        // 3a. Column names — use DESCRIBE on the temp table because duckdb-rs
        //     requires the statement to be executed before column_count()
        //     and column_names() are available, and we need them first.
        let described = describe_table(&conn, tmp)
            .map_err(|e| ConnectorError::query_failed(format!("DESCRIBE {tmp}"), e.to_string()))?;
        let column_names: Vec<String> = described.iter().map(|(name, _)| name.clone()).collect();
        let column_types: Vec<String> = described.iter().map(|(_, ty)| ty.clone()).collect();

        // 3b. Sample rows.
        let col_count = column_names.len();
        let sample_rows: Vec<QueryRow> = {
            let sample_sql = format!("SELECT * FROM {tmp} LIMIT {sample_limit}");
            let mut stmt = conn
                .prepare(&sample_sql)
                .map_err(|e| ConnectorError::query_failed(sample_sql.clone(), e.to_string()))?;

            stmt.query_map([], |row| {
                let cells = (0..col_count)
                    .map(|i| row.get::<_, Value>(i).map(duckdb_to_cell))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(QueryRow(cells))
            })
            .map_err(|e| ConnectorError::query_failed(sample_sql.clone(), e.to_string()))?
            .collect::<Result<Vec<_>, duckdb::Error>>()
            .map_err(|e| ConnectorError::query_failed(sample_sql, e.to_string()))?
        };

        // 4. Per-column stats. Each column's query is best-effort: a
        // future DuckDB type, extension, or user-defined type without
        // MIN/MAX/COUNT(DISTINCT)/TRY_CAST AS DOUBLE support could
        // bind-error and otherwise tank the whole execute_query, surfacing
        // to the analytics agent as a generic "query failed" loop in the
        // reasoning trace. Match on the result and degrade just that
        // column's stats to None — same shape as the BigQuery connector.
        //
        // NB: the DuckDB version pinned today aggregates all built-in
        // complex types (MAP, LIST, STRUCT, UNION, BLOB, BIT) gracefully,
        // so this branch is defense-in-depth rather than a fix for an
        // observed bug.
        let mut col_stats: Vec<ColumnStats> = Vec::with_capacity(column_names.len());
        for (idx, col) in column_names.iter().enumerate() {
            let quoted = format!("\"{}\"", col.replace('"', "\"\""));
            let data_type = column_types.get(idx).cloned();
            let stat_sql = format!(
                "SELECT \
                    COUNT(*) - COUNT({quoted}), \
                    COUNT(DISTINCT {quoted}), \
                    MIN({quoted}), \
                    MAX({quoted}), \
                    AVG(TRY_CAST({quoted} AS DOUBLE)), \
                    STDDEV_POP(TRY_CAST({quoted} AS DOUBLE)) \
                 FROM {tmp}"
            );

            match conn.query_row(&stat_sql, [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Value>(2)?,
                    row.get::<_, Value>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                    row.get::<_, Option<f64>>(5)?,
                ))
            }) {
                Ok((null_count, distinct_count, min_v, max_v, mean, std_dev)) => {
                    col_stats.push(ColumnStats {
                        name: col.clone(),
                        data_type,
                        null_count: null_count as u64,
                        distinct_count: Some(distinct_count as u64),
                        min: Some(duckdb_to_cell(min_v)),
                        max: Some(duckdb_to_cell(max_v)),
                        mean,
                        std_dev,
                    });
                }
                Err(e) => {
                    // TODO(test): the bundled DuckDB version aggregates every
                    // built-in complex type successfully, so this arm has no
                    // realistic reproducer in `tests/integration/duckdb_tests.rs`. If a
                    // future change here breaks the degraded-stats shape it
                    // won't be caught — revisit once an extension or UDT
                    // exists that genuinely bind-errors.
                    tracing::debug!(
                        column = %col,
                        column_type = ?data_type,
                        error = %e,
                        "DuckDB per-column stats query failed; degrading to empty stats \
                         (null_count is unknown, reported as 0)"
                    );
                    col_stats.push(ColumnStats {
                        name: col.clone(),
                        data_type,
                        null_count: 0,
                        distinct_count: None,
                        min: None,
                        max: None,
                        mean: None,
                        std_dev: None,
                    });
                }
            }
        }

        // 5. Clean up.
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {tmp};"));

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

    async fn execute_statement(&self, sql: &str) -> Result<(), ConnectorError> {
        let sql = normalize_sql(sql);
        // Move the synchronous lock + `execute_batch` off the Tokio worker:
        // long warehouse CTAS calls flowing through here (e.g. preagg
        // build-plan rebuilds) would otherwise starve the runtime thread
        // that parked this task.
        let conn = self.conn.clone();
        let sql_owned = sql.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|e| ConnectorError::ConnectionError(format!("mutex poisoned: {e}")))?;
            conn.execute_batch(&sql_owned)
                .map_err(|e| ConnectorError::query_failed(sql_owned.clone(), e.to_string()))
        })
        .await
        .map_err(|e| ConnectorError::ConnectionError(format!("blocking task panicked: {e}")))?
    }

    #[tracing::instrument(
        target = "agentic_connector::query",
        name = "db.query",
        skip_all,
        err(level = "info", Display),
        fields(
            otel.name = %crate::telemetry::span_name(sql, "duckdb"),
            otel.kind = "client",
            db.system.name = "duckdb",
            db.operation.name = %crate::telemetry::operation_name(sql),
            db.query.text = %crate::telemetry::query_text(sql),
            oxy.db.method = "execute_query_full",
        )
    )]
    async fn execute_query_full(&self, sql: &str) -> Result<TypedRowStream, ConnectorError> {
        let sql = normalize_sql(sql);
        let conn = self.conn.clone();
        let sql_owned = sql.to_string();
        let cap = self.result_cap;
        // Move all blocking DuckDB work off the Tokio worker thread so concurrent
        // callers (e.g. join_all of 29 filter-count queries) can run in parallel
        // rather than serialising on the limited worker-thread pool.  Pattern is
        // identical to `execute_statement` above.
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|e| ConnectorError::ConnectionError(format!("mutex poisoned: {e}")))?;

            // DESCRIBE resolves column names + types at the logical plan level —
            // no rows are fetched, no temp table needed.
            let described = describe_query(&conn, &sql_owned).map_err(|e| {
                ConnectorError::query_failed(format!("DESCRIBE ({sql_owned})"), e.to_string())
            })?;
            let columns: Vec<ColumnSpec> = described
                .iter()
                .map(|(name, ty)| ColumnSpec {
                    name: name.clone(),
                    data_type: describe_type_to_typed(ty),
                })
                .collect();
            let col_count = columns.len();
            let column_types: Vec<_> = columns.iter().map(|c| c.data_type.clone()).collect();

            let mut stmt = conn
                .prepare(&sql_owned)
                .map_err(|e| ConnectorError::query_failed(sql_owned.clone(), e.to_string()))?;

            let rows_iter = stmt
                .query_map([], |row| {
                    let mut cells = Vec::with_capacity(col_count);
                    for i in 0..col_count {
                        let v: Value = row.get(i)?;
                        cells.push(duckdb_value_to_typed(v, &column_types[i]));
                    }
                    Ok(cells)
                })
                .map_err(|e| ConnectorError::query_failed(sql_owned.clone(), e.to_string()))?;

            // Collect eagerly so we can release the Mutex and return a `'static`
            // stream — but stop at the ResultCap so an unbounded scan can't
            // materialize gigabytes in the pod before we hand back the stream.
            let mut rows: Vec<Result<Vec<TypedValue>, TypedRowError>> = Vec::new();
            let mut bytes: u64 = 0;
            let mut truncated = false;
            for row in rows_iter {
                match row {
                    Ok(cells) => {
                        bytes += estimate_row_bytes(&cells);
                        rows.push(Ok(cells));
                    }
                    Err(e) => rows.push(Err(TypedRowError::DriverError(e.to_string()))),
                }
                if cap.exceeded(rows.len() as u64, bytes) {
                    truncated = true;
                    break;
                }
            }

            let stream = TypedRowStream::from_rows(columns, rows);
            Ok(if truncated {
                stream.with_truncation(Arc::new(AtomicBool::new(true)))
            } else {
                stream
            })
        })
        .await
        .map_err(|e| ConnectorError::ConnectionError(format!("blocking task panicked: {e}")))?
    }

    fn introspect_schema(&self) -> Result<SchemaInfo, ConnectorError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| ConnectorError::ConnectionError(format!("mutex poisoned: {e}")))?;

        // ── 1. All non-system tables + views ──────────────────────────────────
        let table_rows: Vec<(String, String)> = conn
            .prepare(
                "SELECT table_schema, table_name \
                 FROM information_schema.tables \
                 WHERE table_schema NOT IN ('information_schema', 'pg_catalog') \
                   AND table_name NOT LIKE '_agentic_%' \
                 ORDER BY table_schema, table_name",
            )
            .map_err(|e| ConnectorError::Other(e.to_string()))?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| ConnectorError::Other(e.to_string()))?
            .collect::<Result<_, duckdb::Error>>()
            .map_err(|e| ConnectorError::Other(e.to_string()))?;

        let mut tables: Vec<SchemaTableInfo> = Vec::with_capacity(table_rows.len());

        for (schema, table) in &table_rows {
            let qualified = format!("\"{schema}\".\"{table}\"");

            // ── 2. SUMMARIZE: one pass gives column_name, column_type, min, max.
            let summarize_rows: Vec<(String, String, Option<String>, Option<String>)> = conn
                .prepare(&format!("SUMMARIZE {qualified}"))
                .map_err(|e| ConnectorError::Other(e.to_string()))?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .map_err(|e| ConnectorError::Other(e.to_string()))?
                .collect::<Result<_, duckdb::Error>>()
                .map_err(|e| ConnectorError::Other(e.to_string()))?;

            let col_count = summarize_rows.len();

            // ── 3. One sample query for all columns.
            let mut samples_by_idx: Vec<Vec<CellValue>> = vec![vec![]; col_count];
            if col_count > 0 {
                let sample_res = conn
                    .prepare(&format!("SELECT * FROM {qualified} LIMIT 5"))
                    .and_then(|mut stmt| {
                        stmt.query_map([], |row| {
                            (0..col_count)
                                .map(|i| row.get::<_, Value>(i))
                                .collect::<Result<Vec<_>, _>>()
                        })
                        .map(|mapped| mapped.collect::<Result<Vec<_>, _>>())
                    });
                if let Ok(Ok(rows)) = sample_res {
                    for row_vals in rows {
                        for (i, v) in row_vals.into_iter().enumerate() {
                            if samples_by_idx[i].len() < 5
                                && let Some(cell) = duckdb_to_cell_opt(v)
                            {
                                samples_by_idx[i].push(cell);
                            }
                        }
                    }
                }
            }

            // ── 4. Build column infos from SUMMARIZE output ───────────────────
            let columns: Vec<SchemaColumnInfo> = summarize_rows
                .into_iter()
                .enumerate()
                .map(|(i, (col_name, col_type, min_str, max_str))| {
                    let min = min_str
                        .as_deref()
                        .and_then(|s| parse_summarize_cell(s, &col_type));
                    let max = max_str
                        .as_deref()
                        .and_then(|s| parse_summarize_cell(s, &col_type));
                    let sample_values = samples_by_idx.get(i).cloned().unwrap_or_default();
                    SchemaColumnInfo {
                        name: col_name,
                        data_type: col_type,
                        min,
                        max,
                        sample_values,
                    }
                })
                .collect();

            tables.push(SchemaTableInfo {
                name: table.clone(),
                columns,
            });
        }

        // ── 5. Auto-detect join keys ──────────────────────────────────────────
        let join_keys = detect_join_keys(&tables);

        Ok(SchemaInfo { tables, join_keys })
    }
}

// ── AsArrowConnector impl (feature = "arrow") ────────────────────────────────

#[cfg(feature = "arrow")]
#[async_trait]
impl crate::connector::AsArrowConnector for DuckDbConnector {
    async fn execute_query_arrow(
        &self,
        sql: &str,
    ) -> Result<crate::connector::ArrowQueryStream, ConnectorError> {
        let sql = normalize_sql(sql);
        let conn = self
            .conn
            .lock()
            .map_err(|e| ConnectorError::ConnectionError(format!("mutex poisoned: {e}")))?;

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
        let arrow_iter = stmt
            .query_arrow([])
            .map_err(|e| ConnectorError::query_failed(sql.to_string(), e.to_string()))?;
        let schema = arrow_iter.get_schema();
        // Collect eagerly; `Statement` borrows from the connection and the
        // iterator cannot outlive the lock. This matches the eager collection
        // strategy used by `execute_query_full`.
        let batches: Vec<::arrow::array::RecordBatch> = arrow_iter.collect();
        drop(stmt);

        Ok(crate::connector::ArrowQueryStream {
            schema,
            batches: Box::pin(futures::stream::iter(batches.into_iter().map(Ok))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod span_capture {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id};
        use tracing::{Event, Subscriber};
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::Context;
        use tracing_subscriber::registry::LookupSpan;

        pub type Fields = HashMap<String, String>;

        #[derive(Default, Clone)]
        pub struct Seen {
            pub spans: Arc<Mutex<Vec<(String, Fields)>>>,
            /// `(level, span it was inside, fields)`
            pub events: Arc<Mutex<Vec<(tracing::Level, String, Fields)>>>,
        }

        struct V(Fields);
        impl Visit for V {
            fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
                self.0.insert(f.name().into(), format!("{v:?}"));
            }
            fn record_str(&mut self, f: &Field, v: &str) {
                self.0.insert(f.name().into(), v.into());
            }
        }

        impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Seen {
            fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
                let mut v = V(Fields::new());
                attrs.record(&mut v);
                self.spans
                    .lock()
                    .unwrap()
                    .push((attrs.metadata().name().to_string(), v.0));
            }
            fn on_event(&self, e: &Event<'_>, ctx: Context<'_, S>) {
                let mut v = V(Fields::new());
                e.record(&mut v);
                let span = ctx
                    .event_span(e)
                    .map(|s| s.name().to_string())
                    .unwrap_or_default();
                self.events
                    .lock()
                    .unwrap()
                    .push((*e.metadata().level(), span, v.0));
            }
        }
    }

    /// Every statement is a `db.query` client span carrying the semconv
    /// attributes; a statement the warehouse rejects records its error inside
    /// that span at `info` (never `warn`+, which would also reach Sentry).
    #[tokio::test(flavor = "current_thread")]
    async fn a_query_is_a_db_span_and_a_rejected_one_records_its_error_at_info() {
        use tracing_subscriber::layer::SubscriberExt;
        let seen = span_capture::Seen::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(seen.clone()));

        let conn = DuckDbConnector::new(Connection::open_in_memory().unwrap());
        conn.execute_query("-- revenue\nselect 42 as answer", 10)
            .await
            .unwrap();
        let _ = conn.execute_query("select * from no_such_table", 10).await;

        let spans = seen.spans.lock().unwrap().clone();
        let queries: Vec<_> = spans.iter().filter(|(n, _)| n == "db.query").collect();
        assert_eq!(queries.len(), 2, "{spans:?}");
        let f = &queries[0].1;
        assert_eq!(f["otel.name"], "SELECT duckdb");
        assert_eq!(f["otel.kind"], "client");
        assert_eq!(f["db.system.name"], "duckdb");
        assert_eq!(f["db.operation.name"], "SELECT");
        assert_eq!(f["db.query.text"], "-- revenue\nselect 42 as answer");
        assert_eq!(f["oxy.db.method"], "execute_query");
        assert_eq!(f["oxy.db.sample_limit"], "10");

        let events = seen.events.lock().unwrap().clone();
        let failure = events
            .iter()
            .find(|(_, span, fields)| span == "db.query" && fields.contains_key("error"))
            .unwrap_or_else(|| panic!("the rejected statement's error is on its span: {events:?}"));
        assert_eq!(failure.0, tracing::Level::INFO);
        assert!(failure.2["error"].contains("no_such_table"), "{failure:?}");
    }

    #[test]
    fn normalize_collapses_spaces_to_underscores() {
        assert_eq!(
            normalize_table_name("c20251018_lake_sonoma_100k copy"),
            "c20251018_lake_sonoma_100k_copy"
        );
        assert_eq!(normalize_table_name("my data file"), "my_data_file");
    }

    #[test]
    fn normalize_prefixes_leading_digit() {
        assert_eq!(
            normalize_table_name("20250816_tamalpa_headlands_50k"),
            "_20250816_tamalpa_headlands_50k"
        );
    }

    #[test]
    fn normalize_leaves_clean_names_intact() {
        assert_eq!(normalize_table_name("oxymart"), "oxymart");
        assert_eq!(normalize_table_name("orders_2024"), "orders_2024");
    }

    #[test]
    fn normalize_handles_hyphens_and_dots() {
        assert_eq!(normalize_table_name("my-table.v2"), "my_table_v2");
    }

    #[test]
    fn normalize_fallback_for_empty_slug() {
        assert_eq!(normalize_table_name(""), "unnamed");
        assert_eq!(normalize_table_name("---"), "unnamed");
    }
}
