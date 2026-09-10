//! Integration tests for the full `tools::*` public API.
//!
//! Kept as a single module so tool-scoping tests can mix calls across all
//! executor/spec modules without cross-module reimports.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agentic_connector::{DatabaseConnector, SchemaInfo};
use agentic_core::result::CellValue;
use agentic_core::tools::{ToolDef, ToolError};
use serde_json::json;

use crate::SchemaCatalog;
use crate::types::ChartConfig;
use agentic_llm::validate_openai_strict_schema;

use super::{
    clarifying_tools, execute_clarifying_tool, execute_database_lookup_tool,
    execute_interpreting_tool, execute_specifying_tool, interpreting_tools, new_schema_cache,
    solving_tools, specifying_tools, suggest_chart_config, validate_chart_column_types,
};

// ── OpenAI strict-mode compliance ─────────────────────────────────────────

/// Every tool schema is sent to OpenAI with `"strict": true`.
/// That mode requires every key in `properties` to also appear in
/// `required`.  This test catches violations at compile time rather than
/// at runtime when the HTTP call fails with an opaque 400.
#[test]
fn all_tool_schemas_are_openai_strict_compatible() {
    let all: Vec<ToolDef> = clarifying_tools(false, false)
        .into_iter()
        .chain(specifying_tools(false, false))
        .chain(solving_tools())
        .chain(interpreting_tools())
        .collect();

    for tool in &all {
        let violations = validate_openai_strict_schema(&tool.parameters, tool.name);
        assert!(
            violations.is_empty(),
            "tool '{}' violates OpenAI strict mode:\n  {}",
            tool.name,
            violations.join("\n  ")
        );
    }
}

fn make_catalog() -> SchemaCatalog {
    SchemaCatalog::new()
        .add_table("orders", &["order_id", "customer_id", "revenue", "date"])
        .add_table("customers", &["customer_id", "region", "name"])
        .add_join_key("orders", "customers", "customer_id")
}

// ── Tool scoping ──────────────────────────────────────────────────────────

#[test]
fn clarifying_does_not_include_solving_tools() {
    let tools = clarifying_tools(false, false);
    let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    assert!(
        !names.contains(&"execute_preview"),
        "execute_preview must not appear in clarifying"
    );
    assert!(
        !names.contains(&"update_chart_config"),
        "update_chart_config must not appear in clarifying"
    );
}

#[test]
fn solving_does_not_include_clarifying_tools() {
    let tools = solving_tools();
    let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    assert!(
        !names.contains(&"search_catalog"),
        "search_catalog must not appear in solving"
    );
}

#[test]
fn specifying_does_not_include_solving_tools() {
    let tools = specifying_tools(false, false);
    let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
    assert!(!names.contains(&"execute_preview"));
    assert!(!names.contains(&"update_chart_config"));
}

#[test]
fn specifying_contains_expected_tools() {
    let names: Vec<&str> = specifying_tools(false, false)
        .iter()
        .map(|t| t.name)
        .collect();
    assert!(names.contains(&"get_join_path"));
    assert!(names.contains(&"sample_columns"));
    // Catalog discovery tools moved from clarifying to specifying.
    assert!(names.contains(&"search_catalog"));
}

#[test]
fn solving_contains_execute_preview() {
    let names: Vec<&str> = solving_tools().iter().map(|t| t.name).collect();
    assert!(names.contains(&"execute_preview"));
}

// ── Clarifying tool execution ─────────────────────────────────────────────

#[test]
fn search_catalog_finds_metrics_and_dimensions() {
    let cat = make_catalog();
    let result = execute_clarifying_tool(
        "search_catalog",
        serde_json::json!({ "queries": ["revenue"] }),
        &cat,
    )
    .unwrap();
    let metrics = result["metrics"].as_array().unwrap();
    assert!(
        !metrics.is_empty(),
        "revenue should match at least one metric"
    );
    assert!(
        metrics
            .iter()
            .any(|m| m["name"].as_str().unwrap_or("").contains("revenue"))
    );
    let dims = result["dimensions"].as_array().unwrap();
    assert!(!dims.is_empty(), "matched metric should have dimensions");
}

#[test]
fn search_catalog_empty_query_returns_all() {
    let cat = make_catalog();
    let result = execute_clarifying_tool(
        "search_catalog",
        serde_json::json!({ "queries": [""] }),
        &cat,
    )
    .unwrap();
    let metrics = result["metrics"].as_array().unwrap();
    // orders has `revenue`; that's at least 1 metric
    assert!(!metrics.is_empty());
}

// ── Specifying tool execution ─────────────────────────────────────────────

/// Noop connector used in specifying tests that call `get_join_path`
/// (which never touches the connector).
struct NoopConnector;

#[async_trait::async_trait]
impl DatabaseConnector for NoopConnector {
    fn dialect(&self) -> agentic_connector::SqlDialect {
        agentic_connector::SqlDialect::DuckDb
    }

    async fn execute_query(
        &self,
        _sql: &str,
        _limit: u64,
    ) -> Result<agentic_connector::ExecutionResult, agentic_connector::ConnectorError> {
        panic!("NoopConnector: execute_query must not be called in this test")
    }
}

fn noop_connectors() -> std::collections::HashMap<String, std::sync::Arc<dyn DatabaseConnector>> {
    let mut map: std::collections::HashMap<String, std::sync::Arc<dyn DatabaseConnector>> =
        std::collections::HashMap::new();
    map.insert("default".to_string(), std::sync::Arc::new(NoopConnector));
    map
}

#[tokio::test]
async fn get_join_path_known_pair() {
    let cat = make_catalog();
    let conns = noop_connectors();
    let result = execute_specifying_tool(
        "get_join_path",
        serde_json::json!({ "from_entity": "orders", "to_entity": "customers" }),
        &cat,
        &conns,
        "default",
    )
    .await
    .unwrap();
    assert!(result["path"].as_str().unwrap().contains("customer_id"));
    assert_eq!(result["join_type"], "INNER");
}

#[tokio::test]
async fn get_join_path_unknown_pair_returns_error() {
    let cat = make_catalog();
    let conns = noop_connectors();
    let err = execute_specifying_tool(
        "get_join_path",
        serde_json::json!({ "from_entity": "orders", "to_entity": "products" }),
        &cat,
        &conns,
        "default",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)));
}

// ── Interpreting tool execution ───────────────────────────────────────────

#[tokio::test]
async fn render_chart_emits_event_and_returns_ok() {
    let cols = vec!["region".to_string(), "revenue".to_string()];
    let rows: Vec<Vec<serde_json::Value>> = vec![];
    let result_sets = vec![(cols, rows)];
    let valid_charts = Arc::new(Mutex::new(Vec::new()));
    let result = execute_interpreting_tool(
        "render_chart",
        serde_json::json!({
            "chart_type": "bar_chart",
            "x": "region",
            "y": "revenue",
            "series": null,
            "name": null,
            "value": null,
            "x_axis_label": "Region",
            "y_axis_label": "Revenue",
            "result_index": null,
            "title": "Revenue by Region"
        }),
        &None,
        &result_sets,
        &valid_charts,
    )
    .await
    .unwrap();
    assert_eq!(result["ok"], true);
}

// ── suggest_chart_config ─────────────────────────────────────────────────

#[test]
fn suggest_trend_produces_line_chart() {
    let cols = vec!["date".to_string(), "revenue".to_string()];
    let cfg = suggest_chart_config(&crate::types::QuestionType::Trend, &cols).unwrap();
    assert_eq!(cfg.chart_type, "line_chart");
    assert_eq!(cfg.x.as_deref(), Some("date"));
    assert_eq!(cfg.y.as_deref(), Some("revenue"));
}

#[test]
fn suggest_breakdown_produces_bar_chart() {
    let cols = vec!["region".to_string(), "revenue".to_string()];
    let cfg = suggest_chart_config(&crate::types::QuestionType::Breakdown, &cols).unwrap();
    assert_eq!(cfg.chart_type, "bar_chart");
}

#[test]
fn suggest_single_value_returns_none() {
    let cols = vec!["total".to_string()];
    assert!(suggest_chart_config(&crate::types::QuestionType::SingleValue, &cols).is_none());
}

#[test]
fn suggest_fewer_than_two_columns_returns_none() {
    let cols = vec!["total".to_string()];
    assert!(suggest_chart_config(&crate::types::QuestionType::Breakdown, &cols).is_none());
}

// ── validate_chart_column_types ───────────────────────────────────────────

#[test]
fn column_type_check_passes_for_numeric_y() {
    let config = ChartConfig {
        chart_type: "bar_chart".to_string(),
        x: Some("region".to_string()),
        y: Some("revenue".to_string()),
        series: None,
        name: None,
        value: None,
        title: None,
        x_axis_label: None,
        y_axis_label: None,
    };
    let columns = vec!["region".to_string(), "revenue".to_string()];
    let rows = vec![vec![serde_json::json!("North"), serde_json::json!(42000.0)]];
    let errors = validate_chart_column_types(&config, &columns, &rows);
    assert!(
        errors.is_empty(),
        "numeric y column should pass: {errors:?}"
    );
}

#[test]
fn column_type_check_fails_for_stringified_numeric_y() {
    // Regression: to_2d_array stringifies Arrow numeric columns; the chart
    // renderer receives "42000.0" (a JSON string) instead of 42000.0.
    let config = ChartConfig {
        chart_type: "bar_chart".to_string(),
        x: Some("region".to_string()),
        y: Some("revenue".to_string()),
        series: None,
        name: None,
        value: None,
        title: None,
        x_axis_label: None,
        y_axis_label: None,
    };
    let columns = vec!["region".to_string(), "revenue".to_string()];
    let rows = vec![
        // Both values are JSON strings — simulates the stringification bug.
        vec![serde_json::json!("North"), serde_json::json!("42000.0")],
    ];
    let errors = validate_chart_column_types(&config, &columns, &rows);
    assert!(
        !errors.is_empty(),
        "stringified y column should produce an error"
    );
    assert!(errors[0].contains("revenue"));
}

#[test]
fn column_type_check_passes_for_pie_numeric_value() {
    let config = ChartConfig {
        chart_type: "pie_chart".to_string(),
        x: None,
        y: None,
        series: None,
        name: Some("category".to_string()),
        value: Some("share".to_string()),
        title: None,
        x_axis_label: None,
        y_axis_label: None,
    };
    let columns = vec!["category".to_string(), "share".to_string()];
    let rows = vec![vec![serde_json::json!("A"), serde_json::json!(0.4)]];
    assert!(validate_chart_column_types(&config, &columns, &rows).is_empty());
}

#[test]
fn column_type_check_skipped_when_no_rows() {
    let config = ChartConfig {
        chart_type: "bar_chart".to_string(),
        x: Some("region".to_string()),
        y: Some("revenue".to_string()),
        series: None,
        name: None,
        value: None,
        title: None,
        x_axis_label: None,
        y_axis_label: None,
    };
    let columns = vec!["region".to_string(), "revenue".to_string()];
    // Empty result set — no rows to inspect, should not error.
    let errors = validate_chart_column_types(&config, &columns, &[]);
    assert!(errors.is_empty());
}

// ── Unknown tools return ToolError::UnknownTool ───────────────────────────

#[test]
fn unknown_tool_in_clarifying_returns_error() {
    let cat = make_catalog();
    let err = execute_clarifying_tool(
        "explain_plan",
        serde_json::json!({ "sql": "SELECT 1" }),
        &cat,
    )
    .unwrap_err();
    assert!(matches!(err, ToolError::UnknownTool(_)));
}

// ── Database lookup tools ─────────────────────────────────────────────────

#[test]
fn list_tables_tool_in_clarifying_and_specifying() {
    let clar_names: Vec<&str> = clarifying_tools(false, false)
        .iter()
        .map(|t| t.name)
        .collect();
    let spec_names: Vec<&str> = specifying_tools(false, false)
        .iter()
        .map(|t| t.name)
        .collect();
    assert!(clar_names.contains(&"list_tables"));
    assert!(spec_names.contains(&"list_tables"));
}

#[test]
fn describe_table_tool_in_clarifying_and_specifying() {
    let clar_names: Vec<&str> = clarifying_tools(false, false)
        .iter()
        .map(|t| t.name)
        .collect();
    let spec_names: Vec<&str> = specifying_tools(false, false)
        .iter()
        .map(|t| t.name)
        .collect();
    assert!(clar_names.contains(&"describe_table"));
    assert!(spec_names.contains(&"describe_table"));
}

#[test]
fn list_tables_not_in_solving() {
    let names: Vec<&str> = solving_tools().iter().map(|t| t.name).collect();
    assert!(!names.contains(&"list_tables"));
    assert!(!names.contains(&"describe_table"));
}

#[test]
fn db_tools_excluded_when_has_semantic() {
    let clar = clarifying_tools(true, false);
    let spec = specifying_tools(true, false);
    let clar_names: Vec<&str> = clar.iter().map(|t| t.name).collect();
    let spec_names: Vec<&str> = spec.iter().map(|t| t.name).collect();
    assert!(!clar_names.contains(&"list_tables"));
    assert!(!clar_names.contains(&"describe_table"));
    assert!(!spec_names.contains(&"list_tables"));
    assert!(!spec_names.contains(&"describe_table"));
    // Core tools remain present.
    assert!(clar_names.contains(&"search_catalog"));
    assert!(spec_names.contains(&"sample_columns"));
}

/// Stub connector that returns a fixed schema for introspection.
struct IntrospectableStub {
    schema: SchemaInfo,
    call_count: std::sync::atomic::AtomicUsize,
}

impl IntrospectableStub {
    fn new(schema: SchemaInfo) -> Self {
        Self {
            schema,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.call_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl DatabaseConnector for IntrospectableStub {
    fn dialect(&self) -> agentic_connector::SqlDialect {
        agentic_connector::SqlDialect::DuckDb
    }

    async fn execute_query(
        &self,
        _sql: &str,
        _limit: u64,
    ) -> Result<agentic_connector::ExecutionResult, agentic_connector::ConnectorError> {
        panic!("IntrospectableStub: execute_query must not be called")
    }

    fn introspect_schema(
        &self,
    ) -> Result<agentic_connector::SchemaInfo, agentic_connector::ConnectorError> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.schema.clone())
    }
}

fn sample_schema() -> SchemaInfo {
    use agentic_connector::{SchemaColumnInfo, SchemaTableInfo};
    SchemaInfo {
        tables: vec![
            SchemaTableInfo {
                name: "orders".to_string(),
                columns: vec![
                    SchemaColumnInfo {
                        name: "order_id".to_string(),
                        data_type: "INTEGER".to_string(),
                        min: None,
                        max: None,
                        sample_values: vec![],
                    },
                    SchemaColumnInfo {
                        name: "revenue".to_string(),
                        data_type: "DOUBLE".to_string(),
                        min: None,
                        max: None,
                        sample_values: vec![CellValue::Number(100.0)],
                    },
                ],
            },
            SchemaTableInfo {
                name: "customers".to_string(),
                columns: vec![SchemaColumnInfo {
                    name: "name".to_string(),
                    data_type: "VARCHAR".to_string(),
                    min: None,
                    max: None,
                    sample_values: vec![CellValue::Text("Alice".to_string())],
                }],
            },
        ],
        join_keys: vec![],
    }
}

fn make_connectors(stub: Arc<IntrospectableStub>) -> HashMap<String, Arc<dyn DatabaseConnector>> {
    let mut map: HashMap<String, Arc<dyn DatabaseConnector>> = HashMap::new();
    map.insert("default".to_string(), stub);
    map
}

/// Stub with the lazy shape ClickHouse and Postgres have: `introspect_schema`
/// answers "no tables" until `prepare_schema` has run.
struct LazyStub {
    schema: SchemaInfo,
    prepared: std::sync::atomic::AtomicBool,
}

impl LazyStub {
    fn new(schema: SchemaInfo) -> Self {
        Self {
            schema,
            prepared: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl DatabaseConnector for LazyStub {
    fn dialect(&self) -> agentic_connector::SqlDialect {
        agentic_connector::SqlDialect::DuckDb
    }

    async fn execute_query(
        &self,
        _sql: &str,
        _limit: u64,
    ) -> Result<agentic_connector::ExecutionResult, agentic_connector::ConnectorError> {
        panic!("LazyStub: execute_query must not be called")
    }

    async fn prepare_schema(&self) -> Result<(), agentic_connector::ConnectorError> {
        self.prepared
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn introspect_schema(
        &self,
    ) -> Result<agentic_connector::SchemaInfo, agentic_connector::ConnectorError> {
        if self.prepared.load(std::sync::atomic::Ordering::Relaxed) {
            Ok(self.schema.clone())
        } else {
            Ok(SchemaInfo::default())
        }
    }
}

/// Regression: a lazily-connecting connector must be prepared before it is
/// introspected. Without the `prepare_schema().await` in `cached_schema`,
/// `list_tables` answers `{"tables": []}` — the analytics agent is told the
/// warehouse is empty rather than getting an error.
#[tokio::test]
async fn list_tables_prepares_a_lazy_connector_first() {
    let stub: Arc<dyn DatabaseConnector> = Arc::new(LazyStub::new(sample_schema()));
    let mut connectors: HashMap<String, Arc<dyn DatabaseConnector>> = HashMap::new();
    connectors.insert("default".to_string(), stub);
    let cache = new_schema_cache();

    let result = execute_database_lookup_tool(
        "list_tables",
        json!({ "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();

    let tables = result["tables"].as_array().unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"orders"), "got {names:?}");
    assert!(names.contains(&"customers"), "got {names:?}");
}

/// Same guarantee for `describe_table`, which fails loudly (`available: []`)
/// rather than quietly when the schema is missing.
#[tokio::test]
async fn describe_table_prepares_a_lazy_connector_first() {
    let stub: Arc<dyn DatabaseConnector> = Arc::new(LazyStub::new(sample_schema()));
    let mut connectors: HashMap<String, Arc<dyn DatabaseConnector>> = HashMap::new();
    connectors.insert("default".to_string(), stub);
    let cache = new_schema_cache();

    let result = execute_database_lookup_tool(
        "describe_table",
        json!({ "table": "orders" }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();

    assert_eq!(result["table"].as_str(), Some("orders"));
    assert_eq!(result["columns"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn list_tables_returns_table_names() {
    let stub = Arc::new(IntrospectableStub::new(sample_schema()));
    let connectors = make_connectors(stub);
    let cache = new_schema_cache();
    let result = execute_database_lookup_tool(
        "list_tables",
        json!({ "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();
    let tables = result["tables"].as_array().unwrap();
    assert_eq!(tables.len(), 2);
    let names: Vec<&str> = tables.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"orders"));
    assert!(names.contains(&"customers"));
}

#[tokio::test]
async fn describe_table_returns_columns() {
    let stub = Arc::new(IntrospectableStub::new(sample_schema()));
    let connectors = make_connectors(stub);
    let cache = new_schema_cache();
    let result = execute_database_lookup_tool(
        "describe_table",
        json!({ "table": "orders", "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();
    assert_eq!(result["table"], "orders");
    let cols = result["columns"].as_array().unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0]["name"], "order_id");
    assert_eq!(cols[0]["data_type"], "INTEGER");
    assert_eq!(cols[1]["name"], "revenue");
    let samples = cols[1]["sample_values"].as_array().unwrap();
    assert!(!samples.is_empty());
}

#[tokio::test]
async fn describe_table_unknown_returns_error() {
    let stub = Arc::new(IntrospectableStub::new(sample_schema()));
    let connectors = make_connectors(stub);
    let cache = new_schema_cache();
    let err = execute_database_lookup_tool(
        "describe_table",
        json!({ "table": "nonexistent", "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::Execution(_)));
}

#[tokio::test]
async fn list_tables_caches_schema() {
    let stub = Arc::new(IntrospectableStub::new(sample_schema()));
    let connectors = make_connectors(Arc::clone(&stub));
    let cache = new_schema_cache();
    // First call populates cache.
    execute_database_lookup_tool(
        "list_tables",
        json!({ "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();
    assert_eq!(stub.calls(), 1);
    // Second call uses cache.
    execute_database_lookup_tool(
        "list_tables",
        json!({ "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();
    assert_eq!(
        stub.calls(),
        1,
        "introspect_schema should be called only once"
    );
}

#[tokio::test]
async fn describe_table_case_insensitive() {
    let stub = Arc::new(IntrospectableStub::new(sample_schema()));
    let connectors = make_connectors(stub);
    let cache = new_schema_cache();
    let result = execute_database_lookup_tool(
        "describe_table",
        json!({ "table": "ORDERS", "database": null }),
        &connectors,
        "default",
        &cache,
    )
    .await
    .unwrap();
    assert_eq!(result["table"], "orders");
}

// ── Anomaly tools ─────────────────────────────────────────────────────────

#[test]
fn anomaly_tools_are_openai_strict_compatible() {
    use crate::tools::anomaly_tools;
    let tools = anomaly_tools();
    for tool in &tools {
        let violations = validate_openai_strict_schema(&tool.parameters, tool.name);
        assert!(
            violations.is_empty(),
            "tool '{}' violates OpenAI strict mode:\n  {}",
            tool.name,
            violations.join("\n  ")
        );
    }
}

#[test]
fn anomaly_tools_returns_three_tools() {
    use crate::tools::anomaly_tools;
    assert_eq!(anomaly_tools().len(), 3);
    let names: Vec<&str> = anomaly_tools().iter().map(|t| t.name).collect();
    assert!(names.contains(&"list_anomalies"));
    assert!(names.contains(&"detect_anomalies"));
    assert!(names.contains(&"explain_anomaly"));
}

#[test]
fn anomaly_tools_not_in_solving_tools() {
    let solving = solving_tools();
    let names: Vec<&str> = solving.iter().map(|t| t.name).collect();
    assert!(!names.contains(&"list_anomalies"));
    assert!(!names.contains(&"detect_anomalies"));
    assert!(!names.contains(&"explain_anomaly"));
}

#[tokio::test]
async fn list_anomalies_returns_empty_for_null_store() {
    use crate::anomaly_store::{
        AnomalyFilter, AnomalyRecord, AnomalyStoreError, DetectAndUpsertResult,
    };
    use crate::metric_tree_runner::MetricTreeRunnerError;
    use crate::tools::{AnomalyToolContext, execute_anomaly_tool};
    use oxy_airlayer_compat::DatabaseConfig;
    use oxy_airlayer_compat::SemanticLayer;
    use oxy_airlayer_compat::engine::metric_tree_ops::{
        ExplainConfig, ExplainResult, OpportunityResult,
    };

    struct NullStore;
    #[async_trait::async_trait]
    impl crate::anomaly_store::AnomalyStore for NullStore {
        async fn list(
            &self,
            _: uuid::Uuid,
            _: AnomalyFilter,
        ) -> Result<Vec<AnomalyRecord>, AnomalyStoreError> {
            Ok(vec![])
        }
        async fn get(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
        ) -> Result<Option<AnomalyRecord>, AnomalyStoreError> {
            Ok(None)
        }
        async fn detect_and_upsert(
            &self,
            _: uuid::Uuid,
            _: &str,
            _: &str,
            _: &str,
            _: Vec<(String, f64)>,
        ) -> Result<DetectAndUpsertResult, AnomalyStoreError> {
            Ok(DetectAndUpsertResult {
                anomalies: vec![],
                total_observations: 0,
                message: None,
            })
        }
        async fn get_explain_cache(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
        ) -> Result<Option<serde_json::Value>, AnomalyStoreError> {
            Ok(None)
        }
        async fn set_explain_cache(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
            _: serde_json::Value,
        ) -> Result<(), AnomalyStoreError> {
            Ok(())
        }
    }

    struct NullRunner;
    #[async_trait::async_trait]
    impl crate::metric_tree_runner::MetricTreeRunner for NullRunner {
        async fn load_layer(&self) -> Result<SemanticLayer, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::LayerLoad("test stub".into()))
        }
        async fn list_databases(&self) -> Vec<DatabaseConfig> {
            vec![]
        }
        async fn run_explain(
            &self,
            _: String,
            _: String,
            _: (String, String),
            _: (String, String),
            _: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
            _: ExplainConfig,
        ) -> Result<ExplainResult, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::Op("test stub".into()))
        }
        async fn run_opportunity(
            &self,
            _: String,
            _: String,
            _: (String, String),
            _statistic: oxy_airlayer_compat::engine::metric_tree_ops::BenchmarkStatistic,
        ) -> Result<OpportunityResult, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::Op("test stub".into()))
        }
        async fn get_dimension_values(
            &self,
            _: String,
            _: String,
            _: u32,
        ) -> Result<Vec<String>, MetricTreeRunnerError> {
            Ok(vec![])
        }
        async fn run_time_series(
            &self,
            _: String,
            _: String,
            _: String,
            _: (String, String),
            _: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
            _: Option<String>,
        ) -> Result<Vec<(String, f64)>, MetricTreeRunnerError> {
            Ok(vec![])
        }
    }

    let ctx = AnomalyToolContext {
        workspace_id: uuid::Uuid::nil(),
        store: &NullStore,
        runner: &NullRunner,
    };

    let result = execute_anomaly_tool(
        "list_anomalies",
        serde_json::json!({
            "measure": "revenue",
            "time_dimension": "created_at",
            "granularity": "day",
            "period_start": "2024-01-01T00:00:00Z",
            "period_end": "2024-01-31T00:00:00Z"
        }),
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(result["count"], 0);
    assert!(result["anomalies"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn detect_anomalies_returns_message_when_insufficient_data() {
    use crate::anomaly_store::{
        AnomalyFilter, AnomalyRecord, AnomalyStoreError, DetectAndUpsertResult,
    };
    use crate::metric_tree_runner::MetricTreeRunnerError;
    use crate::tools::{AnomalyToolContext, execute_anomaly_tool};
    use oxy_airlayer_compat::DatabaseConfig;
    use oxy_airlayer_compat::SemanticLayer;
    use oxy_airlayer_compat::engine::metric_tree_ops::{
        ExplainConfig, ExplainResult, OpportunityResult,
    };

    struct ThinStore;
    #[async_trait::async_trait]
    impl crate::anomaly_store::AnomalyStore for ThinStore {
        async fn list(
            &self,
            _: uuid::Uuid,
            _: AnomalyFilter,
        ) -> Result<Vec<AnomalyRecord>, AnomalyStoreError> {
            Ok(vec![])
        }
        async fn get(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
        ) -> Result<Option<AnomalyRecord>, AnomalyStoreError> {
            Ok(None)
        }
        async fn detect_and_upsert(
            &self,
            _: uuid::Uuid,
            _: &str,
            _: &str,
            _: &str,
            _: Vec<(String, f64)>,
        ) -> Result<DetectAndUpsertResult, AnomalyStoreError> {
            // Simulate "not enough data" — return empty with a message.
            Ok(DetectAndUpsertResult {
                anomalies: vec![],
                total_observations: 3,
                message: Some("Not enough data: 3 observations, need at least 8".into()),
            })
        }
        async fn get_explain_cache(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
        ) -> Result<Option<serde_json::Value>, AnomalyStoreError> {
            Ok(None)
        }
        async fn set_explain_cache(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
            _: serde_json::Value,
        ) -> Result<(), AnomalyStoreError> {
            Ok(())
        }
    }

    struct ThinRunner;
    #[async_trait::async_trait]
    impl crate::metric_tree_runner::MetricTreeRunner for ThinRunner {
        async fn load_layer(&self) -> Result<SemanticLayer, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::LayerLoad("stub".into()))
        }
        async fn list_databases(&self) -> Vec<DatabaseConfig> {
            vec![]
        }
        async fn run_explain(
            &self,
            _: String,
            _: String,
            _: (String, String),
            _: (String, String),
            _: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
            _: ExplainConfig,
        ) -> Result<ExplainResult, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::Op("stub".into()))
        }
        async fn run_opportunity(
            &self,
            _: String,
            _: String,
            _: (String, String),
            _statistic: oxy_airlayer_compat::engine::metric_tree_ops::BenchmarkStatistic,
        ) -> Result<OpportunityResult, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::Op("stub".into()))
        }
        async fn get_dimension_values(
            &self,
            _: String,
            _: String,
            _: u32,
        ) -> Result<Vec<String>, MetricTreeRunnerError> {
            Ok(vec![])
        }
        async fn run_time_series(
            &self,
            _: String,
            _: String,
            _: String,
            _: (String, String),
            _: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
            _: Option<String>,
        ) -> Result<Vec<(String, f64)>, MetricTreeRunnerError> {
            // Only 3 data points — not enough for weekly seasonality (needs ≥ 8).
            Ok(vec![
                ("2024-01-01T00:00:00Z".into(), 100.0),
                ("2024-01-02T00:00:00Z".into(), 110.0),
                ("2024-01-03T00:00:00Z".into(), 90.0),
            ])
        }
    }

    let ctx = AnomalyToolContext {
        workspace_id: uuid::Uuid::nil(),
        store: &ThinStore,
        runner: &ThinRunner,
    };

    let result = execute_anomaly_tool(
        "detect_anomalies",
        serde_json::json!({
            "measure": "revenue",
            "time_dimension": "created_at",
            "granularity": "day",
            "period_start": "2024-01-01",
            "period_end": "2024-01-31"
        }),
        &ctx,
    )
    .await
    .unwrap();

    assert_eq!(result["count"], 0);
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("Not enough data")
    );
}

// ── Datasource routing on the model-callable probes ──────────────────────────
//
// `sample_columns` and `check_data_freshness` read a view's `datasource:` and
// query it directly. Rerouting them is invisible in a way the semantic path is
// not: sampled values become the literals the model writes into `WHERE ... IN
// (...)`, and a watermark becomes a "data through <date>" the model states as
// fact. Both must honour the declared datasource or fail naming it — the policy
// is one deleted `.filter(...)` away from regressing.

fn catalog_with_datasource(datasource: &str) -> crate::SemanticCatalog {
    let yaml = format!(
        r#"
name: referrals
description: Referrals
table: july.referrals
datasource: {datasource}
dimensions:
  - name: state
    type: string
    expr: state
  - name: referral_date
    type: date
    expr: referral_date
"#
    );
    let views = vec![crate::airlayer_compat::parse_view_yaml(&yaml).unwrap()];
    let layer = oxy_airlayer_compat::SemanticLayer::new(views, None);
    let dialects = oxy_airlayer_compat::DatasourceDialectMap::with_default(
        oxy_airlayer_compat::Dialect::DuckDB,
    );
    let engine = oxy_airlayer_compat::SemanticEngine::from_semantic_layer(layer, dialects).unwrap();
    crate::SemanticCatalog::from_engine(engine)
}

/// Registers `name` against a connector that PANICS if queried.
///
/// The panic is the assertion for the refuse-to-reroute tests: if the
/// `.filter(...)` ever comes back, the probe silently retargets this connector
/// and executes, and the test fails loudly at the exact wrong behaviour rather
/// than on a string mismatch.
fn one_connector(name: &str) -> HashMap<String, Arc<dyn DatabaseConnector>> {
    HashMap::from([(
        name.to_string(),
        Arc::new(NoopConnector) as Arc<dyn DatabaseConnector>,
    )])
}

/// A connector that answers every query with no rows.
struct EmptyConnector;

#[async_trait::async_trait]
impl DatabaseConnector for EmptyConnector {
    fn dialect(&self) -> agentic_connector::SqlDialect {
        agentic_connector::SqlDialect::DuckDb
    }

    async fn execute_query(
        &self,
        _sql: &str,
        _limit: u64,
    ) -> Result<agentic_connector::ExecutionResult, agentic_connector::ConnectorError> {
        Ok(agentic_connector::ExecutionResult::empty())
    }
}

fn one_queryable_connector(name: &str) -> HashMap<String, Arc<dyn DatabaseConnector>> {
    HashMap::from([(
        name.to_string(),
        Arc::new(EmptyConnector) as Arc<dyn DatabaseConnector>,
    )])
}

#[tokio::test]
async fn sample_columns_refuses_a_view_whose_datasource_is_not_registered() {
    let catalog = catalog_with_datasource("july_airhouse");
    let connectors = one_connector("july_warehouse");

    let result = execute_specifying_tool(
        "sample_columns",
        json!({"columns": [{"table": "referrals", "column": "state"}]}),
        &catalog,
        &connectors,
        "july_warehouse",
    )
    .await
    .expect("the batch wrapper reports per-column errors inline");

    let text = result.to_string();
    assert!(
        text.contains("july_airhouse"),
        "the error must name the datasource the view declared, not the default it \
         would otherwise have been sampled from: {text}"
    );
}

#[tokio::test]
async fn check_data_freshness_refuses_a_view_whose_datasource_is_not_registered() {
    let catalog = catalog_with_datasource("july_airhouse");
    let connectors = one_connector("july_warehouse");

    let result = execute_specifying_tool(
        "check_data_freshness",
        json!({"views": ["referrals"]}),
        &catalog,
        &connectors,
        "july_warehouse",
    )
    .await
    .expect("the batch wrapper reports per-view errors inline");

    let text = result.to_string();
    // No `|| "no freshness target"` escape hatch. With that alternative the
    // test passed on a view with no date dimension -- failing before the
    // connector was ever looked up, and pinning nothing. The view now has one,
    // so reaching the routing decision is part of what this asserts.
    assert!(
        text.contains("july_airhouse"),
        "the error must name the datasource the view declared, not fail earlier \
         or probe the default: {text}"
    );
}

#[tokio::test]
async fn a_datasource_differing_only_in_case_still_resolves() {
    // Refusing to reroute is the policy; refusing on letter case alone is a
    // false positive. airlayer matches views with `eq_ignore_ascii_case`, so a
    // name that resolves there must resolve here.
    let catalog = catalog_with_datasource("July_Airhouse");
    let connectors = one_queryable_connector("july_airhouse");

    let result = execute_specifying_tool(
        "sample_columns",
        json!({"columns": [{"table": "referrals", "column": "state"}]}),
        &catalog,
        &connectors,
        "july_airhouse",
    )
    .await
    .expect("sample_columns reports per-column errors inline");

    let text = result.to_string();
    assert!(
        !text.contains("no connector registered"),
        "a case-differing datasource is the same datasource: {text}"
    );
}
