//! Async executor for the four metric-tree tools.
//!
//! The actual algorithms live in `oxy_airlayer_compat::engine::metric_tree_ops`. This
//! module is the bridge: parse JSON parameters, load the layer + tree via the
//! injected [`MetricTreeRunner`], build the synchronous `QueryExecutor`,
//! and run the op inside `spawn_blocking` (the airlayer op is sync and may
//! issue 100+ warehouse queries — never block the runtime thread).
//!
//! Returns shape:
//! - `explain_metric` → serialized [`oxy_airlayer_compat::engine::metric_tree_ops::ExplainResult`]
//! - `find_opportunities` → [`OpportunityResult`]
//! - `metric_sensitivity` → [`SensitivityResult`]
//! - `predict_impact` → [`PredictResult`]
//!
//! Errors map to `ToolError::Execution` with the airlayer message,
//! so the LLM sees a single sentence rather than a multi-line panic.

use std::sync::Arc;

use agentic_core::tools::ToolError;
use oxy_airlayer_compat::engine::metric_tree_ops::{self, ExplainConfig};
use serde_json::{Value, json};

use crate::metric_tree_runner::MetricTreeRunner;

use super::{emit_tool_error, emit_tool_input, emit_tool_output};

/// Dispatch a metric-tree tool call. Tool name must be one of
/// [`super::METRIC_TREE_TOOL_NAMES`].
#[tracing::instrument(
    skip(runner, params),
    fields(oxy.name = "analytics.tool", oxy.span_type = "analytics", tool = %name)
)]
pub async fn execute_metric_tree_tool(
    name: &str,
    params: Value,
    runner: Arc<dyn MetricTreeRunner>,
) -> Result<Value, ToolError> {
    emit_tool_input(name, &params);
    let result = execute_inner(name, params, runner).await;
    match &result {
        Ok(v) => emit_tool_output(v),
        Err(e) => emit_tool_error(e),
    }
    result
}

async fn execute_inner(
    name: &str,
    params: Value,
    runner: Arc<dyn MetricTreeRunner>,
) -> Result<Value, ToolError> {
    match name {
        "metric_sensitivity" => run_sensitivity(params, runner).await,
        "predict_impact" => run_predict(params, runner).await,
        "explain_metric" => run_explain(params, runner).await,
        "find_opportunities" => run_opportunity(params, runner).await,
        _ => Err(ToolError::UnknownTool(name.into())),
    }
}

// ── metric_sensitivity ──────────────────────────────────────────────────────

async fn run_sensitivity(
    params: Value,
    runner: Arc<dyn MetricTreeRunner>,
) -> Result<Value, ToolError> {
    let target = required_str(&params, "target")?;
    let layer = runner
        .load_layer()
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    let tree = oxy_airlayer_compat::engine::metric_tree::MetricTree::build(&layer);
    let result = metric_tree_ops::sensitivity(&tree, target)
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    serde_json::to_value(result).map_err(|e| ToolError::Execution(e.to_string()))
}

// ── predict_impact ──────────────────────────────────────────────────────────

async fn run_predict(params: Value, runner: Arc<dyn MetricTreeRunner>) -> Result<Value, ToolError> {
    let changes_value = params
        .get("changes")
        .ok_or_else(|| ToolError::BadParams("missing 'changes' array".into()))?;
    let changes_array = changes_value
        .as_array()
        .ok_or_else(|| ToolError::BadParams("'changes' must be an array".into()))?;
    let changes: Result<Vec<(String, f64)>, ToolError> = changes_array
        .iter()
        .map(|entry| {
            let measure = entry
                .get("measure")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::BadParams("change.measure must be a string".into()))?
                .to_string();
            let delta = entry
                .get("delta")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| ToolError::BadParams("change.delta must be a number".into()))?;
            Ok((measure, delta))
        })
        .collect();
    let changes = changes?;

    let layer = runner
        .load_layer()
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    let tree = oxy_airlayer_compat::engine::metric_tree::MetricTree::build(&layer);
    // The lever-conflict refusal has to reach here and not only the two HTTP
    // `/predict` handlers: an LLM picks these levers off the tree it was just
    // shown, so pinning a measure and something upstream of it is a natural
    // move rather than an operator error — and a confident `PredictResult` for
    // an ambiguous pinned set is the worst possible answer to it. Shared
    // definition, not a copy: see `oxy_airlayer_compat::lever_conflicts`.
    oxy_airlayer_compat::reject_lever_conflicts(&tree, &changes).map_err(ToolError::BadParams)?;
    let result = metric_tree_ops::predict(&tree, &changes)
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    serde_json::to_value(result).map_err(|e| ToolError::Execution(e.to_string()))
}

// ── explain_metric ──────────────────────────────────────────────────────────

async fn run_explain(params: Value, runner: Arc<dyn MetricTreeRunner>) -> Result<Value, ToolError> {
    let target = required_str(&params, "target")?.to_string();
    let time_dimension = required_str(&params, "time_dimension")?.to_string();
    let cur_start = required_str(&params, "current_period_start")?.to_string();
    let cur_end = required_str(&params, "current_period_end")?.to_string();
    let prev_start = required_str(&params, "previous_period_start")?.to_string();
    let prev_end = required_str(&params, "previous_period_end")?.to_string();
    let deep = params
        .get("deep")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut config = ExplainConfig::default();
    config.deep = deep;

    let result = runner
        .run_explain(
            target,
            time_dimension,
            (cur_start, cur_end),
            (prev_start, prev_end),
            vec![],
            config,
        )
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    serde_json::to_value(result).map_err(|e| ToolError::Execution(e.to_string()))
}

// ── find_opportunities ──────────────────────────────────────────────────────

async fn run_opportunity(
    params: Value,
    runner: Arc<dyn MetricTreeRunner>,
) -> Result<Value, ToolError> {
    let target = required_str(&params, "target")?.to_string();
    let time_dimension = required_str(&params, "time_dimension")?.to_string();
    let period_start = required_str(&params, "period_start")?.to_string();
    let period_end = required_str(&params, "period_end")?.to_string();

    let result = runner
        .run_opportunity(
            target,
            time_dimension,
            (period_start, period_end),
            crate::metric_tree_runner::DEFAULT_BENCHMARK_STATISTIC,
        )
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;

    serde_json::to_value(result).map_err(|e| ToolError::Execution(e.to_string()))
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn required_str<'a>(params: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::BadParams(format!("missing '{key}' string")))
}

/// Stub result returned by a metric-tree tool when the workspace has no
/// runner wired up. Tools should not be exposed in that case, but the
/// safety net keeps the agent honest if the LLM hallucinates a call.
pub fn no_runner_error() -> ToolError {
    ToolError::Execution(
        "metric-tree tools are not available: no metric_tree_runner is \
         configured for this workspace"
            .into(),
    )
}

/// JSON payload returned when a metric-tree call surfaces a non-error result
/// that the LLM should still treat as a soft failure (empty tree, no drivers,
/// etc.). Kept here so handlers stay terse.
#[allow(dead_code)]
pub fn empty_result(reason: &str) -> Value {
    json!({ "ok": false, "reason": reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric_tree_runner::MetricTreeRunnerError;
    use oxy_airlayer_compat::DatabaseConfig;
    use oxy_airlayer_compat::SemanticLayer;
    use oxy_airlayer_compat::engine::metric_tree_ops::{ExplainResult, OpportunityResult};
    use oxy_airlayer_compat::engine::query::QueryFilter;

    /// A runner that only knows how to hand back a layer. The pure ops
    /// (`sensitivity`, `predict`) never touch a warehouse, so every
    /// query-executing method is unreachable from these tests.
    struct LayerOnlyRunner(SemanticLayer);

    #[async_trait::async_trait]
    impl MetricTreeRunner for LayerOnlyRunner {
        async fn load_layer(&self) -> Result<SemanticLayer, MetricTreeRunnerError> {
            Ok(self.0.clone())
        }
        async fn list_databases(&self) -> Vec<DatabaseConfig> {
            vec![]
        }
        async fn run_explain(
            &self,
            _target: String,
            _time_dimension: String,
            _current_period: (String, String),
            _previous_period: (String, String),
            _filters: Vec<QueryFilter>,
            _config: ExplainConfig,
        ) -> Result<ExplainResult, MetricTreeRunnerError> {
            unreachable!("predict is a pure op")
        }
        async fn run_opportunity(
            &self,
            _target: String,
            _time_dimension: String,
            _period: (String, String),
            _statistic: oxy_airlayer_compat::engine::metric_tree_ops::BenchmarkStatistic,
        ) -> Result<OpportunityResult, MetricTreeRunnerError> {
            unreachable!("predict is a pure op")
        }
        async fn get_dimension_values(
            &self,
            _dimension: String,
            _measure: String,
            _since_days: u32,
        ) -> Result<Vec<String>, MetricTreeRunnerError> {
            unreachable!("predict is a pure op")
        }
        async fn run_time_series(
            &self,
            _measure: String,
            _time_dimension: String,
            _granularity: String,
            _period: (String, String),
            _filters: Vec<QueryFilter>,
            _timezone: Option<String>,
        ) -> Result<Vec<(String, f64)>, MetricTreeRunnerError> {
            unreachable!("predict is a pure op")
        }
    }

    fn runner_with_revenue_over_cost() -> Arc<dyn MetricTreeRunner> {
        let view = oxy_airlayer_compat::parse_view_yaml(
            r#"
name: orders
table: public.orders
dialect: postgres
measures:
  - name: revenue
    type: sum
    expr: amount
  - name: cost
    type: sum
    expr: cost
  - name: profit
    type: number
    expr: "{{orders.revenue}} - {{orders.cost}}"
"#,
        )
        .expect("view parses");
        Arc::new(LayerOnlyRunner(SemanticLayer::new(vec![view], None)))
    }

    /// The refusal has to reach THIS caller, not only the two HTTP handlers.
    /// An LLM picks these levers off the tree it was just shown, so pinning a
    /// measure and something upstream of it is a natural move here rather than
    /// an operator error — and a confident `PredictResult` for an ambiguous set
    /// is the worst possible answer to it.
    #[tokio::test]
    async fn predict_impact_refuses_a_driver_and_its_target_pinned_together() {
        let params = json!({
            "changes": [
                {"measure": "orders.revenue", "delta": 100.0},
                {"measure": "orders.profit", "delta": 50.0},
            ]
        });
        let err =
            execute_metric_tree_tool("predict_impact", params, runner_with_revenue_over_cost())
                .await
                .expect_err("revenue is upstream of profit, so the scenario is ambiguous");
        let message = err.to_string();
        assert!(
            message.contains("ambiguous scenario"),
            "expected the lever-conflict refusal, got: {message}"
        );
    }

    /// The mirror case: independent levers still propagate, so the refusal is
    /// not simply rejecting every multi-lever request.
    #[tokio::test]
    async fn predict_impact_allows_independent_levers() {
        let params = json!({
            "changes": [
                {"measure": "orders.revenue", "delta": 100.0},
                {"measure": "orders.cost", "delta": 50.0},
            ]
        });
        execute_metric_tree_tool("predict_impact", params, runner_with_revenue_over_cost())
            .await
            .expect("neither lever is reachable from the other");
    }
}
