//! Integration tests for the ClickHouse connector.
//!
//! Uses the ClickHouse at `OXY_TEST_CLICKHOUSE_URL` when set (CI's service
//! container), otherwise spins one up via testcontainers, reused across tests
//! via `OnceCell`. Skips if neither is available — unless
//! `OXY_TEST_REQUIRE_CLICKHOUSE=1`, which makes that a failure, because a
//! skipped test reports as a pass.
//!
//! Run with:
//!
//!   cargo nextest run -p agentic-connector --features clickhouse \
//!     --test integration -E 'test(clickhouse_tests)'

#![cfg(feature = "clickhouse")]

use agentic_connector::{ClickHouseConnector, DatabaseConnector};
use agentic_core::result::{ColumnSpec, TypedDataType, TypedRowStream, TypedValue};
use futures::StreamExt;

// ── Container plumbing ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct Conn {
    pub(crate) url: String,
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) database: String,
}

static TEST_CONN: tokio::sync::OnceCell<Conn> = tokio::sync::OnceCell::const_new();
static TEST_CONTAINER: tokio::sync::OnceCell<
    std::sync::Arc<testcontainers::ContainerAsync<testcontainers_modules::clickhouse::ClickHouse>>,
> = tokio::sync::OnceCell::const_new();

pub(crate) async fn test_connection() -> Option<Conn> {
    TEST_CONN
        .get_or_try_init(|| async {
            // Allow external CH via env for local iteration.
            if let Ok(url) = std::env::var("OXY_TEST_CLICKHOUSE_URL") {
                return Ok::<_, String>(Conn {
                    url,
                    user: std::env::var("OXY_TEST_CLICKHOUSE_USER")
                        .unwrap_or_else(|_| "default".into()),
                    password: std::env::var("OXY_TEST_CLICKHOUSE_PASSWORD").unwrap_or_default(),
                    database: std::env::var("OXY_TEST_CLICKHOUSE_DB")
                        .unwrap_or_else(|_| "default".into()),
                });
            }

            use testcontainers::runners::AsyncRunner;
            use testcontainers::{ImageExt, ReuseDirective};
            use testcontainers_modules::clickhouse::ClickHouse;

            let container = TEST_CONTAINER
                .get_or_try_init(|| async {
                    let img = ClickHouse::default() // 64 MB (Docker default) is too small: a parallel plan wants a 32 MB
                        // DSM segment and a REUSED container accumulates them.
                        // Must match at every setup site — reuse hashes the config.
                        // See internal-docs/workspace-source.md.
                        .with_shm_size(1024 * 1024 * 1024)
                        // The module defaults to 23.3; CI's service and prod
                        // run 25.8. Newer images lock the passwordless
                        // default user to localhost unless told not to.
                        .with_tag("25.8-alpine")
                        .with_env_var("CLICKHOUSE_SKIP_USER_SETUP", "1")
                        // Reuse matches on labels, not image: without one of
                        // our own it adopts whatever reusable container is up
                        // (oxy-app's test Postgres, say) and finds no 8123.
                        .with_label("tech.oxy.test-clickhouse", "25.8-alpine")
                        .with_reuse(ReuseDirective::Always);
                    img.start()
                        .await
                        .map(std::sync::Arc::new)
                        .map_err(|e| format!("clickhouse testcontainer failed: {e}"))
                })
                .await?;

            let host = container
                .get_host()
                .await
                .map_err(|e| format!("get_host: {e}"))?
                .to_string();
            let port = container
                .get_host_port_ipv4(8123)
                .await
                .map_err(|e| format!("get_host_port: {e}"))?;

            Ok(Conn {
                url: format!("http://{host}:{port}"),
                user: "default".to_string(),
                password: String::new(),
                database: "default".to_string(),
            })
        })
        .await
        .map(Conn::clone)
        .inspect_err(|e| {
            if std::env::var("OXY_TEST_REQUIRE_CLICKHOUSE").as_deref() == Ok("1") {
                panic!("OXY_TEST_REQUIRE_CLICKHOUSE=1 but no ClickHouse is reachable: {e}");
            }
        })
        .ok()
}

pub(crate) async fn skip_without_docker() -> Option<ClickHouseConnector> {
    let c = test_connection().await?;
    ClickHouseConnector::new(c.url, c.user, c.password, c.database)
        .await
        .ok()
}

// ── Helpers ─────────────────────────────────────────────────────────────────

pub(crate) async fn collect_typed(
    stream: TypedRowStream,
) -> (Vec<ColumnSpec>, Vec<Vec<TypedValue>>) {
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
                toInt32(1)   AS i32, \
                toInt64(2)   AS i64, \
                toFloat64(3.5) AS f, \
                toUInt32(4)  AS u32",
        )
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Int32);
    assert_eq!(cols[1].data_type, TypedDataType::Int64);
    assert_eq!(cols[2].data_type, TypedDataType::Float64);
    // UInt32 widens into Int64 on the Rust side.
    assert_eq!(cols[3].data_type, TypedDataType::Int64);

    assert_eq!(rows[0][0], TypedValue::Int32(1));
    assert_eq!(rows[0][1], TypedValue::Int64(2));
    assert_eq!(rows[0][2], TypedValue::Float64(3.5));
    assert_eq!(rows[0][3], TypedValue::Int64(4));
}

#[tokio::test]
async fn execute_query_full_preserves_nulls_via_nullable() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full(
            "SELECT toNullable(1)::Nullable(Int32) AS a, CAST(NULL AS Nullable(Int32)) AS b",
        )
        .await
        .unwrap();
    let (_cols, rows) = collect_typed(stream).await;

    assert_eq!(rows[0][0], TypedValue::Int32(1));
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
                toDate('2026-04-22')              AS d, \
                toDateTime('2026-04-22 12:34:56') AS dt",
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
        "date {d} out of expected 2026-ish range"
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
        .execute_query_full("SELECT toDecimal64(123.45, 2) AS amt")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert!(matches!(cols[0].data_type, TypedDataType::Decimal { .. }));
    match &rows[0][0] {
        TypedValue::Decimal(s) => {
            // Exact textual representation is stable across CH versions.
            assert_eq!(s, "123.45");
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[tokio::test]
async fn execute_query_full_array_as_json() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT [1, 2, 3] AS arr")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Json);
    match &rows[0][0] {
        TypedValue::Json(j) => {
            // ClickHouse JSONCompact serialises Array(Int64) elements as
            // JSON strings (e.g. `"1"`) because 64-bit integers can exceed
            // the safe-integer range of JavaScript. Assert the structural
            // shape, not the per-element encoding.
            let arr = j.as_array().expect("array value");
            assert_eq!(arr.len(), 3);
        }
        other => panic!("expected Json, got {other:?}"),
    }
}

#[tokio::test]
async fn execute_query_full_lowcardinality_string_is_text() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT CAST('hello' AS LowCardinality(String)) AS s")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Text);
    assert_eq!(rows[0][0], TypedValue::Text("hello".into()));
}

#[tokio::test]
async fn execute_query_full_uuid_as_text() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT toUUID('11111111-2222-3333-4444-555555555555') AS u")
        .await
        .unwrap();
    let (cols, rows) = collect_typed(stream).await;

    assert_eq!(cols[0].data_type, TypedDataType::Text);
    assert_eq!(
        rows[0][0],
        TypedValue::Text("11111111-2222-3333-4444-555555555555".into())
    );
}

#[tokio::test]
async fn execute_query_full_no_truncation() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };

    let stream = c
        .execute_query_full("SELECT number FROM system.numbers LIMIT 3000")
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

/// Behavioral guardrail for the OOM fix: the connector's memory guard
/// (`max_result_bytes` + `result_overflow_mode=break`) must cap a runaway scan
/// to a *partial* result against a real ClickHouse, instead of streaming the
/// whole column back — which is what peaked the pod at 3Gi and OOM-killed it.
///
/// We force the guard to trip on a tiny result via `with_max_result_bytes`
/// rather than generating 256 MiB. `numbers()` is generated on the fly, so this
/// stays cheap. Companion to `execute_query_full_no_truncation`, which proves
/// the default ceiling leaves small results intact.
#[tokio::test]
async fn result_byte_guard_truncates_runaway_scan() {
    let Some(conn) = test_connection().await else {
        eprintln!("skipping: Docker not available");
        return;
    };
    let connector = ClickHouseConnector::new(conn.url, conn.user, conn.password, conn.database)
        .await
        .expect("connect")
        // 1 KiB — far below a single `numbers()` block, so the guard trips
        // immediately and breaks with a partial result.
        .with_max_result_bytes(1024);

    // Without the guard this returns all 1,000,000 rows.
    let stream = connector
        .execute_query_full("SELECT number FROM system.numbers LIMIT 1000000")
        .await
        .expect("break mode returns a partial result, not an error");
    let (_cols, rows) = collect_typed(stream).await;

    // `break` returns at least the first block (never empty, never an error)...
    assert!(
        !rows.is_empty(),
        "break mode must still return a partial result"
    );
    // ...but far short of the 1M requested — proving the cap actually fired.
    assert!(
        rows.len() < 1_000_000,
        "memory guard must truncate the runaway scan, got {} rows",
        rows.len()
    );
}
