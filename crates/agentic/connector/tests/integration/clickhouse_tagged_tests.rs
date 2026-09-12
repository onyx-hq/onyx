//! `execute_statement_tagged` against a real ClickHouse.
//!
//! Every write a custom-app function makes carries an audit tag naming the
//! app, function and invocation. On ClickHouse, where that tag goes decides
//! whether the write happens at all: everything after `VALUES` or
//! `FORMAT` is the row stream, not SQL, so a tag appended to the statement is
//! parsed as one more row and the insert is refused whole (`Code: 27 … expected
//! '(' before: '/*oxy.app=…'`). Every `ctx.warehouse.insert` against ClickHouse
//! failed that way in 0.5.140–0.5.144, while each side's own tests passed — the
//! host's checked the tagged string, this crate's ran untagged SQL. Only an
//! engine can see the failure, so these run the tagged path on one.
//!
//! Run with:
//!
//!   cargo nextest run -p agentic-connector --features clickhouse \
//!     --test integration -E 'test(clickhouse_tagged_tests)'

#![cfg(feature = "clickhouse")]

use agentic_connector::{ClickHouseConnector, DatabaseConnector};
use agentic_core::result::TypedValue;

use crate::clickhouse_tests::{collect_typed, skip_without_docker};

/// The tag the host builds: sqlcommenter pairs with URL-encoded values.
const TAG: &str = "oxy.app='poke-house-staging',oxy.fn='submit-receiving-report',\
                   oxy.invocation='0b9f4c7e1d2a4e5f8a6b3c2d1e0f9a8b'";

/// A table no other test or earlier run touches — the container is reused
/// across runs, and nextest runs each test in its own process.
fn unique_table(stem: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!("{stem}_{}_{nanos}", std::process::id())
}

async fn create_table(c: &ClickHouseConnector, table: &str) {
    c.execute_statement(&format!(
        "CREATE TABLE {table} (a Int32, b String) ENGINE = MergeTree ORDER BY a"
    ))
    .await
    .expect("create table");
}

async fn rows_of(c: &ClickHouseConnector, table: &str) -> Vec<(i32, String)> {
    let stream = c
        .execute_query_full(&format!("SELECT a, b FROM {table} ORDER BY a"))
        .await
        .expect("read back");
    let (_, rows) = collect_typed(stream).await;
    rows.into_iter()
        .map(|r| match (&r[0], &r[1]) {
            (TypedValue::Int32(a), TypedValue::Text(b)) => (*a, b.clone()),
            other => panic!("unexpected row shape {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_tagged_insert_lands_every_row() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };
    let table = unique_table("tagged_insert");
    create_table(&c, &table).await;

    // What `ctx.warehouse.insert` sends: `build_insert_sql`'s quoted
    // identifiers and a multi-row VALUES list — the prod failure, verbatim.
    c.execute_statement_tagged(
        &format!(r#"INSERT INTO "{table}" ("a", "b") VALUES (1, 'x'), (2, 'y')"#),
        TAG,
    )
    .await
    .expect("a tagged VALUES insert must land");
    // A statement that opens with a comment. Moving the tag in front of
    // statements whose first word is INSERT (#3158) still missed this one:
    // the first word here is `--`.
    c.execute_statement_tagged(
        &format!("-- receiving report\nINSERT INTO {table} (a, b) VALUES (3, 'z')"),
        TAG,
    )
    .await
    .expect("a comment-prefixed tagged insert must land");
    // `FORMAT` streams the rest of the body as data exactly as VALUES does.
    c.execute_statement_tagged(
        &format!(r#"INSERT INTO {table} FORMAT JSONEachRow {{"a": 4, "b": "w"}}"#),
        TAG,
    )
    .await
    .expect("a tagged FORMAT insert must land");

    assert_eq!(
        rows_of(&c, &table).await,
        vec![
            (1, "x".to_string()),
            (2, "y".to_string()),
            (3, "z".to_string()),
            (4, "w".to_string()),
        ]
    );
}

#[tokio::test]
async fn every_statement_of_a_tagged_script_lands_and_carries_the_tag() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };
    let table = unique_table("tagged_script");
    create_table(&c, &table).await;

    // ClickHouse's HTTP interface runs one statement per request, so the
    // connector splits a script; each piece must still carry the tag.
    c.execute_statement_tagged(
        &format!(
            "INSERT INTO {table} (a, b) VALUES (5, 'p');\n\
             -- the second line of a receiving report\n\
             INSERT INTO {table} (a, b) VALUES (6, 'q');"
        ),
        TAG,
    )
    .await
    .expect("a tagged two-statement script must land");
    c.execute_statement("SYSTEM FLUSH LOGS")
        .await
        .expect("flush logs");

    assert_eq!(
        rows_of(&c, &table).await,
        vec![(5, "p".to_string()), (6, "q".to_string())]
    );
    let stream = c
        .execute_query_full(&format!(
            "SELECT toInt32(count()) AS n FROM system.query_log \
             WHERE type = 'QueryFinish' AND query_kind = 'Insert' \
               AND has(tables, currentDatabase() || '.{table}') \
               AND log_comment = '{}'",
            TAG.replace('\'', "\\'")
        ))
        .await
        .expect("read query_log");
    let (_, rows) = collect_typed(stream).await;
    assert_eq!(
        rows[0][0],
        TypedValue::Int32(2),
        "both inserts, both tagged"
    );
}

#[tokio::test]
async fn the_tag_is_logged_beside_the_statement_and_never_inside_it() {
    let Some(c) = skip_without_docker().await else {
        eprintln!("skipping: Docker not available");
        return;
    };
    let table = unique_table("tagged_log");
    create_table(&c, &table).await;

    // An INSERT … SELECT has no data stream, so it succeeds wherever the tag
    // goes. That isolates the second property: the audit line reaches
    // `system.query_log`, and the SQL the engine logged is the SQL the app wrote.
    let sql = format!("INSERT INTO {table} SELECT 7, 'logged'");
    c.execute_statement_tagged(&sql, TAG)
        .await
        .expect("tagged insert");
    c.execute_statement("SYSTEM FLUSH LOGS")
        .await
        .expect("flush logs");

    let stream = c
        .execute_query_full(&format!(
            "SELECT log_comment, query FROM system.query_log \
             WHERE type = 'QueryFinish' AND query_kind = 'Insert' \
               AND has(tables, currentDatabase() || '.{table}')"
        ))
        .await
        .expect("read query_log");
    let (_, rows) = collect_typed(stream).await;
    assert_eq!(rows.len(), 1, "exactly the one insert, got {rows:?}");
    assert_eq!(rows[0][0], TypedValue::Text(TAG.to_string()));
    assert_eq!(
        rows[0][1],
        TypedValue::Text(sql),
        "the statement reaches the engine byte for byte"
    );
}
