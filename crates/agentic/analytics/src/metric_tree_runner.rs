//! Cross-crate hook for executing metric-tree queries against a real warehouse.
//!
//! The metric-tree ops in `oxy_airlayer_compat::engine::metric_tree_ops` (`explain`,
//! `opportunity`) compile airlayer `QueryRequest`s into SQL and need to run
//! that SQL against an actual database. The analytics domain doesn't know
//! how to reach Oxy's connector pool — that wiring lives in `oxy-app`
//! (`OxyProjectContext::build_connector_for`). Mirror the `SubrunRunner`
//! pattern: define a trait here, inject a concrete impl from
//! `agentic-pipeline` / `oxy-app`.
//!
//! Construction is async because building a connector talks to the
//! configured warehouse (auth, dialect resolution); execution returns the
//! synchronous `QueryExecutor` closure that airlayer's ops expect.

use oxy_airlayer_compat::DatabaseConfig;
use oxy_airlayer_compat::SemanticLayer;
use oxy_airlayer_compat::engine::EngineError;
use oxy_airlayer_compat::engine::metric_tree::MetricTree;
use oxy_airlayer_compat::engine::metric_tree_ops::BenchmarkStatistic;
use oxy_airlayer_compat::engine::metric_tree_ops::{
    ExplainConfig, ExplainResult, OpportunityResult,
};
use oxy_airlayer_compat::engine::query::QueryFilter;
use std::sync::Arc;

/// Run airlayer metric-tree ops against a real warehouse.
///
/// The query-executing ops (`explain`, `opportunity`) need both:
/// - a synchronous `QueryExecutor` closure that talks to the connector pool, and
/// - a `spawn_blocking` wrapper so the airlayer algorithm (which is sync and
///   issues 100+ queries per call) doesn't block the runtime thread.
///
/// airlayer's `QueryExecutor` type is `dyn Fn(...)`, not `Send`, so the
/// executor must be constructed *inside* `spawn_blocking`. The trait therefore
/// exposes `run_explain` / `run_opportunity` directly rather than handing the
/// executor across an `.await` point. Pure ops (`sensitivity`, `predict`)
/// operate on the in-memory tree and only need `load_layer`.
///
/// Concrete impl: `OxyMetricTreeRunner` in `crates/app/src/agentic_wiring/`.
/// The benchmark statistic used when a caller names none.
///
/// `P75` because it is the closest single value to the adaptive rule airlayer
/// removed — it is the branch that rule took for any dimension large enough
/// for a percentile to be stable — and because oxy's own fixtures and UI
/// predominantly show `p75`. A caller wanting the old `best_peer` behaviour on
/// a thin dimension now asks for it explicitly, which is the point: the choice
/// is visible in the request rather than frozen in a constant.
pub const DEFAULT_BENCHMARK_STATISTIC: BenchmarkStatistic = BenchmarkStatistic::P75;

#[async_trait::async_trait]
pub trait MetricTreeRunner: Send + Sync {
    /// Load the workspace's semantic model (the same scan path used by the
    /// analytics solver's catalog).
    async fn load_layer(&self) -> Result<SemanticLayer, MetricTreeRunnerError>;

    /// List configured warehouse databases (used by airlayer to resolve dialects).
    async fn list_databases(&self) -> Vec<DatabaseConfig>;

    /// Period-over-period root cause analysis. The implementor builds the
    /// `QueryExecutor` and runs `oxy_airlayer_compat::engine::metric_tree_ops::explain` inside `spawn_blocking`.
    ///
    /// `filters` are base equals-filters applied to every query the explain
    /// issues — used to scope a per-segment anomaly (e.g. one restaurant) so
    /// the totals and decomposition reflect that segment rather than the
    /// chain-wide aggregate. Pass an empty vec for an unscoped explain.
    async fn run_explain(
        &self,
        target: String,
        time_dimension: String,
        current_period: (String, String),
        previous_period: (String, String),
        filters: Vec<QueryFilter>,
        config: ExplainConfig,
    ) -> Result<ExplainResult, MetricTreeRunnerError>;

    /// Segment opportunity sizing. Same `spawn_blocking` contract as
    /// [`Self::run_explain`].
    ///
    /// `statistic` is the caller's, not the engine's. airlayer used to choose
    /// it adaptively — the best-performing segment for a dimension with few
    /// segments, the 75th percentile once there were enough for a percentile to
    /// mean anything — and replaced that with an explicit argument. No fixed
    /// value reproduces the old rule, so rather than freeze one here the choice
    /// is passed down, defaulting to [`DEFAULT_BENCHMARK_STATISTIC`].
    async fn run_opportunity(
        &self,
        target: String,
        time_dimension: String,
        period: (String, String),
        statistic: BenchmarkStatistic,
    ) -> Result<OpportunityResult, MetricTreeRunnerError>;

    /// Return the distinct non-null values of `dimension` observed for
    /// `measure` in the last `since_days` days. Used by the anomaly scanner
    /// to fan out a `group_by` monitor without enumerating segments in YAML.
    async fn get_dimension_values(
        &self,
        dimension: String,
        measure: String,
        since_days: u32,
    ) -> Result<Vec<String>, MetricTreeRunnerError>;

    /// Fetch a single measure's value broken down by a time dimension at the
    /// given granularity. Used by anomaly monitoring to pull the input series
    /// for the detector. Returns `(timestamp_string, value)` rows in
    /// time-ascending order.
    ///
    /// `granularity` is the airlayer string: `"day"` | `"week"` | `"month"` |
    /// `"quarter"` | `"year"`.
    ///
    /// `filters` are additional `equals` dimension filters applied before
    /// aggregation — used for per-store / per-segment monitors.
    ///
    /// `timezone` is an IANA name used to bucket the series in local time.
    /// `None` means "use the runner's configured default" (which is UTC for
    /// runners that have none) — NOT an explicit UTC request.
    ///
    /// **Contract:** implementations return only buckets within the requested
    /// `period`, inclusive on both ends, and are responsible for compensating
    /// for any timezone-conversion edge effects of their own query engine.
    /// (Concretely: airlayer applies a `date_range` filter to the *raw*,
    /// unconverted column while bucketing on the timezone-*converted* one, so
    /// a non-UTC request clips or partially-sums its first and last buckets
    /// unless the implementor pads the underlying query and trims the result
    /// back down.) This is what lets every caller — the scheduled scanner and
    /// the chat-driven `detect_anomalies` tool alike — pass `period` as-is
    /// without knowing about the quirk or duplicating a workaround.
    async fn run_time_series(
        &self,
        measure: String,
        time_dimension: String,
        granularity: String,
        period: (String, String),
        filters: Vec<QueryFilter>,
        timezone: Option<String>,
    ) -> Result<Vec<(String, f64)>, MetricTreeRunnerError>;

    /// Execute a full airlayer query (window already injected by the caller as a
    /// `time_dimensions` date range) and reduce it to ONE scalar — the value of
    /// the request's single measure in the single aggregated row. Correct for
    /// any measure type (sum, average, count-distinct). Used by external-source
    /// reconciliation. Defaults to unsupported so existing runners need no change.
    async fn run_query_scalar(
        &self,
        request: oxy_airlayer_compat::engine::query::QueryRequest,
    ) -> Result<f64, MetricTreeRunnerError> {
        let _ = request;
        Err(MetricTreeRunnerError::Op(
            "run_query_scalar not supported by this runner".to_string(),
        ))
    }
}

/// Failure modes for runner construction. Op-level errors come back as
/// `oxy_airlayer_compat::engine::EngineError` from the executor itself.
#[derive(Debug)]
pub enum MetricTreeRunnerError {
    LayerLoad(String),
    ExecutorBuild(String),
    Op(String),
}

impl std::fmt::Display for MetricTreeRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LayerLoad(e) => write!(f, "failed to load semantic model: {e}"),
            Self::ExecutorBuild(e) => write!(f, "failed to construct query executor: {e}"),
            Self::Op(e) => write!(f, "metric-tree op failed: {e}"),
        }
    }
}

impl std::error::Error for MetricTreeRunnerError {}

impl From<MetricTreeRunnerError> for EngineError {
    fn from(value: MetricTreeRunnerError) -> Self {
        EngineError::QueryError(value.to_string())
    }
}

/// Convenience: build a `MetricTree` for the runner's current semantic model.
/// Wrapped here so callers don't have to depend on `oxy_semantic` directly.
pub async fn load_tree(
    runner: &Arc<dyn MetricTreeRunner>,
) -> Result<(SemanticLayer, MetricTree), MetricTreeRunnerError> {
    let layer = runner.load_layer().await?;
    let tree = oxy_airlayer_compat::engine::metric_tree::MetricTree::build(&layer);
    Ok((layer, tree))
}
