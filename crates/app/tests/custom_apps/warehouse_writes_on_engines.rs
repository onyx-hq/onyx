//! `ctx.warehouse` writes through the real function host, on real engines.
//!
//! In 0.5.140–0.5.144 every `ctx.warehouse.insert` against ClickHouse failed
//! while every layer's own tests passed: the host's checked the audit tag as a
//! string, the connector's ran untagged SQL on a live server, and nothing sent
//! what the host sends to an engine. These do. Each drives
//! `ProjectFunctionHost::warehouse_write` — the destination allowlist,
//! `build_insert_sql`, the audit tag and the connector the workspace config
//! builds — and reads the rows back from the engine.
//!
//! ClickHouse comes from `OXY_TEST_CLICKHOUSE_URL` (CI's service container) or
//! a reused testcontainer; with `OXY_TEST_REQUIRE_CLICKHOUSE=1` its absence
//! fails the test instead of skipping it. Postgres is a per-test database from
//! [`common::fresh_db`]; DuckDB is a file inside the test's workspace.

use std::path::Path;
use std::sync::Arc;

use agentic_connector::DatabaseConnector;
use agentic_core::result::CellValue;
use oxy::adapters::workspace::builder::WorkspaceBuilder;
use oxy::config::OnMissing;
use oxy_app::agentic_wiring::OxyProjectContext;
use oxy_app::server::api::custom_apps_functions::InvocationIdentity;
use oxy_app::server::api::custom_apps_functions::host::{
    FunctionCapabilities, ProjectFunctionHost, into_arc,
};
use oxy_app::server::api::custom_apps_functions::runtime::FunctionHost;
use oxy_app::server::api::custom_apps_functions::seam::{
    FunctionProjectContext, FunctionQueryExecutor,
};
use serde_json::json;
use uuid::Uuid;

use crate::common::{Schema, fresh_db};

// ── Engines ─────────────────────────────────────────────────────────────────

struct ClickHouseServer {
    url: String,
    user: String,
    password: String,
}

/// The ClickHouse these tests write to, or `None` to skip — never `None` when
/// `OXY_TEST_REQUIRE_CLICKHOUSE=1`, which is what keeps CI from going green on
/// a suite that did not run.
async fn clickhouse() -> Option<ClickHouseServer> {
    let found = match std::env::var("OXY_TEST_CLICKHOUSE_URL") {
        Ok(url) => Ok(ClickHouseServer {
            url,
            user: std::env::var("OXY_TEST_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into()),
            password: std::env::var("OXY_TEST_CLICKHOUSE_PASSWORD").unwrap_or_default(),
        }),
        Err(_) => clickhouse_container().await,
    };
    match found {
        Ok(server) => Some(server),
        Err(e) if std::env::var("OXY_TEST_REQUIRE_CLICKHOUSE").as_deref() == Ok("1") => {
            panic!("OXY_TEST_REQUIRE_CLICKHOUSE=1 but no ClickHouse is reachable: {e}")
        }
        Err(e) => {
            eprintln!("skipping: no ClickHouse ({e})");
            None
        }
    }
}

type ClickHouseContainer =
    testcontainers::ContainerAsync<testcontainers_modules::clickhouse::ClickHouse>;

static CLICKHOUSE_CONTAINER: tokio::sync::OnceCell<ClickHouseContainer> =
    tokio::sync::OnceCell::const_new();

async fn clickhouse_container() -> Result<ClickHouseServer, String> {
    use testcontainers::runners::AsyncRunner;
    use testcontainers::{ImageExt, ReuseDirective};
    use testcontainers_modules::clickhouse::ClickHouse;

    // The same image config as agentic-connector's ClickHouse suite: reuse
    // hashes it, so matching it shares one container across both.
    let container = CLICKHOUSE_CONTAINER
        .get_or_try_init(|| async {
            ClickHouse::default()
                .with_shm_size(1024 * 1024 * 1024)
                .with_tag("25.8-alpine")
                .with_env_var("CLICKHOUSE_SKIP_USER_SETUP", "1")
                // Reuse matches on labels, not image — see agentic-connector's
                // `clickhouse_tests`, which this must match.
                .with_label("tech.oxy.test-clickhouse", "25.8-alpine")
                .with_reuse(ReuseDirective::Always)
                .start()
                .await
                .map_err(|e| format!("clickhouse testcontainer failed: {e}"))
        })
        .await?;
    let host = container.get_host().await.map_err(|e| e.to_string())?;
    let port = container
        .get_host_port_ipv4(8123)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ClickHouseServer {
        url: format!("http://{host}:{port}"),
        user: "default".into(),
        password: String::new(),
    })
}

/// A per-test Postgres database, as a `config.yml` entry.
async fn postgres_entry(name: &str) -> String {
    let (_db, url) = fresh_db(Schema::Central).await;
    let url = url::Url::parse(&url).expect("fresh_db hands back a URL");
    format!(
        "  - name: {name}\n    type: postgres\n    host: {host}\n    port: \"{port}\"\n    \
         user: {user}\n    password: {password}\n    database: {database}\n",
        host = url.host_str().expect("host"),
        port = url.port().unwrap_or(5432),
        user = url.username(),
        password = url.password().unwrap_or_default(),
        database = url.path().trim_start_matches('/'),
    )
}

// ── Host ────────────────────────────────────────────────────────────────────

/// These tests only write; a read through `ctx.query` is not what they cover.
struct NoQueries;

#[async_trait::async_trait]
impl FunctionQueryExecutor for NoQueries {
    async fn execute(
        &self,
        _connector: Arc<dyn DatabaseConnector>,
        _sql: &str,
        _max_rows: usize,
    ) -> Result<Vec<serde_json::Value>, String> {
        Err("not exercised by the warehouse write tests".into())
    }
}

struct Workspace {
    host: Arc<dyn FunctionHost>,
    project: Arc<OxyProjectContext>,
    _root: tempfile::TempDir,
}

/// A real host over a workspace whose `config.yml` declares `databases`, with
/// every one of them allowed as a write destination.
async fn workspace(databases: &str, destinations: &[&str]) -> Workspace {
    let root = tempfile::tempdir().expect("workspace dir");
    std::fs::write(
        root.path().join("config.yml"),
        format!("databases:\n{databases}models: []\n"),
    )
    .expect("write config.yml");
    let project = Arc::new(OxyProjectContext::new(manager(root.path()).await));
    let host = ProjectFunctionHost::new(
        project.clone() as Arc<dyn FunctionProjectContext>,
        Arc::new(NoQueries),
        // Only a write that lands after the invocation ends touches it.
        sea_orm::DatabaseConnection::default(),
        destinations.iter().map(|d| d.to_string()).collect(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::nil(),
        "Receiving".into(),
        FunctionCapabilities::default(),
        Default::default(),
        oxy_app::server::api::operating_graph::reach::Reach::nowhere(),
        InvocationIdentity {
            invocation_id: Uuid::new_v4(),
            function_name: "submit-receiving-report".into(),
            mode: "route".into(),
            request_id: None,
            app_slug: "receiving".into(),
            user_id: None,
            user_email: None,
        },
    );
    Workspace {
        host: into_arc(host),
        project,
        _root: root,
    }
}

async fn manager(
    root: &Path,
) -> oxy::adapters::workspace::manager::WorkspaceManager<oxy::config::WorkingCopy> {
    WorkspaceBuilder::new(Uuid::new_v4())
        .with_working_copy(root, None, OnMissing::Fail)
        .await
        .expect("config.yml loads")
        .build()
        .await
        .expect("workspace manager")
}

impl Workspace {
    async fn write(&self, op: &str, payload: serde_json::Value) -> Result<(), String> {
        self.host
            .warehouse_write(op.to_string(), payload)
            .await
            .map(|_| ())
    }

    /// `(a, b)` rows of `table`, read straight from the engine.
    async fn rows(&self, database: &str, table: &str) -> Vec<(i64, String)> {
        let connector = self
            .project
            .build_connector_for(database)
            .await
            .expect("connector");
        let result = connector
            .execute_query(&format!("SELECT a, b FROM {table} ORDER BY a"), 100)
            .await
            .expect("read back");
        result
            .result
            .rows
            .iter()
            .map(|row| match (&row.0[0], &row.0[1]) {
                (CellValue::Number(a), CellValue::Text(b)) => (*a as i64, b.clone()),
                other => panic!("unexpected row shape {other:?}"),
            })
            .collect()
    }
}

fn rows(expected: &[(i64, &str)]) -> Vec<(i64, String)> {
    expected.iter().map(|(a, b)| (*a, b.to_string())).collect()
}

fn unique_table(stem: &str) -> String {
    format!("{stem}_{}", Uuid::new_v4().simple())
}

// ── ClickHouse ──────────────────────────────────────────────────────────────

async fn clickhouse_workspace() -> Option<Workspace> {
    let ch = clickhouse().await?;
    let entry = format!(
        "  - name: ch\n    type: clickhouse\n    host: {}\n    user: {}\n    password: \"{}\"\n    \
         database: default\n",
        ch.url, ch.user, ch.password
    );
    Some(workspace(&entry, &["ch"]).await)
}

#[tokio::test]
async fn clickhouse_inserts_and_execs_land() {
    let Some(ws) = clickhouse_workspace().await else {
        return;
    };
    let table = unique_table("receiving");
    ws.write(
        "exec",
        json!({ "database": "ch", "sql": format!(
            "CREATE TABLE {table} (a Int32, b String) ENGINE = MergeTree ORDER BY a"
        ) }),
    )
    .await
    .expect("tagged DDL");

    // Exactly what failed for every app in 0.5.140–0.5.144.
    ws.write(
        "insert",
        json!({ "database": "ch", "table": table, "rows": [
            { "a": 1, "b": "x" }, { "a": 2, "b": "y" },
        ] }),
    )
    .await
    .expect("ctx.warehouse.insert on ClickHouse");
    // The first word here is `--`, which defeated deciding the tag's position
    // from the statement's first word.
    ws.write(
        "exec",
        json!({ "database": "ch", "sql": format!(
            "-- a receiving report line\nINSERT INTO {table} (a, b) VALUES (3, 'z')"
        ) }),
    )
    .await
    .expect("a comment-prefixed INSERT through ctx.warehouse.exec");

    assert_eq!(
        ws.rows("ch", &table).await,
        rows(&[(1, "x"), (2, "y"), (3, "z")])
    );
}

#[tokio::test]
async fn clickhouse_upsert_is_refused_by_name() {
    let Some(ws) = clickhouse_workspace().await else {
        return;
    };
    let table = unique_table("receiving_upsert");
    ws.write(
        "exec",
        json!({ "database": "ch", "sql": format!(
            "CREATE TABLE {table} (a Int32, b String) ENGINE = MergeTree ORDER BY a"
        ) }),
    )
    .await
    .expect("create table");

    let err = ws
        .write(
            "upsert",
            json!({ "database": "ch", "table": table, "rows": [{ "a": 1, "b": "x" }],
                    "conflictColumns": ["a"] }),
        )
        .await
        .expect_err("ON CONFLICT is not ClickHouse SQL");
    assert!(
        err.contains("upsert is not supported on ClickHouse"),
        "an app author reads what to do instead, not a row-parse error: {err}"
    );
}

// ── DuckDB and Postgres: the sqlcommenter trailer ───────────────────────────

/// Insert, upsert and exec on an engine where the trailing tag is SQL comment.
async fn assert_writes_land(ws: &Workspace, database: &str) {
    let table = unique_table("receiving");
    ws.write(
        "exec",
        json!({ "database": database, "sql": format!(
            "CREATE TABLE {table} (a INTEGER PRIMARY KEY, b TEXT)"
        ) }),
    )
    .await
    .expect("create table");
    ws.write(
        "insert",
        json!({ "database": database, "table": table, "rows": [
            { "a": 1, "b": "x" }, { "a": 2, "b": "y" },
        ] }),
    )
    .await
    .expect("insert");
    ws.write(
        "upsert",
        json!({ "database": database, "table": table, "rows": [{ "a": 1, "b": "x2" }],
                "conflictColumns": ["a"] }),
    )
    .await
    .expect("upsert");
    // A statement ending in `;` and a line comment still runs with the tag
    // placed after it. (Whether the tag survives there is not observable on
    // these engines; this guards that placing it breaks nothing.)
    ws.write(
        "exec",
        json!({ "database": database, "sql": format!(
            "INSERT INTO {table} (a, b) VALUES (3, 'z'); -- one more line"
        ) }),
    )
    .await
    .expect("exec ending in a comment");

    assert_eq!(
        ws.rows(database, &table).await,
        rows(&[(1, "x2"), (2, "y"), (3, "z")])
    );
}

#[tokio::test]
async fn duckdb_writes_land() {
    let ws = workspace(
        "  - name: duck\n    type: duckdb\n    path: warehouse.duckdb\n",
        &["duck"],
    )
    .await;
    assert_writes_land(&ws, "duck").await;
}

#[tokio::test]
async fn postgres_writes_land() {
    let ws = workspace(&postgres_entry("pg").await, &["pg"]).await;
    assert_writes_land(&ws, "pg").await;
}
