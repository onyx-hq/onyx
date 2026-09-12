//! Integration tests for the MySQL connector.
//!
//! Spins up MySQL 8 via testcontainers (root user, no password, `test`
//! database) unless `OXY_TEST_MYSQL_URL` is set. Container reused across
//! tests via `OnceCell`. Skips gracefully when Docker is unavailable.
//!
//! Run with:
//!
//!   cargo nextest run -p agentic-connector --features mysql \
//!     --test integration -E 'test(mysql_tests)'

#![cfg(feature = "mysql")]

use agentic_connector::{DatabaseConnector, MysqlConnector, ResultCap};
use agentic_core::result::{ColumnSpec, TypedDataType, TypedRowStream, TypedValue};
use futures::StreamExt;

// ── Container plumbing ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Dsn {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

static TEST_DSN: tokio::sync::OnceCell<Dsn> = tokio::sync::OnceCell::const_new();
static TEST_CONTAINER: tokio::sync::OnceCell<
    std::sync::Arc<testcontainers::ContainerAsync<testcontainers_modules::mysql::Mysql>>,
> = tokio::sync::OnceCell::const_new();

async fn test_dsn() -> Option<Dsn> {
    TEST_DSN
        .get_or_try_init(|| async {
            if let Ok(url) = std::env::var("OXY_TEST_MYSQL_URL") {
                // `mysql://user:pass@host:port/db`
                let rest = url.strip_prefix("mysql://").unwrap_or(&url);
                let (creds, rest) = rest.split_once('@').unwrap_or(("root", rest));
                let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
                let (hostport, database) = rest.split_once('/').unwrap_or((rest, "test"));
                let (host, port) = hostport.split_once(':').unwrap_or((hostport, "3306"));
                return Ok::<_, String>(Dsn {
                    host: host.to_string(),
                    port: port.parse().unwrap_or(3306),
                    user: user.to_string(),
                    password: password.to_string(),
                    database: database.to_string(),
                });
            }

            use testcontainers::runners::AsyncRunner;
            use testcontainers::{ImageExt, ReuseDirective};
            use testcontainers_modules::mysql::Mysql;

            let container = TEST_CONTAINER
                .get_or_try_init(|| async {
                    Mysql::default()
                        // 64 MB (Docker default) is too small: a parallel plan wants a 32 MB
                        // DSM segment and a REUSED container accumulates them.
                        // Must match at every setup site — reuse hashes the config.
                        // See internal-docs/workspace-source.md.
                        .with_shm_size(1024 * 1024 * 1024)
                        // Reuse matches on labels, not image. Without one of
                        // our own this adopted whatever reusable container was
                        // up (the ClickHouse or Postgres one), found no 3306,
                        // and every test here skipped — green in CI while two
                        // of them could not pass on MySQL.
                        .with_label("tech.oxy.test-mysql", "8.1")
                        .with_reuse(ReuseDirective::Always)
                        .start()
                        .await
                        .map(std::sync::Arc::new)
                        .map_err(|e| format!("mysql testcontainer failed: {e}"))
                })
                .await?;

            let host = container
                .get_host()
                .await
                .map_err(|e| format!("get_host: {e}"))?
                .to_string();
            let port = container
                .get_host_port_ipv4(3306)
                .await
                .map_err(|e| format!("get_host_port: {e}"))?;
            Ok(Dsn {
                host,
                port,
                user: "root".to_string(),
                password: String::new(),
                database: "test".to_string(),
            })
        })
        .await
        .ok()
        .cloned()
}

async fn skip_without_docker() -> Option<MysqlConnector> {
    let dsn = test_dsn().await?;
    MysqlConnector::new(&dsn.host, dsn.port, &dsn.user, &dsn.password, &dsn.database)
        .await
        .ok()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn collect_typed(stream: TypedRowStream) -> (Vec<ColumnSpec>, Vec<Vec<TypedValue>>) {
    let TypedRowStream {
        columns, mut rows, ..
    } = stream;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await {
        match row {
            Ok(cells) => out.push(cells),
            Err(e) => panic!("row stream error: {e}"),
        }
    }
    (columns, out)
}

/// A multi-row scan must come back as a bounded *partial* result with the
/// truncation flag set — MySQL streams via `fetch`, so this exercises the
/// shared `guard_row_stream` adaptor wrapping the [`ResultCap`].
#[tokio::test]
async fn result_cap_truncates_runaway_scan() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("skipping: Docker not available and OXY_TEST_MYSQL_URL not set");
        return;
    };
    let connector =
        MysqlConnector::new(&dsn.host, dsn.port, &dsn.user, &dsn.password, &dsn.database)
            .await
            .expect("connect")
            .with_result_cap(ResultCap {
                max_rows: 3,
                max_bytes: u64::MAX,
            });

    // 10-row inline generator (MySQL has no generate_series); cap at 3 rows.
    let sql = "SELECT n FROM (\
        SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 \
        UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9 UNION ALL SELECT 10\
        ) g";
    let stream = connector
        .execute_query_full(sql)
        .await
        .expect("cap returns a partial result, not an error");

    let TypedRowStream {
        mut rows,
        truncated,
        ..
    } = stream;
    let mut count = 0usize;
    while let Some(row) = rows.next().await {
        row.expect("row decodes");
        count += 1;
    }

    assert!(
        (1..=3).contains(&count),
        "row cap must bound the scan, got {count}"
    );
    assert!(
        agentic_core::result::truncation_flag_set(&truncated),
        "connector must flag the partial result truncated"
    );
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn execute_query_full_preserves_int_types() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full(
            "SELECT \
                CAST(1 AS SIGNED INTEGER) AS i, \
                CAST(2 AS UNSIGNED) AS u, \
                CAST(3 AS SIGNED) AS b, \
                CAST(4.5 AS DOUBLE) AS f",
        )
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    // Column types come from `information_schema.columns.data_type`, so we
    // just assert the decoded rows.
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0][0],
        TypedValue::Int64(_) | TypedValue::Int32(_)
    ));
    // `CAST(… AS UNSIGNED)` is BIGINT UNSIGNED, which can exceed i64: the
    // connector routes it through Decimal on purpose (`mysql_type_to_typed`).
    // This expected Int* and never ran — the container it reused was not
    // MySQL, so the suite skipped.
    assert_eq!(rows[0][1], TypedValue::Decimal("2".to_string()));
    assert!(matches!(rows[0][3], TypedValue::Float64(f) if (f - 4.5).abs() < 1e-9));

    // At least one column's ColumnSpec should surface as Int*/Float64.
    let int_cols = cols
        .iter()
        .filter(|c| matches!(c.data_type, TypedDataType::Int32 | TypedDataType::Int64))
        .count();
    assert!(int_cols >= 2, "expected at least two integer-typed columns");
}

#[tokio::test]
async fn execute_query_full_preserves_nulls() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT 1 AS a, CAST(NULL AS SIGNED) AS b")
        .await
        .unwrap();
    let (_cols, rows) = collect_typed(stream).await;

    assert!(matches!(
        rows[0][0],
        TypedValue::Int32(1) | TypedValue::Int64(1)
    ));
    assert_eq!(rows[0][1], TypedValue::Null);
}

#[tokio::test]
async fn execute_query_full_date_and_datetime() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full(
            "SELECT \
                CAST('2026-04-22' AS DATE)                    AS d, \
                CAST('2026-04-22 12:34:56' AS DATETIME)       AS dt",
        )
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Date);
    assert_eq!(cols[1].data_type, TypedDataType::Timestamp);

    let d = match rows[0][0] {
        TypedValue::Date(v) => v,
        _ => panic!("expected Date"),
    };
    assert!(
        (20_400..=20_700).contains(&d),
        "date {d} not in expected 2026-ish range"
    );
    assert!(matches!(rows[0][1], TypedValue::Timestamp(_)));
}

#[tokio::test]
async fn execute_query_full_decimal_preserves_string() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT CAST(123.45 AS DECIMAL(10, 2)) AS amt")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert!(matches!(cols[0].data_type, TypedDataType::Decimal { .. }));
    match &rows[0][0] {
        TypedValue::Decimal(s) => assert_eq!(s, "123.45"),
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[tokio::test]
async fn execute_query_full_varchar_and_text() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT 'hello' AS v, 'world' AS w")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Text);
    assert_eq!(rows[0][0], TypedValue::Text("hello".into()));
    assert_eq!(rows[0][1], TypedValue::Text("world".into()));
}

#[tokio::test]
async fn execute_query_full_json_passthrough() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT JSON_OBJECT('a', 1, 'b', 2) AS j")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Json);
    match &rows[0][0] {
        TypedValue::Json(j) => {
            assert_eq!(j, &serde_json::json!({"a": 1, "b": 2}));
        }
        other => panic!("expected Json, got {other:?}"),
    }
}

#[tokio::test]
async fn execute_query_full_no_truncation() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    // MySQL has no `generate_series`, so build rows via a recursive CTE —
    // 1000 deep at most, MySQL's default `cte_max_recursion_depth`, crossed
    // with three rows for 3000. (A 3000-deep CTE was refused with error 3636;
    // nobody saw, because this suite skipped.)
    let stream = c
        .execute_query_full(
            "WITH RECURSIVE seq(n) AS ( \
                SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1000 \
             ) SELECT seq.n * 3 - k.k AS n FROM seq \
               CROSS JOIN (SELECT 0 AS k UNION ALL SELECT 1 UNION ALL SELECT 2) k",
        )
        .await
        .unwrap();
    let (_cols, rows) = collect_typed(stream).await;
    assert_eq!(rows.len(), 3000);
}

#[tokio::test]
async fn execute_query_full_reports_query_errors() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let err = c
        .execute_query_full("SELECT * FROM does_not_exist_xyz")
        .await
        .expect_err("unknown table must error");
    match err {
        agentic_connector::ConnectorError::QueryFailed(_) => {}
        other => panic!("expected QueryFailed, got {other:?}"),
    }
}

// ── Bytes decoding ────────────────────────────────────────────────────────────

#[tokio::test]
async fn execute_query_full_information_schema_returns_text_not_bytes() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    // MySQL's binary protocol reports information_schema string columns as
    // BLOB. The connector must cast them so the frontend sees Text, not bytes.
    let stream = c
        .execute_query_full(
            "SELECT table_name, column_name, data_type \
             FROM information_schema.columns \
             WHERE table_schema = 'information_schema' \
             ORDER BY table_name, ordinal_position \
             LIMIT 5",
        )
        .await
        .unwrap();

    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(
        cols[0].data_type,
        TypedDataType::Text,
        "table_name must be Text"
    );
    assert_eq!(
        cols[1].data_type,
        TypedDataType::Text,
        "column_name must be Text"
    );
    assert_eq!(
        cols[2].data_type,
        TypedDataType::Text,
        "data_type must be Text"
    );

    assert!(!rows.is_empty(), "must return at least one row");
    for row in &rows {
        for cell in row {
            assert!(
                matches!(cell, TypedValue::Text(_) | TypedValue::Null),
                "cell must be Text, not bytes: {cell:?}"
            );
        }
    }
}

// ── introspect_schema ─────────────────────────────────────────────────────────

async fn setup_introspect_tables(c: &MysqlConnector) {
    for sql in [
        "CREATE TABLE IF NOT EXISTS oxy_test_orders (\
            order_id INT PRIMARY KEY, \
            customer_id INT NOT NULL, \
            total DOUBLE\
        )",
        "CREATE TABLE IF NOT EXISTS oxy_test_customers (\
            customer_id INT PRIMARY KEY, \
            region VARCHAR(50)\
        )",
    ] {
        let _ = c.execute_query_full(sql).await;
    }
}

/// Returns a fresh connector whose schema was fetched after the test tables exist.
async fn connector_with_test_schema() -> Option<MysqlConnector> {
    let dsn = test_dsn().await?;
    let setup = MysqlConnector::new(&dsn.host, dsn.port, &dsn.user, &dsn.password, &dsn.database)
        .await
        .ok()?;
    setup_introspect_tables(&setup).await;
    MysqlConnector::new(&dsn.host, dsn.port, &dsn.user, &dsn.password, &dsn.database)
        .await
        .ok()
}

#[tokio::test]
async fn introspect_schema_finds_tables_and_columns() {
    let Some(c) = connector_with_test_schema().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let info = c
        .introspect_schema()
        .expect("introspect_schema must succeed");

    let table = info
        .tables
        .iter()
        .find(|t| t.name == "oxy_test_orders")
        .expect("oxy_test_orders must appear in schema");

    let cols: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(cols.contains(&"order_id"), "{cols:?}");
    assert!(cols.contains(&"customer_id"), "{cols:?}");
    assert!(cols.contains(&"total"), "{cols:?}");
}

#[tokio::test]
async fn introspect_schema_column_data_types_are_populated() {
    let Some(c) = connector_with_test_schema().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let info = c.introspect_schema().expect("must succeed");
    let table = info
        .tables
        .iter()
        .find(|t| t.name == "oxy_test_orders")
        .unwrap();

    for col in &table.columns {
        assert!(
            !col.data_type.is_empty(),
            "column '{}' must have a non-empty data_type",
            col.name
        );
    }
}

#[tokio::test]
async fn introspect_schema_detects_join_key_via_shared_id_column() {
    let Some(c) = connector_with_test_schema().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let info = c.introspect_schema().expect("must succeed");

    let join = info
        .join_keys
        .iter()
        .find(|(_, _, col)| col == "customer_id");
    assert!(
        join.is_some(),
        "customer_id shared between oxy_test_orders and oxy_test_customers must be a join key; \
         join_keys = {:?}",
        info.join_keys
    );
}

#[tokio::test]
async fn introspect_schema_table_and_column_names_are_strings_not_bytes() {
    // Regression test: fetch_schema queries information_schema whose columns
    // are reported as BLOB by MySQL's binary protocol. Without explicit CASTs
    // in the SQL, table names and column names arrive as Vec<u8> and the
    // schema comes back empty (decode error swallowed by unwrap_or_default).
    let Some(c) = connector_with_test_schema().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let info = c.introspect_schema().expect("must succeed");

    assert!(
        !info.tables.is_empty(),
        "schema must contain tables — if empty, information_schema columns decoded as bytes"
    );

    let table = info
        .tables
        .iter()
        .find(|t| t.name == "oxy_test_orders")
        .expect("oxy_test_orders must appear; if missing, table_name decoded as bytes");

    assert!(
        !table.columns.is_empty(),
        "oxy_test_orders must have columns; if empty, column_name decoded as bytes"
    );
    assert!(
        table.columns.iter().all(|c| !c.data_type.is_empty()),
        "every column must have a non-empty data_type string"
    );
}
