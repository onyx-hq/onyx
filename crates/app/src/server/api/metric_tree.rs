//! Metric Tree API — tree structure and analysis ops over the semantic model.
//!
//! The tree is built per request from the workspace's semantic model, resolved
//! through the same scan source `execute_semantic_query` uses —
//! [`resolve_query_scan_source`]: the compile boundary first, the working copy
//! second. Every route here is `FleetOk`, so reading the working copy directly
//! is not an option: a stateless serve replica has none, and scanning the
//! missing directory failed every call with a flat 500 (oxy-hq/oxygen#878).
//!
//! The pure ops — tree, `sensitivity`, `predict` — need no database access.
//! The query-executing ops (`explain`, `opportunity`) run airlayer's
//! algorithms against a `QueryExecutor` bridged to Oxy's connector: airlayer
//! compiles each `QueryRequest` to SQL, and `run_via_agentic_connector`
//! executes it.

use std::collections::BTreeMap;

use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) use super::metric_tree_groups::{SkipKind, SkippedGroup};
use entity::workspace_members::WorkspaceRole;
use oxy::adapters::workspace::manager::WorkspaceManager;
use oxy_airlayer_compat::engine::metric_tree::MetricTree;
use oxy_airlayer_compat::engine::metric_tree_fit::{FittedDriver, apply_fitted_coefficients};
use oxy_airlayer_compat::engine::metric_tree_ops::{
    BaselineOutcome, BenchmarkStatistic, ExplainConfig, ExplainResult, OpportunityResult,
};
use oxy_auth::extractor::AuthenticatedUserExtractor;

use crate::agentic_wiring::metric_tree_runner::{
    OxyMetricTreeRunner, build_drill_query_executor, build_query_executor,
};
use crate::server::api::middlewares::workspace_context::{
    EffectiveWorkspaceRole, PreaggCacheCtx, SemanticEngineCacheCtx, WorkspaceManagerReadOnly,
};
use crate::server::api::semantic::{QueryScanSource, resolve_query_scan_source};

/// Minimum distinct entities required behind a segment for `opportunity`
/// (and `opportunity_drill`) to consider it, per upstream's `n < min_support`
/// exclusion. Pinned to `1` — which excludes only segments with zero
/// distinct entities behind them — rather than adopting upstream's new
/// default of `2`: the pre-bump code had no support floor at all, and a
/// dependency-version pin is not the place to introduce new refusals. If a
/// real support-floor product decision is made later, change it here once.
pub const OPPORTUNITY_MIN_SUPPORT: usize = 1;

/// Serde hook for the optional `statistic` request field. The default lives
/// in `agentic-analytics` beside the trait, so the HTTP surface and the agent
/// tool cannot drift apart on what "unspecified" means.
fn default_benchmark_statistic() -> BenchmarkStatistic {
    agentic_analytics::metric_tree_runner::DEFAULT_BENCHMARK_STATISTIC
}

#[derive(Debug)]
pub enum MetricTreeError {
    LayerLoad(String),
    /// No semantic model is reachable on this node: the workspace has no
    /// compiled revision and this replica has no working copy to fall back to.
    /// Retryable — a compile is enqueued on the way out — so it must not be
    /// flattened into the generic 500 above.
    ScanUnavailable(String),
    NotFound(String),
    /// The request itself is out of range — a caller error, not a failure.
    /// Distinct from [`MetricTreeError::Op`] because that one is logged as a
    /// server error and answered with a generic message, which is exactly
    /// wrong for something the caller can fix by changing a number.
    BadRequest(String),
    Op(String),
}

impl IntoResponse for MetricTreeError {
    fn into_response(self) -> Response {
        let mut retry_after = false;
        let (status, msg) = match &self {
            MetricTreeError::LayerLoad(e) => {
                tracing::error!("metric-tree layer load failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to load semantic model".to_string(),
                )
            }
            MetricTreeError::ScanUnavailable(m) => {
                // Logged, not silent: the "no compiled revision at all" branch
                // leaves no breadcrumb anywhere else (`resolve_query_scan_source`
                // only warns when a materialise actually failed), so a workspace
                // stuck un-compiled would be visible to the client and to nobody
                // operating the fleet.
                tracing::warn!("metric-tree scan unavailable: {m}");
                retry_after = true;
                (StatusCode::SERVICE_UNAVAILABLE, m.clone())
            }
            MetricTreeError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            MetricTreeError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            MetricTreeError::Op(e) => {
                tracing::error!("metric-tree op failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Metric tree operation failed".to_string(),
                )
            }
        };
        if retry_after {
            // A compile is enqueued on the way out; give the client a concrete
            // interval instead of leaving the retry cadence to guesswork.
            return (status, [(axum::http::header::RETRY_AFTER, "5")], msg).into_response();
        }
        (status, msg).into_response()
    }
}

/// Resolve the scan root for this request: compile boundary first, working copy
/// second — the same resolution `execute_semantic_query` performs.
///
/// The returned [`QueryScanSource`] owns the materialised tempdir, so callers
/// must keep it alive until every read of `scan_path` has finished. Every such
/// read happens in the async half of a handler — parsing the layer, or a
/// runner's `snapshot_for_blocking` — deliberately: a `spawn_blocking` task
/// outlives handler cancellation (client disconnect, `EXPLAIN_TIMEOUT`), which
/// drops the guard and deletes the directory underneath it. Keep it that way;
/// the invariant "hold the guard across the blocking task" is not one the
/// handler can actually enforce.
pub(crate) async fn resolve_scan<S: oxy::config::DiskSlot>(
    workspace_manager: &WorkspaceManager<S>,
) -> Result<QueryScanSource, MetricTreeError> {
    resolve_query_scan_source(workspace_manager)
        .await
        .map_err(|e| MetricTreeError::ScanUnavailable(e.message()))
}

/// Parse the workspace's semantic model from an already-resolved scan root.
pub(crate) fn load_layer_at(
    scan_path: &std::path::Path,
) -> Result<oxy_airlayer_compat::SemanticLayer, MetricTreeError> {
    OxyMetricTreeRunner::load_layer_at(scan_path)
        .map_err(|e| MetricTreeError::LayerLoad(e.to_string()))
}

/// Build the metric tree from an already-resolved scan root.
fn load_tree_at(scan_path: &std::path::Path) -> Result<MetricTree, MetricTreeError> {
    let layer = load_layer_at(scan_path)?;
    Ok(oxy_semantic::build_metric_tree(&layer))
}

/// The airlayer database configs for the workspace.
pub(crate) fn workspace_databases<S: oxy::config::DiskSlot>(
    workspace_manager: &WorkspaceManager<S>,
) -> Vec<oxy_airlayer_compat::DatabaseConfig> {
    OxyMetricTreeRunner::list_databases_sync(workspace_manager)
}

// ── Pure ops: tree, sensitivity, predict ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    /// Optional measure id to root the returned subtree at.
    pub root: Option<String>,
}

/// `GET .../semantic/metric-tree` — the full tree, or `?root=<id>` subtree.
pub async fn get_metric_tree(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    Query(q): Query<TreeQuery>,
) -> Result<Json<MetricTree>, MetricTreeError> {
    let source = resolve_scan(&workspace_manager).await?;
    let tree = load_tree_at(&source.scan_path)?;
    match q.root {
        Some(root) => oxy_semantic::subtree(&tree, &root)
            .map(Json)
            .ok_or_else(|| MetricTreeError::NotFound(format!("measure '{root}' not in tree"))),
        None => Ok(Json(tree)),
    }
}

/// `GET .../semantic/metric-tree/{measure_id}/sensitivity` — ranked drivers.
pub async fn get_sensitivity(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    Path((_workspace_id, measure_id)): Path<(Uuid, String)>,
) -> Result<Json<oxy_airlayer_compat::engine::metric_tree_ops::SensitivityResult>, MetricTreeError>
{
    let source = resolve_scan(&workspace_manager).await?;
    let tree = load_tree_at(&source.scan_path)?;
    oxy_semantic::sensitivity(&tree, &measure_id)
        .map(Json)
        .map_err(|e| MetricTreeError::Op(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct PredictRequest {
    pub changes: Vec<PredictChange>,
    /// Current values keyed by metric-node id. Absent → the historical
    /// value-free behaviour, where multiplicative edges come back
    /// `unquantifiable`. Present → those edges get sized.
    #[serde(default)]
    pub values: Option<std::collections::HashMap<String, f64>>,
    /// Coefficients the baseline fitted, echoed back verbatim. This endpoint
    /// stays database-free by design — the UI re-runs it on every keystroke —
    /// so the fit rides in rather than being re-measured here. Refusals may be
    /// included and are ignored; only an entry carrying a coefficient applies,
    /// and only to an edge that declares none.
    #[serde(default)]
    pub coefficients: Vec<FittedDriver>,
}

#[derive(Debug, Deserialize)]
pub struct PredictChange {
    pub measure: String,
    pub delta: f64,
}

/// Refuse a `predict` request that pins two levers where one is reachable
/// from the other.
///
/// A thin delegate: the rule and its message live in
/// [`oxy_airlayer_compat::reject_lever_conflicts`], because the analytics tool
/// needs the same refusal and cannot depend on this crate. This wrapper stays
/// so the two `post_predict` handlers keep one local name to call and one
/// place to read about which callers are covered.
///
/// **Three enforcement points, not two.** Both `/predict` handlers call this,
/// and `agentic-analytics`'s `predict_impact` tool calls the shared function
/// directly — so curl, `oxyc`, an SDK integration, an agentic analytics tool
/// and a scheduled custom-app function all get the refusal rather than a
/// confident `PredictResult` that silently picked one of the two readings. The
/// browser client's `leverConflicts.ts` is a fourth copy on purpose, and a
/// pre-flight rather than an enforcer: it keeps the UI from firing a request it
/// already knows will 400.
///
/// Returns the message rather than an error type, so this one seam serves
/// both `post_predict` handlers even though they answer in two different
/// error currencies (`MetricTreeError::BadRequest` here, `err_with_code(..,
/// "lever_conflict")` on the `projects/` twin).
pub(crate) fn reject_lever_conflicts(
    tree: &MetricTree,
    changes: &[(String, f64)],
) -> Result<(), String> {
    oxy_airlayer_compat::reject_lever_conflicts(tree, changes)
}

/// `POST .../semantic/metric-tree/predict` — propagate hypothetical deltas.
pub async fn post_predict(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    Json(req): Json<PredictRequest>,
) -> Result<Json<oxy_airlayer_compat::engine::metric_tree_ops::PredictResult>, MetricTreeError> {
    let source = resolve_scan(&workspace_manager).await?;
    let mut tree = load_tree_at(&source.scan_path)?;
    // Before propagation: a fitted coefficient has to make the edge behave
    // exactly as a declared one would, or the forecast the user sees would
    // depend on where the number came from.
    apply_fitted_coefficients(&mut tree, &req.coefficients);
    let changes: Vec<(String, f64)> = req
        .changes
        .into_iter()
        .map(|c| (c.measure, c.delta))
        .collect();
    reject_lever_conflicts(&tree, &changes).map_err(MetricTreeError::BadRequest)?;
    match req.values {
        Some(values) => oxy_semantic::predict_with_values(&tree, &changes, &values),
        None => oxy_semantic::predict(&tree, &changes),
    }
    .map(Json)
    .map_err(|e| MetricTreeError::Op(e.to_string()))
}

// ── Executor bridge ─────────────────────────────────────────────────────────
//
// The executor closure that compiles airlayer `QueryRequest`s and runs them
// through Oxy's connector pool lives in [`crate::agentic_wiring::metric_tree_runner`]
// so both this HTTP handler and the agentic analytics tools route through the
// same code path. See `build_query_executor` there.

/// Build the airlayer `SemanticEngine` for the workspace, with dialects
/// resolved from the configured databases.
pub(crate) fn build_engine(
    layer: oxy_airlayer_compat::SemanticLayer,
    databases: &[oxy_airlayer_compat::DatabaseConfig],
) -> Result<oxy_airlayer_compat::SemanticEngine, MetricTreeError> {
    oxy_airlayer_compat::build_engine(layer, databases)
        .map_err(|e| MetricTreeError::Op(e.to_string()))
}

// ── Query-executing ops: explain, opportunity ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExplainRequest {
    pub target: String,
    pub time_dimension: String,
    /// `[start, end]` inclusive date strings.
    pub current_period: (String, String),
    pub previous_period: (String, String),
    /// Optional `ExplainConfig` override; `None` uses airlayer defaults.
    pub config: Option<ExplainConfigOverride>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ExplainConfigOverride {
    pub deep: Option<bool>,
    pub max_depth: Option<usize>,
    pub coverage_threshold: Option<f64>,
}

pub(crate) fn explain_config(over: Option<ExplainConfigOverride>) -> ExplainConfig {
    let mut config = ExplainConfig::default();
    if let Some(o) = over {
        if let Some(d) = o.deep {
            config.deep = d;
        }
        if let Some(d) = o.max_depth {
            config.max_depth = d;
        }
        if let Some(c) = o.coverage_threshold {
            config.coverage_threshold = c;
        }
    }
    config
}

/// Soft cap on how long an explain call can run before the handler bails.
/// Explain can fire 50+ warehouse queries on rich schemas; the UI just
/// shows a spinner the whole time, so failing loud after the cap is
/// kinder than hanging indefinitely.
const EXPLAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// `POST .../semantic/metric-tree/explain` — period-over-period root cause.
///
/// Goes through `OxyMetricTreeRunner::run_explain` so the HTTP path and
/// the agentic path apply the same dim-pruning policy (the runner strips
/// high-cardinality numeric dims before passing the layer to airlayer).
pub async fn post_explain(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    preagg_ctx: PreaggCacheCtx,
    engine_cache: SemanticEngineCacheCtx,
    Json(req): Json<ExplainRequest>,
) -> Result<Json<ExplainResult>, MetricTreeError> {
    use agentic_analytics::MetricTreeRunner as _;
    // `source` owns the materialised tempdir — it must outlive `runner`, which
    // re-parses the layer from `scan_path` once per run it performs (in the
    // async `snapshot_for_blocking`, not inside the blocking task).
    let source = resolve_scan(&workspace_manager).await?;
    let config_manager = workspace_manager.config_manager.clone();
    // Resolved before the manager moves into the runner. What `source` actually
    // READ, not the request's pinned revision — those differ on a node that
    // holds a working copy and is pinned to a revision.
    let source_revision = source.source_revision(&workspace_manager);
    let runner = OxyMetricTreeRunner::new(workspace_manager, user.id, role)
        .with_scan_path(source.scan_path.clone())
        .with_engine_cache(engine_cache.cache.clone(), source_revision)
        .with_preagg(
            preagg_ctx.cache.clone(),
            preagg_ctx.renewal_threshold_secs_or(&config_manager),
        );
    let config = explain_config(req.config);

    let result = tokio::time::timeout(
        EXPLAIN_TIMEOUT,
        runner.run_explain(
            req.target,
            req.time_dimension,
            (req.current_period.0, req.current_period.1),
            (req.previous_period.0, req.previous_period.1),
            vec![],
            config,
        ),
    )
    .await
    .map_err(|_| {
        MetricTreeError::Op(format!(
            "explain timed out after {}s — consider narrowing the target measure or filtering dims",
            EXPLAIN_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| MetricTreeError::Op(e.to_string()))?;

    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
pub struct OpportunityRequest {
    pub target: String,
    pub time_dimension: String,
    /// `[start, end]` inclusive date strings.
    pub period: (String, String),
    /// Which statistic over the peer segments sets the benchmark:
    /// `median` | `p75` | `best_peer`. Omitted means
    /// [`agentic_analytics::metric_tree_runner::DEFAULT_BENCHMARK_STATISTIC`].
    ///
    /// The caller's choice rather than the engine's: airlayer deleted the
    /// adaptive rule that used to make it (best segment for a thin dimension,
    /// p75 once a percentile meant something) and no fixed value reproduces
    /// it, so freezing one server-side would settle a product question inside
    /// a dependency bump.
    #[serde(default = "default_benchmark_statistic")]
    pub statistic: BenchmarkStatistic,
    /// Narrow the scan to one world-model instance. `None` sizes across the
    /// whole population.
    pub instance: Option<OpportunityInstance>,
}

/// A world-model instance to scope a scan to, addressed the way the rest of the
/// world-model API addresses one: entity name plus instance key.
#[derive(Debug, Deserialize)]
pub struct OpportunityInstance {
    pub entity: String,
    /// JSON array for a composite key, else a bare scalar.
    pub key: String,
}

/// Resolve an [`OpportunityRequest`]'s optional instance into the engine scope.
///
/// No instance → an empty scope, i.e. size the whole population.
///
/// An instance that cannot be resolved is an error rather than an ignored
/// scope: silently sizing the population under a request that asked for one
/// instance is how a panel ends up reporting company-wide numbers under an
/// instance header.
fn opportunity_scope(
    layer: &oxy_airlayer_compat::SemanticLayer,
    req: &OpportunityRequest,
) -> Result<Vec<oxy_airlayer_compat::engine::query::QueryFilter>, MetricTreeError> {
    let Some(instance) = req.instance.as_ref() else {
        return Ok(Vec::new());
    };
    crate::server::api::world_model_graph::instance_scope_filters(
        layer,
        &instance.entity,
        &instance.key,
    )
    .ok_or_else(|| {
        MetricTreeError::NotFound(format!(
            "cannot scope '{}' to '{}': no entity named '{}' in the semantic model",
            req.target, instance.key, instance.entity
        ))
    })
}

/// Airlayer's sizing result, plus the denominator that makes its rates legible.
#[derive(Debug, Serialize)]
pub struct OpportunityResponse {
    #[serde(flatten)]
    pub result: OpportunityResult,
    /// The `count` measure whose value divides the target to form the per-unit
    /// rates, as a `view.measure` id — set only in `weight_basis: "rows"` mode,
    /// the only mode that forms rates.
    ///
    /// Without it a rate is an unlabelled number: the panel can only say
    /// "533.9 vs 801.6", and a reader has no way to learn that means revenue per
    /// order, nor *which* count it divided by. That last part is not pedantry —
    /// the engine takes the FIRST declared `count` measure, so a view declaring
    /// a filtered one (`completed_orders`) silently rates against a different
    /// denominator than a reader would assume. Naming it is how a modeller can
    /// notice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_denominator: Option<String>,
}

/// First `type: count` measure declared on `view_name`, as a `view.measure` id.
///
/// Mirrors airlayer's private `discover_count_measure` — same "first Count
/// measure wins" rule, so the id we report is the one the engine actually
/// divided by. Kept in sync by hand: if the crate ever surfaces the denominator
/// on `OpportunityResult` itself, delete this and read it from there.
fn count_measure_id(layer: &oxy_airlayer_compat::SemanticLayer, view_name: &str) -> Option<String> {
    let view = layer.views.iter().find(|v| v.name == view_name)?;
    view.measures_list()
        .iter()
        .find(|m| m.measure_type == oxy_airlayer_compat::schema::models::MeasureType::Count)
        .map(|m| format!("{}.{}", view_name, m.name))
}

/// Whether `target` (a `view.measure` id) is something the drill sizes on a
/// per-unit rate (`filtered_sum / count`).
///
/// airlayer no longer hard-gates rate mode to `type: sum` — an eligible additive
/// composite (e.g. a `type: number | custom` root whose refs flatten to a single
/// same-view additive expression) is rate-sized too. `supports_rate_basis` is
/// airlayer's own authoritative answer to this question; call it rather than
/// re-deriving eligibility from `measure_type` alone, which is how this handler
/// used to accept only `type: sum` and silently omit `rate_denominator` for an
/// accepted composite. Mirrors that rule handler-side, since `DrillResult` does
/// not surface the weight basis the way `OpportunityResult` does.
fn target_supports_rate_basis(layer: &oxy_airlayer_compat::SemanticLayer, target: &str) -> bool {
    oxy_airlayer_compat::engine::metric_tree_ops::supports_rate_basis(layer, target)
}

/// `POST .../semantic/metric-tree/opportunity` — segment opportunity sizing.
pub async fn post_opportunity(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    preagg_ctx: PreaggCacheCtx,
    Json(req): Json<OpportunityRequest>,
) -> Result<Json<OpportunityResponse>, MetricTreeError> {
    // `source` owns the materialised tempdir; hold it until the layer below is
    // parsed. Nothing inside the blocking task reads it — the executor derives
    // its own (different) pre-aggregation cache root from the workspace manager.
    let source = resolve_scan(&workspace_manager).await?;
    let mut layer = load_layer_at(&source.scan_path)?;
    // Order is load-bearing. The tree is built from the clean layer: the
    // dispersion measure below is a pass-through, and pass-throughs read as
    // composite nodes, so a tree built after it would sprout an internal
    // `__opp_stddev__…` node into the world-model graph. The engine, by
    // contrast, must be built from the augmented layer — it resolves measure
    // names against its own copy, and would reject a measure it had never seen.
    let tree = oxy_semantic::build_metric_tree(&layer);
    oxy_airlayer_compat::engine::metric_tree_ops::augment_layer_for_opportunity(
        &mut layer,
        &req.target,
    );
    let scope = opportunity_scope(&layer, &req)?;
    // Resolved here because `layer` and `req` are both moved into the blocking
    // task below. Read from the augmented layer deliberately, but this is not
    // shadow-proof: `augment_layer_for_opportunity` also installs a
    // `MeasureType::Count` companion (`__opp_n__<measure>`) when the target
    // carries `.filters`, and `count_measure_id` — like the engine's own
    // `discover_count_measure` — takes the FIRST `Count` measure on the view.
    // On a view with a natural `count` measure, that one is declared first and
    // wins, so the synthetic companion never shows up here. On a view with NO
    // natural count measure, the synthetic companion becomes the only `Count`
    // found and gets reported as the denominator — naming a measure that
    // doesn't exist on the layer the analyst actually sees.
    let denominator = req
        .target
        .split_once('.')
        .and_then(|(view, _)| count_measure_id(&layer, view));
    let databases = workspace_databases(&workspace_manager);
    // Not cached, and deliberately: `augment_layer_for_opportunity` above
    // rewrote `layer` for this request's target, so this engine is unique to
    // the request and shares nothing with the workspace's.
    let engine = std::sync::Arc::new(build_engine(layer.clone(), &databases)?);
    let handle = tokio::runtime::Handle::current();
    let preagg = crate::agentic_wiring::metric_tree_runner::RunnerPreagg {
        cache: preagg_ctx.cache.clone(),
        renewal_threshold_secs: preagg_ctx
            .renewal_threshold_secs_or(&workspace_manager.config_manager),
        // A read surface: an opportunity sizing is a display of the data, so a
        // rollup a cycle behind beats a warehouse scan.
        freshness: crate::server::preagg_context::RollupFreshness::ServeStale,
    };

    let result = tokio::task::spawn_blocking(move || {
        let executor = build_query_executor(
            engine,
            databases,
            workspace_manager,
            user.id,
            role,
            handle,
            preagg,
        );
        oxy_airlayer_compat::engine::metric_tree_ops::opportunity(
            &tree,
            &layer,
            &req.target,
            &req.time_dimension,
            (req.period.0.as_str(), req.period.1.as_str()),
            &scope,
            req.statistic,
            OPPORTUNITY_MIN_SUPPORT,
            &executor,
        )
    })
    .await
    .map_err(|e| MetricTreeError::Op(format!("opportunity task panicked: {e}")))?
    .map_err(|e| MetricTreeError::Op(e.to_string()))?;

    // Only "rows" mode divides by the count to form rates; reporting a
    // denominator for a scan that never used one would invite the reader to
    // read `current_value` as a rate when it is a raw measure value.
    let rate_denominator = (result.weight_basis == "rows")
        .then_some(denominator)
        .flatten();
    Ok(Json(OpportunityResponse {
        result,
        rate_denominator,
    }))
}

// ── Baseline (scenario simulation) ──────────────────────────────────────────

/// Soft cap on the baseline read as a whole — not one query but a fan-out of
/// them. `baseline_reads` asks `grouped_values` for one query per view in the
/// forward-reachable set, then `grouped_fit` for one more per single-view
/// group plus up to one per cross-view pair (worst case N + V(V-1)/2), all
/// against the same warehouse a cold scan can already hang on its own — same
/// reasoning as [`EXPLAIN_TIMEOUT`], on a shorter clock because this handler
/// exists to show a lever's current value, not to explain a query plan.
///
/// The outer `tokio::time::timeout` around this handler's `spawn_blocking`
/// only bounds how long the CLIENT waits — it cannot cancel the OS thread, so
/// a fan-out with no deadline of its own would keep issuing warehouse queries
/// after the response had already gone back as a timeout. What actually
/// bounds the fan-out is the shared, absolute deadline `baseline_reads`
/// builds from this constant and threads through
/// `metric_tree_baseline::splitting_executor` — checked before every single
/// query the executor is asked to run, not only inside its additivity-split
/// retry — so the read degrades to partial values and a recorded budget
/// refusal instead of outliving the request that asked for it.
///
/// `pub(crate)` so the `projects/` twin shares this exact number instead of
/// keeping its own copy: two handlers hitting the same warehouse with two
/// silently diverging deadlines is a bug waiting for someone to tune one.
pub(crate) const BASELINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The tail of [`BASELINE_TIMEOUT`] that [`BASELINE_QUERY_BUDGET`] deliberately
/// does NOT spend — reserved for *finishing*, not for *starting*.
///
/// The deadline cannot make the response beat `BASELINE_TIMEOUT`: nothing
/// bounds a single warehouse query, and the outer `tokio::time::timeout` wraps
/// a `spawn_blocking` it cannot cancel. What it can do is stop the fan-out
/// issuing a query so late that the request is already doomed when it lands.
/// So the budget is `BASELINE_TIMEOUT` MINUS what one last query plus the work
/// after it plausibly needs — a subtraction, not a fraction of the whole.
///
/// Ten seconds covers, in order of cost: the one query already in flight when
/// the deadline passes — uncancellable, and the only genuinely unbounded term
/// here — plus everything `baseline_reads` still has to do after it, which is
/// `FittedDriver::with_profile`, [`classify_unvalued`], a sort and the JSON.
/// That tail is in-memory over a few hundred rows: microseconds against the
/// query's seconds. So this is effectively all in-flight-query allowance, and
/// generous at that — more than ten times the per-call allowance the fan-out
/// itself implies (see [`BASELINE_QUERY_BUDGET`]).
pub(crate) const BASELINE_QUERY_HEADROOM: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a scenario baseline may go on ISSUING warehouse queries.
///
/// **What it is.** `BASELINE_TIMEOUT - BASELINE_QUERY_HEADROOM`, as ONE
/// absolute deadline shared by both of [`baseline_reads`]' reads and every
/// query either of them provokes. Absolute and shared because a per-call
/// duration would restart for each read and so bound nothing.
///
/// **What it has to fit — the whole fan-out, not one query.** For a
/// forward-reachable set spanning `V` views, `baseline_reads` issues
///
/// ```text
///   V              grouped_values  — one request per view
/// + V              grouped_fit     — one panel request per single-view group
/// + V(V-1)/2       grouped_fit     — one panel request per cross-view PAIR
/// = V(V+3)/2 executor calls
/// ```
///
/// and each of those may fan out again, into one query per (view, additivity)
/// group, when airlayer refuses the batch. `example_new` — the workspace this
/// repo's fixtures and `tests/platform/metric_tree_fit_panel` run against —
/// ships six views, so `V = 6` gives `6 + 6 + 15 = 27` executor calls. Twenty
/// seconds across 27 of them is ~740ms per warehouse round trip.
///
/// **Why not half of `BASELINE_TIMEOUT`.** It was `BASELINE_TIMEOUT / 2` when
/// the deadline gated only the additivity-split RETRY: a rare path, a handful
/// of queries, where spending half the request was plainly fine. It now gates
/// every query the executor serves — `split_groups` checks it before issuing
/// `req` at all — so that same 15s had to cover all 27 calls: ~550ms each,
/// under a cold warehouse round trip. That made the DEADLINE, not the
/// warehouse, the thing that failed the read: a six-view tree that used to
/// answer in ~20s would start returning partial values and `BUDGET_SPENT`.
/// Widening the remit without re-sizing the number was the regression.
///
/// **What happens when it is hit.** `split_groups` returns `BUDGET_SPENT`
/// rather than issuing the query, so the read degrades to values for the views
/// that answered plus a refusal naming the budget for the rest — instead of
/// outliving the request that asked for it. That degradation is the point:
/// pushing this any closer to `BASELINE_TIMEOUT` trades an honest partial
/// answer for a timed-out response with queries still hitting the warehouse.
pub(crate) const BASELINE_QUERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(
    BASELINE_TIMEOUT
        .as_secs()
        .saturating_sub(BASELINE_QUERY_HEADROOM.as_secs()),
);

/// The shared deadline a baseline fan-out starts against.
///
/// A function rather than an `Instant::now() + BASELINE_QUERY_BUDGET` at each
/// call site: `metric_tree_projection` kept its own copy of the old
/// `BASELINE_TIMEOUT / 2`, so tuning one of them tuned half the surface. Same
/// reasoning that makes `BASELINE_TIMEOUT` itself `pub(crate)`.
pub(crate) fn baseline_query_deadline() -> std::time::Instant {
    std::time::Instant::now() + BASELINE_QUERY_BUDGET
}

#[derive(Debug, Deserialize)]
pub struct BaselineRequest {
    /// Lever node ids. Values are fetched for these plus everything
    /// forward-reachable from them — not the whole tree.
    pub roots: Vec<String>,
    pub time_dimension: String,
    /// `[start, end]` inclusive date strings.
    pub period: (String, String),
    /// Narrow to one world-model instance. `None` values the whole population.
    pub instance: Option<BaselineInstance>,
}

/// A world-model instance, addressed the way the rest of the world-model API
/// addresses one. Identical in shape to `OpportunityInstance` on purpose: the
/// scope picker is the same component, and one vocabulary beats two.
#[derive(Debug, Deserialize)]
pub struct BaselineInstance {
    pub entity: String,
    /// JSON array for a composite key, else a bare scalar.
    pub key: String,
}

#[derive(Debug, Serialize)]
pub struct BaselineResponse {
    /// node_id → current value over the window.
    pub values: std::collections::HashMap<String, f64>,
    /// Reachable nodes with no value, and why.
    pub unvalued: Vec<UnvaluedNode>,
    /// Echoed back: the query is expensive and the client caches on it.
    pub resolved_period: (String, String),
    /// Why the baseline produced no values, in words the UI can show. `None`
    /// when measures were valued normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_note: Option<String>,
    /// Coefficients measured from history for driver edges that declare none,
    /// plus the refusals. Returned from the baseline rather than from
    /// `predict` because fitting is a warehouse query and `predict` re-runs on
    /// every keystroke; the client echoes these back into `predict`.
    ///
    /// Empty when every reachable driver edge already declares a coefficient,
    /// which costs no query at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fitted: Vec<FittedDriver>,
}

/// What a scenario baseline's two reads produced.
///
/// A struct rather than a tuple because `skipped` has to reach both
/// [`classify_unvalued`] and [`baseline_note`], and a four-tuple threaded
/// through two handlers is where the fourth element quietly stops being
/// passed.
pub(crate) struct BaselineReads {
    pub values: std::collections::HashMap<String, f64>,
    pub outcome: BaselineOutcome,
    pub fitted: Vec<FittedDriver>,
    /// Views the values read could not ask about. Empty in the ordinary case.
    pub skipped: Vec<SkippedGroup>,
}

/// Both warehouse reads a scenario baseline needs, against one executor.
///
/// They are two different queries — values are one aggregate over the window,
/// the fit needs that window broken out by panel and day — but they share a
/// window, a scope and a connection, so running them together is what keeps
/// the executor construction in one place. Shared with the `projects/` twin,
/// which builds its executor differently but needs the identical reads.
pub(crate) fn baseline_reads(
    tree: &MetricTree,
    layer: &oxy_airlayer_compat::SemanticLayer,
    roots: &[String],
    time_dimension: &str,
    period: (&str, &str),
    scope: &[oxy_airlayer_compat::engine::query::QueryFilter],
    executor: Box<oxy_airlayer_compat::engine::metric_tree_ops::QueryExecutor>,
) -> BaselineReads {
    // Both reads below batch measures for their own reasons, and both are
    // refused when the batch mixes an additive measure with a non-additive one
    // from the same view under a fan-out join. Which one breaks depends only
    // on which lever was pinned, so the retry belongs around the executor
    // rather than at either call site — see `metric_tree_baseline`. Only a
    // refusal is retried, so a single-view tree still costs one round trip per
    // read.
    //
    // Taken by value, and wrapped here rather than by the callers, so a third
    // baseline call site cannot be added that quietly skips the retry.
    // ONE deadline shared by both reads below, so a workspace whose view count
    // makes the fan-out expensive degrades to the honest refusal instead of
    // spending the whole `BASELINE_TIMEOUT` and timing the response out with
    // queries still in flight. A per-call duration would give each read the
    // full allowance and bound nothing. Sized in `BASELINE_QUERY_BUDGET`.
    let split_deadline = baseline_query_deadline();
    let executor =
        super::metric_tree_baseline::splitting_executor(layer.clone(), executor, split_deadline);
    let executor = &*executor;

    // Both reads go one view at a time — see `metric_tree_groups`. A batch
    // spanning views with no join path between them is refused as a whole, and
    // the measures that needed no join go down with it: nine identical
    // "could not be sized from history" refusals for one failed query, seven
    // of whose edges were single-view. Grouping is what stops one unjoinable
    // pair from speaking for the rest.
    let grouped = super::metric_tree_groups::grouped_values(
        tree,
        layer,
        roots,
        time_dimension,
        period,
        scope,
        executor,
    );
    let fitted: Vec<_> = super::metric_tree_groups::grouped_fit(
        tree,
        layer,
        roots,
        time_dimension,
        period,
        scope,
        executor,
    )
    .into_iter()
    // Sample each response here, because this is the only place that has both the
    // fit and the target's current aggregate — a log link needs the latter and the
    // fit never sees it. Without this the UI receives coefficients it would have to
    // interpret per shape, which is exactly what the profile exists to avoid.
    .map(|f| {
        let target = grouped.values.get(&f.to).copied();
        // The profile has to be sampled in the same space `predict` will apply
        // the coefficients in, or the curve the UI draws and the number it
        // reports come from two different evaluations of one fit.
        let space = super::metric_tree_groups::space_of(tree, &f.to);
        f.with_profile(target, space)
    })
    .collect();
    BaselineReads {
        values: grouped.values,
        outcome: grouped.outcome,
        fitted,
        skipped: grouped.skipped,
    }
}

/// Turn an engine outcome into something worth showing a person.
///
/// Each case names a different fix, which is the whole reason the engine
/// reports them separately.
///
/// `skipped` is appended rather than folded into the outcome: a read where one
/// view was valued and another was never asked about has *both* things to say,
/// and the outcome enum can only carry one of them.
pub(crate) fn baseline_note(
    outcome: &BaselineOutcome,
    time_dimension: &str,
    skipped: &[SkippedGroup],
) -> Option<String> {
    let engine = match outcome {
        // A partially-unreadable read still says so. The measures named here
        // were asked for and came back as something that is not a number, so
        // silence would leave them looking like nodes the window had nothing
        // for — a different problem with a different fix.
        BaselineOutcome::Valued { unreadable } if !unreadable.is_empty() => Some(format!(
            "{} came back unreadable (not a number) and were left unvalued",
            quoted_list(unreadable)
        )),
        BaselineOutcome::Valued { .. } => None,
        BaselineOutcome::NothingRequested => None,
        BaselineOutcome::ExecutorError(msg) => Some(format!("the warehouse rejected the query: {msg}")),
        BaselineOutcome::NoRows => Some(format!(
            "no rows in this period on `{time_dimension}` — try a longer window, or a time dimension the pinned measure actually has data for"
        )),
        BaselineOutcome::NoMatchingColumns => Some(
            "the query returned rows but none carried these measures — the time dimension may not belong to the pinned measure's view".to_string(),
        ),
        // Deliberately not folded into `NoRows`: "your window is empty" would
        // send someone lengthening a window that was never the problem.
        BaselineOutcome::UnreadableValues(ids) => Some(format!(
            "the query returned rows, but {} held values that are not numbers — check those measures' expressions",
            quoted_list(ids)
        )),
        // `BaselineOutcome` is `#[non_exhaustive]`, so a variant added upstream
        // lands here instead of failing the build. No note rather than a wrong
        // one — but nothing will tell you, so re-read this match on a pin bump.
        _ => None,
    };
    // Grouped by reason, because every view a tree reaches but cannot window
    // is refused for the SAME reason: one sentence per view restated that
    // reason verbatim, and a workspace with nine airhouse views turned the
    // note into nine copies of one paragraph. First-appearance order, so the
    // list still reads in the order the views were tried.
    let mut groups: Vec<((SkipKind, &str), Vec<String>)> = Vec::new();
    for s in skipped {
        let key = (s.kind, s.reason.as_str());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, views)) => views.push(s.view.clone()),
            None => groups.push((key, vec![s.view.clone()])),
        }
    }
    let skips: Vec<String> = groups
        .iter()
        .map(|((kind, reason), views)| {
            let names = quoted_list(views);
            match kind {
                SkipKind::NotQueried => {
                    let verb = if views.len() == 1 { "was" } else { "were" };
                    format!("{names} {verb} not read: {reason}")
                }
                SkipKind::QueryFailed => {
                    format!("the warehouse rejected the query for {names}: {reason}")
                }
            }
        })
        .collect();
    match (engine, skips.is_empty()) {
        (None, true) => None,
        (None, false) => Some(skips.join("; ")),
        (Some(note), true) => Some(note),
        (Some(note), false) => Some(format!("{note}; {}", skips.join("; "))),
    }
}

/// `` `a`, `b` `` — measure ids for a sentence, backticked so a dotted path
/// reads as one token rather than as prose.
fn quoted_list(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("`{id}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UnvaluedNode {
    pub node_id: String,
    pub reason: UnvaluedReason,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnvaluedReason {
    /// The query ran; this node's alias was absent from the row.
    NoRowsInWindow,
    /// The executor errored — applies to every reachable node.
    QueryFailed,
    /// Rows came back, but none carried any of the requested measures —
    /// usually a time dimension outside the pinned measure's view.
    ///
    /// A fourth outcome the engine reports separately and this used to fold
    /// into `NoRowsInWindow`, so every node said "no rows in window" while the
    /// banner above it said the rows *were* there. The two name different
    /// fixes: lengthen the window, versus pick a different time dimension.
    NoMatchingColumns,
    /// No query was issued for this node's view at all — see [`SkippedGroup`].
    ///
    /// Distinct from all three above, which describe a query that ran. The
    /// fix is neither a longer window nor a different dimension on the *pinned*
    /// measure: this view cannot be read over this window, and the note says
    /// why. Folding it into `NoRowsInWindow` would advise lengthening a window
    /// that was never applied.
    NotQueried,
}

/// Diff the forward-reachable set against the values actually returned.
///
/// `reachable_values` silently omits any node whose alias is missing from the
/// row, and returns an empty map outright when the query fails. Without this
/// diff the canvas cannot tell "genuinely zero" from "we didn't get it", and
/// `0` is the most dangerous default a simulation can show.
///
/// Takes the outcome rather than a `query_failed` flag, which is what caught
/// the case that motivated this: under the flag, `NoMatchingColumns` fell into
/// "no rows in window" while the banner directly above it said the rows WERE
/// there. Note the guarantee has weakened — airlayer marked `BaselineOutcome`
/// `#[non_exhaustive]`, so a new variant no longer fails this build; the
/// wildcard arm below is what it hits, and a pin bump is when to re-read it.
pub(crate) fn classify_unvalued(
    tree: &oxy_airlayer_compat::engine::metric_tree::MetricTree,
    roots: &[String],
    values: &std::collections::HashMap<String, f64>,
    outcome: &BaselineOutcome,
    skipped: &[SkippedGroup],
) -> Vec<UnvaluedNode> {
    let reason = match outcome {
        BaselineOutcome::ExecutorError(_) => UnvaluedReason::QueryFailed,
        BaselineOutcome::NoMatchingColumns => UnvaluedReason::NoMatchingColumns,
        // An unreadable value is a column that WAS there and could not be
        // read as a number. Nearest existing reason: the column did not
        // answer. It is not "no rows in window" — the rows were there — and
        // the note above names the measures, which is where the detail lives
        // until this enum grows a case of its own.
        BaselineOutcome::UnreadableValues(_) => UnvaluedReason::NoMatchingColumns,
        BaselineOutcome::Valued { .. }
        | BaselineOutcome::NothingRequested
        | BaselineOutcome::NoRows => UnvaluedReason::NoRowsInWindow,
        // The compile-time guarantee this function's doc claims is only as
        // strong as the enum: airlayer marked it `#[non_exhaustive]`, so a new
        // outcome now reaches here silently. `NoMatchingColumns` is the
        // least-wrong default — "this node did not come back" — because the
        // alternative, `NoRowsInWindow`, is the one reading that tells someone
        // to lengthen a window when we do not know that the window is why.
        _ => UnvaluedReason::NoMatchingColumns,
    };
    // Per-node, and it wins over the outcome: with one view valued and another
    // skipped the outcome is `Valued`, which would otherwise label the skipped
    // nodes "no rows in window" — a query that never ran, or that errored,
    // reporting on its window.
    let per_view: std::collections::HashMap<&str, UnvaluedReason> = skipped
        .iter()
        .flat_map(|s| {
            let reason = match s.kind {
                SkipKind::NotQueried => UnvaluedReason::NotQueried,
                SkipKind::QueryFailed => UnvaluedReason::QueryFailed,
            };
            s.nodes.iter().map(move |n| (n.as_str(), reason.clone()))
        })
        .collect();
    // A root absent from the tree is rejected earlier with a 400, so
    // everything this returns is genuinely part of the reachable set.
    let mut unvalued: Vec<UnvaluedNode> =
        super::metric_tree_baseline::forward_reachable(tree, roots)
            .into_iter()
            .filter(|id| !values.contains_key(id))
            .map(|node_id| {
                let reason = per_view
                    .get(node_id.as_str())
                    .cloned()
                    .unwrap_or_else(|| reason.clone());
                UnvaluedNode { node_id, reason }
            })
            .collect();
    // Sorted rather than left in traversal order, so the response is stable
    // and the frontend cache key is meaningful.
    unvalued.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    unvalued
}

/// Resolve a [`BaselineRequest`]'s optional instance into the engine scope.
///
/// Mirrors [`opportunity_scope`]: no instance → an empty scope, i.e. value
/// the whole population. An instance that cannot be resolved is an error
/// rather than an ignored scope — silently valuing the whole population
/// under a request that named one instance is how a panel reports
/// company-wide numbers under an instance header.
fn baseline_scope(
    layer: &oxy_airlayer_compat::SemanticLayer,
    req: &BaselineRequest,
) -> Result<Vec<oxy_airlayer_compat::engine::query::QueryFilter>, MetricTreeError> {
    baseline_scope_core(layer, req.instance.as_ref()).map_err(MetricTreeError::NotFound)
}

/// The rule, once, for both surfaces.
///
/// The `projects/` twin needs the same decision in a different error currency
/// (a `Response`, not a `MetricTreeError`), so the rule lives here and each
/// caller supplies only the wrapping. Two copies of *this* is what a scope that
/// silently drops on one surface and errors on the other would look like.
///
/// Returns the message on failure rather than an error type, so neither
/// currency leaks into the other's module.
pub(crate) fn baseline_scope_core(
    layer: &oxy_airlayer_compat::SemanticLayer,
    instance: Option<&BaselineInstance>,
) -> Result<Vec<oxy_airlayer_compat::engine::query::QueryFilter>, String> {
    let Some(instance) = instance else {
        return Ok(Vec::new());
    };
    crate::server::api::world_model_graph::instance_scope_filters(
        layer,
        &instance.entity,
        &instance.key,
    )
    .ok_or_else(|| {
        format!(
            "cannot scope to '{}': no entity named '{}' in the semantic layer",
            instance.key, instance.entity
        )
    })
}

/// Build the executor and run the baseline's per-view value and per-group fit
/// queries (see [`BASELINE_TIMEOUT`]), off the async runtime. Split out of
/// [`post_baseline`] to stay under the file's function-length budget —
/// mirrors the projects/ twin's `run_baseline_query`.
///
/// Unlike [`post_opportunity`], this does not go through
/// `OxyMetricTreeRunner` — the fan-out `baseline_reads` drives isn't part of
/// the `MetricTreeRunner` trait — so the executor is built directly here.
#[allow(clippy::too_many_arguments)]
async fn run_baseline_query(
    workspace_manager: WorkspaceManager<oxy::config::ReadOnly>,
    user_id: Uuid,
    role: WorkspaceRole,
    preagg_ctx: PreaggCacheCtx,
    layer: oxy_airlayer_compat::SemanticLayer,
    req: &BaselineRequest,
    scope: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
) -> Result<BaselineReads, MetricTreeError> {
    let tree = oxy_semantic::build_metric_tree(&layer);
    let databases = workspace_databases(&workspace_manager);
    let engine = std::sync::Arc::new(build_engine(layer.clone(), &databases)?);
    let handle = tokio::runtime::Handle::current();
    let preagg = crate::agentic_wiring::metric_tree_runner::RunnerPreagg {
        cache: preagg_ctx.cache.clone(),
        renewal_threshold_secs: preagg_ctx
            .renewal_threshold_secs_or(&workspace_manager.config_manager),
        // A read surface, same as `post_opportunity` above: a baseline is a
        // display of the data, so a rollup a cycle behind beats a warehouse
        // scan across every root in the batch.
        freshness: crate::server::preagg_context::RollupFreshness::ServeStale,
    };
    let roots = req.roots.clone();
    let period = req.period.clone();
    let time_dimension = req.time_dimension.clone();

    tokio::time::timeout(
        BASELINE_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let executor = build_query_executor(
                engine,
                databases,
                workspace_manager,
                user_id,
                role,
                handle,
                preagg,
            );
            baseline_reads(
                &tree,
                &layer,
                &roots,
                &time_dimension,
                (period.0.as_str(), period.1.as_str()),
                &scope,
                executor,
            )
        }),
    )
    .await
    .map_err(|_| {
        MetricTreeError::Op(format!(
            "baseline timed out after {}s — consider narrowing the period or the scope",
            BASELINE_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| MetricTreeError::Op(format!("baseline task panicked: {e}")))
}

/// `POST .../semantic/metric-tree/baseline` — current values for the levers
/// and everything downstream of them.
pub async fn post_baseline(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    preagg_ctx: PreaggCacheCtx,
    Json(req): Json<BaselineRequest>,
) -> Result<Json<BaselineResponse>, MetricTreeError> {
    let source = resolve_scan(&workspace_manager).await?;
    let layer = load_layer_at(&source.scan_path)?;
    let tree = oxy_semantic::build_metric_tree(&layer);

    // Reject unknown levers up front: a typo must not read as "this measure
    // has no value", which is a completely different message in the UI.
    for root in &req.roots {
        if !tree.nodes.iter().any(|n| &n.id == root) {
            return Err(MetricTreeError::NotFound(format!(
                "measure '{root}' not in tree"
            )));
        }
    }

    let scope = baseline_scope(&layer, &req)?;

    let reads = run_baseline_query(
        workspace_manager,
        user.id,
        role,
        preagg_ctx,
        layer.clone(),
        &req,
        scope,
    )
    .await?;

    // Ask the engine WHY rather than inferring it from an empty map: an
    // executor error and an empty window are opposite problems with opposite
    // fixes, and guessing produced a message that told users the wrong one.
    let unvalued = classify_unvalued(
        &tree,
        &req.roots,
        &reads.values,
        &reads.outcome,
        &reads.skipped,
    );

    Ok(Json(BaselineResponse {
        values: reads.values,
        unvalued,
        resolved_period: req.period,
        baseline_note: baseline_note(&reads.outcome, &req.time_dimension, &reads.skipped),
        fitted: reads.fitted,
    }))
}

// ── Query-executing op: opportunity drill (recursive decomposition) ──────────

#[derive(Debug, Deserialize)]
pub struct DrillRequest {
    pub target: String,
    pub time_dimension: String,
    /// `[start, end]` inclusive date strings.
    pub period: (String, String),
    /// See [`OpportunityRequest::statistic`]. A drill re-scans the root the
    /// same way, so it takes the same benchmark.
    #[serde(default = "default_benchmark_statistic")]
    pub statistic: BenchmarkStatistic,
    /// Narrow the scan to one world-model instance. `None` drills across the
    /// whole population.
    pub instance: Option<OpportunityInstance>,
    /// Optional overrides; defaults to airlayer's `DrillConfig::default()`
    /// (max_depth 5, alpha = the single-scan significance budget).
    pub max_depth: Option<usize>,
    pub alpha: Option<f64>,
    /// Root the drill at a specific row of the root scan instead of its
    /// top-ranked one. The merged panel sends this when an analyst expands a
    /// row that is not the engine's own pick.
    pub root: Option<oxy_airlayer_compat::engine::metric_tree_ops::DrillRoot>,
}

/// The drill tree, plus the denominator that makes its rates legible (same role
/// as [`OpportunityResponse::rate_denominator`]). `result` is null when the root
/// scan found nothing to drill (`Ok(None)`).
#[derive(Debug, Serialize)]
pub struct DrillResponse {
    #[serde(flatten)]
    pub result: Option<oxy_airlayer_compat::engine::metric_tree_ops::DrillResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_denominator: Option<String>,
}

/// Resolve a [`DrillRequest`]'s optional instance into the engine scope.
///
/// Mirrors [`opportunity_scope`] against `DrillRequest`: no instance → an empty
/// scope (drill the whole population); an instance that cannot be resolved is an
/// error rather than a silently-ignored scope.
fn opportunity_scope_from_drill(
    layer: &oxy_airlayer_compat::SemanticLayer,
    req: &DrillRequest,
) -> Result<Vec<oxy_airlayer_compat::engine::query::QueryFilter>, MetricTreeError> {
    let Some(instance) = req.instance.as_ref() else {
        return Ok(Vec::new());
    };
    crate::server::api::world_model_graph::instance_scope_filters(
        layer,
        &instance.entity,
        &instance.key,
    )
    .ok_or_else(|| {
        MetricTreeError::NotFound(format!(
            "cannot scope '{}' to '{}': no entity named '{}' in the semantic model",
            req.target, instance.key, instance.entity
        ))
    })
}

/// `POST .../semantic/metric-tree/drill` — recursive gap decomposition.
///
/// Mirrors [`post_opportunity`] with three deltas: the augmented layer is shared
/// via `Arc<RwLock<>>`, the executor rebuilds its engine per query from that
/// shared layer (so the drill's mid-recursion synthetic measures compile), and
/// it calls `opportunity_drill` instead of `opportunity`.
pub async fn post_opportunity_drill(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    preagg_ctx: PreaggCacheCtx,
    Json(req): Json<DrillRequest>,
) -> Result<Json<DrillResponse>, MetricTreeError> {
    // `source` owns the materialised tempdir; hold it until the layer below is
    // parsed. Nothing inside the blocking task reads it — the executor derives
    // its own (different) pre-aggregation cache root from the workspace manager.
    let source = resolve_scan(&workspace_manager).await?;
    let mut clean_layer = load_layer_at(&source.scan_path)?;
    // Tree from the CLEAN layer (same reason as post_opportunity: augmentation
    // adds pass-through measures that would sprout graph nodes).
    let tree = oxy_semantic::build_metric_tree(&clean_layer);
    // Augment the ROOT target before sharing — the drill's children augment
    // themselves via dimension_candidates.
    oxy_airlayer_compat::engine::metric_tree_ops::augment_layer_for_opportunity(
        &mut clean_layer,
        &req.target,
    );
    // opportunity_scope + denominator read from the augmented layer, same as
    // post_opportunity.
    let scope = opportunity_scope_from_drill(&clean_layer, &req)?;
    let denominator = req
        .target
        .split_once('.')
        .and_then(|(view, _)| count_measure_id(&clean_layer, view));
    // Read the target's rate-basis eligibility before the layer is moved into
    // the Arc, so the response can gate the rate denominator on rows mode
    // (sum, or an eligible additive composite).
    let target_supports_rate_basis = target_supports_rate_basis(&clean_layer, &req.target);

    let databases = workspace_databases(&workspace_manager);
    let dialects = oxy_airlayer_compat::DatasourceDialectMap::from_config_databases(&databases);
    let shared: oxy_airlayer_compat::engine::metric_tree_ops::SharedLayer =
        std::sync::Arc::new(std::sync::RwLock::new(clean_layer));
    let handle = tokio::runtime::Handle::current();
    let preagg = crate::agentic_wiring::metric_tree_runner::RunnerPreagg {
        cache: preagg_ctx.cache.clone(),
        renewal_threshold_secs: preagg_ctx
            .renewal_threshold_secs_or(&workspace_manager.config_manager),
        // A read surface: an opportunity sizing is a display of the data, so a
        // rollup a cycle behind beats a warehouse scan.
        freshness: crate::server::preagg_context::RollupFreshness::ServeStale,
    };
    let default_config = oxy_airlayer_compat::engine::metric_tree_ops::DrillConfig::default();
    let config = oxy_airlayer_compat::engine::metric_tree_ops::DrillConfig {
        max_depth: req.max_depth.unwrap_or(default_config.max_depth),
        alpha: req.alpha.unwrap_or(default_config.alpha),
        root: req.root.clone(),
        ..default_config
    };

    let shared_for_exec = shared.clone();
    let result = tokio::task::spawn_blocking(move || {
        let executor = build_drill_query_executor(
            shared_for_exec,
            dialects,
            databases,
            workspace_manager,
            user.id,
            role,
            handle,
            preagg,
        );
        oxy_airlayer_compat::engine::metric_tree_ops::opportunity_drill(
            &tree,
            &shared,
            &req.target,
            &req.time_dimension,
            (req.period.0.as_str(), req.period.1.as_str()),
            &scope,
            &config,
            req.statistic,
            OPPORTUNITY_MIN_SUPPORT,
            &executor,
        )
    })
    .await
    .map_err(|e| MetricTreeError::Op(format!("drill task panicked: {e}")))?
    .map_err(|e| MetricTreeError::Op(e.to_string()))?;

    // A rate denominator only makes sense when the drill actually formed per-unit
    // rates — i.e. rows mode, which airlayer gates to a `supports_rate_basis`
    // target (a sum, or an eligible additive composite). `opportunity_drill`
    // returns `Some` for any additive root (its dimensions are populated in
    // value_share/equal mode too), so gating on `Some` alone would mislabel a raw
    // value as a rate. Require BOTH a result and a rate-basis target, mirroring
    // `post_opportunity`'s `weight_basis == "rows"` gate.
    let rate_denominator = result
        .as_ref()
        .filter(|_| target_supports_rate_basis)
        .and(denominator);
    Ok(Json(DrillResponse {
        result,
        rate_denominator,
    }))
}

// ── Discovery: time dimensions per view ─────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TimeDimensionsResponse {
    /// Map of view name → fully-qualified time-dimension ids
    /// (e.g. `orders.order_date`). Includes every `date` and `datetime`
    /// dimension declared on the view. Empty for views without one.
    pub by_view: BTreeMap<String, Vec<String>>,
}

/// `GET .../semantic/metric-tree/time-dimensions` — list valid time
/// dimensions per view. Lets clients (the metric-tree UI in particular)
/// drop hardcoded curated maps and discover what's actually queryable.
pub async fn get_time_dimensions(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
) -> Result<Json<TimeDimensionsResponse>, MetricTreeError> {
    use oxy_airlayer_compat::schema::models::DimensionType;

    let source = resolve_scan(&workspace_manager).await?;
    let layer = load_layer_at(&source.scan_path)?;
    let mut by_view: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for view in &layer.views {
        let mut dims: Vec<String> = view
            .dimensions
            .iter()
            .filter(|d| {
                matches!(
                    d.dimension_type,
                    DimensionType::Date | DimensionType::Datetime
                )
            })
            .map(|d| format!("{}.{}", view.name, d.name))
            .collect();
        dims.sort();
        by_view.insert(view.name.clone(), dims);
    }
    Ok(Json(TimeDimensionsResponse { by_view }))
}

// ── Distribution: structural decomposition at a single period ───────────────

#[derive(Debug, Deserialize)]
pub struct DistributionRequest {
    pub target: String,
    pub time_dimension: String,
    /// `[start, end]` inclusive — the period to value the tree at.
    pub period: (String, String),
}

/// `POST .../semantic/metric-tree/distribution` — distribution view of a
/// measure. Returns an `ExplainResult`-shaped payload (so the same client
/// renderers work), but with the baseline auto-derived as the equal-length
/// window immediately before `period`. Callers only specify the period of
/// interest; ignore the delta fields when rendering a pure distribution.
pub async fn post_distribution(
    WorkspaceManagerReadOnly(workspace_manager): WorkspaceManagerReadOnly,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    preagg_ctx: PreaggCacheCtx,
    engine_cache: SemanticEngineCacheCtx,
    Json(req): Json<DistributionRequest>,
) -> Result<Json<ExplainResult>, MetricTreeError> {
    use agentic_analytics::MetricTreeRunner as _;

    let baseline = derive_baseline_period(&req.period.0, &req.period.1)
        .ok_or_else(|| MetricTreeError::Op("invalid period dates (expected YYYY-MM-DD)".into()))?;

    // `source` owns the materialised tempdir; keep it alive for the whole run.
    let source = resolve_scan(&workspace_manager).await?;
    let config_manager = workspace_manager.config_manager.clone();
    // Resolved before the manager moves into the runner. What `source` actually
    // READ, not the request's pinned revision — those differ on a node that
    // holds a working copy and is pinned to a revision.
    let source_revision = source.source_revision(&workspace_manager);
    let runner = OxyMetricTreeRunner::new(workspace_manager, user.id, role)
        .with_scan_path(source.scan_path.clone())
        .with_engine_cache(engine_cache.cache.clone(), source_revision)
        .with_preagg(
            preagg_ctx.cache.clone(),
            preagg_ctx.renewal_threshold_secs_or(&config_manager),
        );
    let config = ExplainConfig::default();

    let result = tokio::time::timeout(
        EXPLAIN_TIMEOUT,
        runner.run_explain(
            req.target,
            req.time_dimension,
            (req.period.0.clone(), req.period.1.clone()),
            baseline,
            vec![],
            config,
        ),
    )
    .await
    .map_err(|_| {
        MetricTreeError::Op(format!(
            "distribution timed out after {}s — try narrowing the target measure",
            EXPLAIN_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| MetricTreeError::Op(e.to_string()))?;

    Ok(Json(result))
}

/// Compute an `(start, end)` window of the same inclusive day-count as
/// `period`, ending the day before `period.0`. Returns `None` if either
/// bound is not a valid `YYYY-MM-DD` date.
pub(crate) fn derive_baseline_period(start: &str, end: &str) -> Option<(String, String)> {
    use chrono::{Duration, NaiveDate};
    let start = NaiveDate::parse_from_str(start, "%Y-%m-%d").ok()?;
    let end = NaiveDate::parse_from_str(end, "%Y-%m-%d").ok()?;
    if end < start {
        return None;
    }
    let duration_days = (end - start).num_days();
    let baseline_end = start - Duration::days(1);
    let baseline_start = baseline_end - Duration::days(duration_days);
    Some((baseline_start.to_string(), baseline_end.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Omitting `statistic` must mean p75 — the documented default, and what
    /// every existing caller (which sends no such field) now gets.
    #[test]
    fn opportunity_request_defaults_the_statistic_to_p75() {
        let req: OpportunityRequest = serde_json::from_value(serde_json::json!({
            "target": "sales.net_revenue",
            "time_dimension": "sales.order_date",
            "period": ["2026-08-01", "2026-08-31"],
        }))
        .unwrap();
        assert_eq!(req.statistic, BenchmarkStatistic::P75);
    }

    #[test]
    fn opportunity_request_takes_the_callers_statistic() {
        for (wire, want) in [
            ("median", BenchmarkStatistic::Median),
            ("p75", BenchmarkStatistic::P75),
            ("best_peer", BenchmarkStatistic::BestPeer),
        ] {
            let req: OpportunityRequest = serde_json::from_value(serde_json::json!({
                "target": "sales.net_revenue",
                "time_dimension": "sales.order_date",
                "period": ["2026-08-01", "2026-08-31"],
                "statistic": wire,
            }))
            .unwrap();
            assert_eq!(req.statistic, want, "wire value `{wire}`");
        }
    }

    #[test]
    fn drill_request_defaults_the_statistic_to_p75() {
        let req: DrillRequest = serde_json::from_value(serde_json::json!({
            "target": "sales.net_revenue",
            "time_dimension": "sales.order_date",
            "period": ["2026-08-01", "2026-08-31"],
        }))
        .unwrap();
        assert_eq!(req.statistic, BenchmarkStatistic::P75);
    }

    #[test]
    fn an_unknown_statistic_is_refused_naming_the_valid_values() {
        let err = serde_json::from_value::<OpportunityRequest>(serde_json::json!({
            "target": "sales.net_revenue",
            "time_dimension": "sales.order_date",
            "period": ["2026-08-01", "2026-08-31"],
            "statistic": "mean",
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("median") && err.contains("p75") && err.contains("best_peer"),
            "{err}"
        );
    }

    use super::*;
    use std::collections::HashMap;

    /// A driver edge declaring no coefficient, so a fit has something to
    /// apply to and `predict` has something to refuse without one.
    fn qualitative_driver_tree() -> oxy_airlayer_compat::engine::metric_tree::MetricTree {
        let view = r#"
name: ops
table: public.ops
dialect: postgres
measures:
  - name: spend
    type: sum
    expr: spend
  - name: sales
    type: sum
    expr: sales
    drivers:
      - measure: ops.spend
        direction: positive
"#;
        let parsed = oxy_airlayer_compat::parse_view_yaml(view).unwrap();
        let layer = oxy_airlayer_compat::SemanticLayer::new(vec![parsed], None);
        oxy_semantic::build_metric_tree(&layer)
    }

    /// A `FittedDriver` from the fields a test cares about.
    ///
    /// Built through serde rather than as a struct literal: every field but
    /// `from`/`to` carries a serde default, so this stays honest as airlayer
    /// grows the shape (it has, twice) instead of breaking the build on
    /// fields no test here has an opinion about.
    fn fitted_driver(json: serde_json::Value) -> FittedDriver {
        serde_json::from_value(json).expect("fixture should match FittedDriver")
    }

    #[test]
    fn predict_request_accepts_a_baselines_fitted_coefficients_verbatim() {
        // The wire contract between the two endpoints: whatever `baseline`
        // serializes into `fitted`, `predict` must accept back as
        // `coefficients`. If these shapes drift, the UI silently loses every
        // fitted edge and the canvas goes inert with no error anywhere.
        let fitted = vec![fitted_driver(serde_json::json!({
            "from": "ops.spend",
            "to": "ops.sales",
            "lag": 7,
            "n": 8592,
            "n_panels": 24,
            "coefficient": 5.78,
            "se": 0.16,
            "t_stat": 36.47,
        }))];
        // Serialized from the real struct, so this exercises `baseline`'s
        // actual output shape rather than the JSON the fixture was written in.
        let response_json = serde_json::to_value(&fitted).unwrap();
        let body = serde_json::json!({
            "changes": [{ "measure": "ops.spend", "delta": 100.0 }],
            "coefficients": response_json,
        });

        let req: PredictRequest = serde_json::from_value(body).unwrap();
        assert_eq!(req.coefficients.len(), 1);
        assert_eq!(req.coefficients[0].coefficient, Some(5.78));
        assert_eq!(req.coefficients[0].lag, Some(7));
    }

    #[test]
    fn a_fitted_coefficient_makes_a_qualitative_edge_propagate() {
        let mut tree = qualitative_driver_tree();
        // Untouched, the edge carries no magnitude and nothing moves.
        let before = oxy_semantic::predict(&tree, &[("ops.spend".to_string(), 100.0)]).unwrap();
        assert!(before.impacts.is_empty());

        apply_fitted_coefficients(
            &mut tree,
            &[fitted_driver(serde_json::json!({
                "from": "ops.spend",
                "to": "ops.sales",
                "n": 100,
                "n_panels": 5,
                "coefficient": 3.0,
                "se": 0.1,
                "t_stat": 30.0,
            }))],
        );
        let after = oxy_semantic::predict(&tree, &[("ops.spend".to_string(), 100.0)]).unwrap();
        let sales = after
            .impacts
            .iter()
            .find(|i| i.measure == "ops.sales")
            .expect("the fitted edge propagates");
        assert_eq!(sales.estimated_delta, 300.0);
        // Fitted or declared, an observational coefficient is an estimate.
        assert_eq!(sales.confidence, "estimated");
    }

    #[test]
    fn a_refusal_echoed_into_predict_leaves_the_edge_inert() {
        // The client echoes back the whole `fitted` array, refusals included.
        // Treating a refusal as a coefficient would turn "we could not size
        // this" into a forecast of exactly zero — the lie this whole feature
        // is built to avoid.
        let mut tree = qualitative_driver_tree();
        apply_fitted_coefficients(
            &mut tree,
            &[fitted_driver(serde_json::json!({
                "from": "ops.spend",
                "to": "ops.sales",
                "n": 9600,
                "n_panels": 24,
                "se": 0.35,
                "t_stat": 0.51,
                "refusal": "no reliable relationship in this window",
            }))],
        );
        let result = oxy_semantic::predict(&tree, &[("ops.spend".to_string(), 100.0)]).unwrap();
        assert!(result.impacts.is_empty());
    }

    // `reject_lever_conflicts` is the seam `post_predict` calls before it
    // dispatches to `predict`/`predict_with_values` — the enforcing copy of
    // the check the browser client already runs in `leverConflicts.ts`.
    // Neither `post_predict` handler (this one or the `projects/` twin) can
    // run in a unit test: both start from a `WorkspaceManagerReadOnly` /
    // `enter_semantic_boundary` extractor that resolves a compiled revision
    // from Postgres, so there's no way to drive them without a database.
    // These tests instead pin down the one seam that carries the actual
    // decision, the same way this file already tests `classify_unvalued`
    // and `baseline_note` as pure functions rather than through the
    // handlers that call them.

    #[test]
    fn reject_lever_conflicts_allows_independent_levers() {
        // `ops.spend` drives `ops.sales`; a second, unrelated lever pinned
        // alongside it is not a conflict — same fixture, different question
        // than `a_fitted_coefficient_makes_a_qualitative_edge_propagate`.
        let view = r#"
name: ops
table: public.ops
dialect: postgres
measures:
  - name: spend
    type: sum
    expr: spend
  - name: headcount
    type: sum
    expr: headcount
"#;
        let parsed = oxy_airlayer_compat::parse_view_yaml(view).unwrap();
        let layer = oxy_airlayer_compat::SemanticLayer::new(vec![parsed], None);
        let tree = oxy_semantic::build_metric_tree(&layer);
        let changes = vec![
            ("ops.spend".to_string(), 100.0),
            ("ops.headcount".to_string(), 1.0),
        ];
        assert!(reject_lever_conflicts(&tree, &changes).is_ok());
    }

    #[test]
    fn reject_lever_conflicts_refuses_a_driver_and_its_target_pinned_together() {
        // Pinning `ops.spend` (upstream) and `ops.sales` (downstream, reachable
        // from `ops.spend` via the driver edge) together is exactly the
        // ambiguity `oxy_semantic::lever_conflicts` exists to catch: does
        // `ops.sales` hold at the pinned value despite the spend change, or
        // move to the value the driver relationship implies? Before this seam
        // existed, `post_predict` would silently pick one reading — this test
        // is what a curl/oxyc/agentic caller sees fixed.
        let tree = qualitative_driver_tree();
        let changes = vec![
            ("ops.spend".to_string(), 100.0),
            ("ops.sales".to_string(), 50.0),
        ];
        let err = reject_lever_conflicts(&tree, &changes).expect_err("upstream/downstream overlap");
        // The message must name the pair so a non-browser caller (no UI to
        // read a conflicts list from) can act on it from the error text alone.
        assert!(
            err.contains("ops.spend"),
            "message should name upstream: {err}"
        );
        assert!(
            err.contains("ops.sales"),
            "message should name downstream: {err}"
        );
    }

    #[test]
    fn reject_lever_conflicts_does_not_flag_the_same_lever_pinned_twice() {
        // Deliberately distinct from the conflict case above: a duplicate
        // lever id is one pinned value restated, not two readings in tension.
        // `lever_conflicts` already dedupes internally — this pins down that
        // the wiring here doesn't turn that dedup into a spurious refusal.
        let tree = qualitative_driver_tree();
        let changes = vec![
            ("ops.spend".to_string(), 100.0),
            ("ops.spend".to_string(), 100.0),
        ];
        assert!(reject_lever_conflicts(&tree, &changes).is_ok());
    }

    #[test]
    fn baseline_response_omits_fitted_when_there_was_nothing_to_fit() {
        // The common case — every reachable driver edge already declares a
        // coefficient — must not add a key to a response every client parses.
        let response = BaselineResponse {
            values: HashMap::new(),
            unvalued: vec![],
            resolved_period: ("2026-01-01".to_string(), "2026-03-31".to_string()),
            baseline_note: None,
            fitted: vec![],
        };
        let json = serde_json::to_value(&response).unwrap();
        assert!(json.get("fitted").is_none());
    }

    fn chain_tree() -> oxy_airlayer_compat::engine::metric_tree::MetricTree {
        let view = r#"
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
"#;
        let parsed = oxy_airlayer_compat::parse_view_yaml(view).unwrap();
        let layer = oxy_airlayer_compat::SemanticLayer::new(vec![parsed], None);
        oxy_semantic::build_metric_tree(&layer)
    }

    #[test]
    fn a_failed_query_marks_every_reachable_node() {
        let tree = chain_tree();
        let unvalued = classify_unvalued(
            &tree,
            &["orders.revenue".to_string()],
            &HashMap::new(),
            &BaselineOutcome::ExecutorError("connection refused".to_string()),
            &[],
        );
        assert!(
            unvalued
                .iter()
                .all(|u| u.reason == UnvaluedReason::QueryFailed)
        );
        // revenue itself plus profit downstream of it.
        assert!(unvalued.iter().any(|u| u.node_id == "orders.revenue"));
        assert!(unvalued.iter().any(|u| u.node_id == "orders.profit"));
    }

    #[test]
    fn a_missing_node_in_a_successful_query_is_no_rows() {
        let tree = chain_tree();
        let values = HashMap::from([("orders.revenue".to_string(), 100.0)]);
        let unvalued = classify_unvalued(
            &tree,
            &["orders.revenue".to_string()],
            &values,
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            &[],
        );
        assert_eq!(unvalued.len(), 1);
        assert_eq!(unvalued[0].node_id, "orders.profit");
        assert_eq!(unvalued[0].reason, UnvaluedReason::NoRowsInWindow);
    }

    #[test]
    fn a_skipped_view_outranks_a_valued_outcome() {
        // The case grouping introduced: one view answers, another was never
        // asked. The outcome says `Valued` for both, so without the per-view
        // override every node in the skipped view would read "no rows in
        // window" — advice about a window that was never applied to it.
        let tree = chain_tree();
        let values = HashMap::from([("orders.revenue".to_string(), 100.0)]);
        let unvalued = classify_unvalued(
            &tree,
            &["orders.revenue".to_string()],
            &values,
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            &[SkippedGroup {
                view: "orders".to_string(),
                nodes: vec!["orders.profit".to_string()],
                reason: "no time dimension".to_string(),
                kind: SkipKind::NotQueried,
            }],
        );
        assert_eq!(unvalued.len(), 1);
        assert_eq!(unvalued[0].node_id, "orders.profit");
        assert_eq!(unvalued[0].reason, UnvaluedReason::NotQueried);
    }

    #[test]
    fn a_view_whose_query_errored_is_not_a_view_never_asked() {
        let tree = chain_tree();
        let values = HashMap::from([("orders.revenue".to_string(), 100.0)]);
        let skipped = [SkippedGroup {
            view: "orders".to_string(),
            nodes: vec!["orders.profit".to_string()],
            reason: "connection refused".to_string(),
            kind: SkipKind::QueryFailed,
        }];
        let unvalued = classify_unvalued(
            &tree,
            &["orders.revenue".to_string()],
            &values,
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            &skipped,
        );
        assert_eq!(unvalued[0].reason, UnvaluedReason::QueryFailed);
        // And the note says so, next to nothing about the window.
        let note = baseline_note(
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            "orders.order_date",
            &skipped,
        )
        .expect("a failed view has something to say even under a valued outcome");
        assert!(note.contains("connection refused"), "{note}");
        assert!(!note.contains("longer window"), "{note}");
    }

    /// Views refused for the same reason say it once.
    ///
    /// Every view a tree reaches but cannot window is skipped for the SAME
    /// reason, so a per-view sentence repeats that reason verbatim N times. On
    /// a real workspace that filled the panel with one paragraph restated for
    /// nine airhouse views. The reason belongs in the code; the panel needs
    /// the list and the fact.
    #[test]
    fn views_refused_for_one_reason_are_named_together() {
        let reason = "no `sales_daily.business_date` to anchor the window on";
        let skipped: Vec<SkippedGroup> = ["daily_operations", "quickbooks_pl"]
            .iter()
            .map(|view| SkippedGroup {
                view: view.to_string(),
                nodes: vec![format!("{view}.something")],
                reason: reason.to_string(),
                kind: SkipKind::NotQueried,
            })
            .collect();

        let note = baseline_note(
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            "sales_daily.business_date",
            &skipped,
        )
        .expect("an unread view has something to say");

        assert_eq!(
            note,
            "`daily_operations`, `quickbooks_pl` were not read: no \
             `sales_daily.business_date` to anchor the window on"
        );
        // Said once, not once per view.
        assert_eq!(note.matches("anchor the window on").count(), 1, "{note}");
    }

    #[test]
    fn a_single_unread_view_reads_as_one_view() {
        let skipped = [SkippedGroup {
            view: "quickbooks_pl".to_string(),
            nodes: vec!["quickbooks_pl.net_income".to_string()],
            reason: "no `sales_daily.business_date` to anchor the window on".to_string(),
            kind: SkipKind::NotQueried,
        }];
        let note = baseline_note(
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            "sales_daily.business_date",
            &skipped,
        )
        .expect("an unread view has something to say");
        assert_eq!(
            note,
            "`quickbooks_pl` was not read: no `sales_daily.business_date` to anchor the \
             window on"
        );
    }

    #[test]
    fn a_fully_valued_reachable_set_reports_nothing() {
        let tree = chain_tree();
        let values = HashMap::from([
            ("orders.revenue".to_string(), 100.0),
            ("orders.profit".to_string(), 40.0),
        ]);
        let unvalued = classify_unvalued(
            &tree,
            &["orders.revenue".to_string()],
            &values,
            &BaselineOutcome::Valued {
                unreadable: Vec::new(),
            },
            &[],
        );
        assert!(unvalued.is_empty());
    }

    #[test]
    fn rows_that_carried_no_measures_are_not_an_empty_window() {
        // The two name opposite fixes — lengthen the window versus pick a time
        // dimension the pinned measure's view actually has — and folding this
        // into `NoRowsInWindow` made every node contradict the banner over it,
        // which says the rows were there.
        let tree = chain_tree();
        let unvalued = classify_unvalued(
            &tree,
            &["orders.revenue".to_string()],
            &HashMap::new(),
            &BaselineOutcome::NoMatchingColumns,
            &[],
        );
        assert!(!unvalued.is_empty());
        assert!(
            unvalued
                .iter()
                .all(|u| u.reason == UnvaluedReason::NoMatchingColumns),
            "{unvalued:?}"
        );
    }

    /// The handler source, excluding this test module — which necessarily
    /// names the very symbols the tests below assert are absent.
    fn handler_src() -> &'static str {
        include_str!("metric_tree.rs")
            .split("\n#[cfg(test)]")
            .next()
            .expect("source splits on its own test-module attribute")
    }

    /// The handlers must resolve their scan root through the compile boundary,
    /// never from the workspace working copy — a stateless serve replica has
    /// none, and reading it there 500'd every metric-tree call on every
    /// workspace (oxy-hq/oxygen#878). `load_layer_sync` is gone, but
    /// `config_manager.semantics_scan_path()` is still one autocomplete away,
    /// and it compiles fine — it just fails in cloud. So guard the shape, in
    /// the spirit of `tests/authz/authz_boundaries.rs`.
    #[test]
    fn handlers_never_scan_the_working_copy() {
        assert!(
            !handler_src().contains("semantics_scan_path"),
            "metric_tree.rs must resolve its scan root via resolve_query_scan_source \
             (compile boundary first), not config_manager.semantics_scan_path()"
        );
    }

    /// Parsed from YAML rather than built field-by-field, so the fixture is
    /// bound to the real view schema instead of a hand-rolled approximation.
    fn layer(yaml: &str) -> oxy_airlayer_compat::SemanticLayer {
        serde_yaml::from_str(yaml).expect("fixture layer should parse")
    }

    #[test]
    fn count_measure_id_names_the_denominator_rates_are_formed_against() {
        let l = layer(
            r#"
views:
  - name: orders
    measures:
      - name: gross_revenue
        type: sum
        expr: total_amount
      - name: total_orders
        type: count
"#,
        );

        // The sum is skipped: only a `count` can serve as the volume denominator.
        assert_eq!(
            count_measure_id(&l, "orders").as_deref(),
            Some("orders.total_orders")
        );
    }

    #[test]
    fn count_measure_id_takes_the_first_count_as_the_engine_does() {
        // The engine divides by the FIRST declared count, so reporting any other
        // would name a denominator it never used — the exact confusion this
        // field exists to prevent.
        let l = layer(
            r#"
views:
  - name: orders
    measures:
      - name: completed_orders
        type: count
      - name: total_orders
        type: count
"#,
        );

        assert_eq!(
            count_measure_id(&l, "orders").as_deref(),
            Some("orders.completed_orders")
        );
    }

    #[test]
    fn count_measure_id_is_absent_when_a_view_declares_no_count() {
        // Mirrors the engine refusing to size: no count, no rate, no denominator
        // to name.
        let l = layer(
            r#"
views:
  - name: orders
    measures:
      - name: gross_revenue
        type: sum
        expr: total_amount
"#,
        );

        assert_eq!(count_measure_id(&l, "orders"), None);
        assert_eq!(count_measure_id(&l, "nonexistent_view"), None);
    }

    fn sized_result(weight_basis: &str) -> OpportunityResult {
        OpportunityResult {
            target: "orders.gross_revenue".into(),
            period: ("2026-04-15".into(), "2026-07-13".into()),
            overall_value: 640_000.0,
            direction: oxy_airlayer_compat::schema::models::MeasureDirection::HigherIsBetter,
            weight_basis: weight_basis.into(),
            dimensions: vec![],
            skipped_dimensions: vec![],
            downstream: vec![],
        }
    }

    #[test]
    fn response_flattens_the_denominator_alongside_the_engine_result() {
        // The client reads `rate_denominator` as a sibling of `weight_basis`; a
        // nested `result` object would silently break every field of the panel.
        let json = serde_json::to_value(OpportunityResponse {
            result: sized_result("rows"),
            rate_denominator: Some("orders.total_orders".into()),
        })
        .unwrap();

        assert_eq!(json["weight_basis"], "rows");
        assert_eq!(json["rate_denominator"], "orders.total_orders");
        assert_eq!(json["target"], "orders.gross_revenue");
        assert!(json.get("result").is_none(), "must not nest under `result`");
    }

    #[test]
    fn response_omits_the_denominator_when_no_rate_was_formed() {
        // In value_share/equal mode `current_value` is a raw measure value, not a
        // rate. Naming a denominator would invite reading it as one.
        let json = serde_json::to_value(OpportunityResponse {
            result: sized_result("equal"),
            rate_denominator: None,
        })
        .unwrap();

        assert!(json.get("rate_denominator").is_none());
    }

    #[test]
    fn drill_response_wire_format_is_stable() {
        use oxy_airlayer_compat::engine::metric_tree_ops::{
            CandidateKind, DrillCandidate, DrillLevel, DrillResult, StopReason,
        };
        let resp = DrillResponse {
            result: Some(DrillResult {
                target: "orders.revenue".into(),
                root_gap: 500.0,
                root_upside: 276_000.0,
                benchmark_filter: vec![],
                levels: vec![DrillLevel {
                    measure: "orders.revenue".into(),
                    segment_filter: vec![],
                    gap: 500.0,
                    root_share: 1.0,
                    candidates: vec![DrillCandidate {
                        kind: CandidateKind::Dimension {
                            dimension: "orders.category".into(),
                            value: "sides".into(),
                        },
                        concentration: 0.9,
                        gap: 450.0,
                        gated: true,
                    }],
                    stop_reason: Some(StopReason::NoCandidates),
                }],
            }),
            rate_denominator: Some("orders.total_orders".into()),
        };
        let j = serde_json::to_value(&resp).unwrap();
        assert_eq!(j["rate_denominator"], "orders.total_orders");
        assert_eq!(j["target"], "orders.revenue"); // flattened, not nested under "result"
        let cand = &j["levels"][0]["candidates"][0];
        assert_eq!(cand["kind"]["Dimension"]["dimension"], "orders.category"); // externally tagged
        assert_eq!(cand["kind"]["Dimension"]["value"], "sides");
        assert_eq!(cand["gated"], true);
        assert_eq!(j["levels"][0]["stop_reason"], "NoCandidates"); // unit variant → string
    }

    #[test]
    fn drill_request_accepts_an_optional_root() {
        // Absent -> None: the top-pick path the panel uses before any row is expanded.
        let without: DrillRequest = serde_json::from_value(serde_json::json!({
            "target": "orders.revenue",
            "time_dimension": "orders.created_at",
            "period": ["2024-01-01", "2024-03-31"],
        }))
        .expect("root is optional");
        assert!(without.root.is_none());

        // Present -> the named row, verbatim.
        let with: DrillRequest = serde_json::from_value(serde_json::json!({
            "target": "orders.revenue",
            "time_dimension": "orders.created_at",
            "period": ["2024-01-01", "2024-03-31"],
            "root": { "dimension": "orders.channel", "segment": "mobile_app" },
        }))
        .expect("root must deserialize");
        let root = with.root.expect("root present");
        assert_eq!(root.dimension, "orders.channel");
        assert_eq!(root.segment, "mobile_app");
    }

    #[test]
    fn drill_response_none_serializes_without_result_fields() {
        // The frontend treats "no `levels` key" as nothing-to-drill. A None
        // result must flatten to an object carrying NEITHER the DrillResult
        // fields NOR rate_denominator, so the panel's empty-state branch fires.
        let resp = DrillResponse {
            result: None,
            rate_denominator: None,
        };
        let j = serde_json::to_value(&resp).unwrap();
        assert!(
            j.get("levels").is_none(),
            "None must not emit `levels`: {j}"
        );
        assert!(
            j.get("target").is_none(),
            "None must not emit `target`: {j}"
        );
        assert!(
            j.get("rate_denominator").is_none(),
            "a None denominator must be skipped: {j}"
        );
    }

    // ── The baseline query budget ───────────────────────────────────────────

    /// Executor calls `baseline_reads` issues for a forward-reachable set
    /// spanning `views` views — `V(V+3)/2`, derived in
    /// [`BASELINE_QUERY_BUDGET`]: one `grouped_values` request per view, one
    /// `grouped_fit` panel request per single-view group, and one more per
    /// cross-view pair.
    ///
    /// A floor, not a ceiling: any of these may split again into one query per
    /// (view, additivity) group. Sizing the budget against the floor is the
    /// conservative direction — the real fan-out is only ever wider.
    fn baseline_executor_calls(views: u32) -> u32 {
        views + views + views * views.saturating_sub(1) / 2
    }

    /// Views in `example_new/semantics/views` — the workspace this repo's own
    /// metric-tree fixtures and `tests/platform/metric_tree_fit_panel` run
    /// against. Read from disk rather than hard-coded so adding a seventh view
    /// re-derives the budget's reference fan-out instead of silently
    /// invalidating it.
    fn reference_view_count() -> u32 {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../example_new/semantics/views");
        std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".view.yml"))
            .count() as u32
    }

    #[test]
    fn the_query_budget_and_its_headroom_partition_the_baseline_timeout() {
        // Not decoration: the budget's whole claim is that what it does not
        // spend is still available to the in-flight query and the assembly
        // after it. A headroom that no longer complements the timeout — either
        // constant edited alone — makes that claim false without failing
        // anything else.
        assert_eq!(
            BASELINE_QUERY_BUDGET + BASELINE_QUERY_HEADROOM,
            BASELINE_TIMEOUT,
            "the query budget and its headroom must partition BASELINE_TIMEOUT"
        );
        assert!(
            BASELINE_QUERY_BUDGET < BASELINE_TIMEOUT,
            "a budget that reaches BASELINE_TIMEOUT stops reserving anything for \
             the query already in flight — which the outer tokio::time::timeout \
             cannot cancel, because it wraps a spawn_blocking"
        );
        assert!(
            BASELINE_QUERY_HEADROOM >= std::time::Duration::from_secs(10),
            "the headroom is the allowance for one uncancellable warehouse query \
             plus response assembly; below ~10s it stops covering a cold round trip"
        );
    }

    #[test]
    fn the_query_budget_does_not_refuse_the_reference_workspaces_fan_out() {
        // The regression this pins. The deadline used to gate only the
        // additivity-split retry — a handful of queries — so BASELINE_TIMEOUT/2
        // was ample. It now gates EVERY query the executor serves, and the same
        // 15s had to cover the whole V(V+3)/2 fan-out: ~550ms per warehouse
        // round trip on this repo's own six-view fixture, which is under a cold
        // round trip. At that point the budget, not the warehouse, is what fails
        // the read — partial values and BUDGET_SPENT where a 20s read used to
        // succeed.
        //
        // 700ms is the floor a per-call allowance has to clear to be a bound on
        // a runaway fan-out rather than a cap on how wide a workspace may be. It
        // is deliberately modest: a windowed aggregate is a few hundred
        // milliseconds warm and seconds cold, so this does not promise the read
        // finishes — only that the budget is not the first thing to give up.
        const MIN_PER_CALL: std::time::Duration = std::time::Duration::from_millis(700);

        let views = reference_view_count();
        let calls = baseline_executor_calls(views);
        assert_eq!(
            (views, calls),
            (6, 27),
            "example_new's shape changed; re-derive BASELINE_QUERY_BUDGET rather \
             than just updating this number"
        );
        assert!(
            BASELINE_QUERY_BUDGET >= MIN_PER_CALL * calls,
            "BASELINE_QUERY_BUDGET is {:?}, which across the reference workspace's \
             {calls} executor calls ({views} views, V(V+3)/2) leaves {:?} per \
             warehouse round trip — under the {MIN_PER_CALL:?} floor. The budget \
             would refuse queries a read that fits inside BASELINE_TIMEOUT still \
             needs.",
            BASELINE_QUERY_BUDGET,
            BASELINE_QUERY_BUDGET / calls,
        );
    }

    #[test]
    fn both_baseline_surfaces_take_the_budget_from_one_place() {
        // `metric_tree_projection` carried its own `BASELINE_TIMEOUT / 2`. Two
        // handlers hitting the same warehouse with two silently diverging
        // deadlines is exactly what `BASELINE_TIMEOUT` was made `pub(crate)` to
        // prevent, and the fraction escaped that rule by being written out
        // longhand at each call site.
        //
        // Comment lines are skipped, or this gate matches the very prose above
        // that explains why the fraction is gone — the same trap
        // `authz::shared_db_registry` documents. A gate that counts prose is a
        // gate that fires for the wrong reason.
        for (name, src) in [
            ("metric_tree.rs", include_str!("metric_tree.rs")),
            (
                "metric_tree_projection.rs",
                include_str!("metric_tree_projection.rs"),
            ),
        ] {
            let offender = src
                .split("\n#[cfg(test)]")
                .next()
                .expect("source splits on its own test-module attribute")
                .lines()
                .find(|l| !l.trim_start().starts_with("//") && l.contains("BASELINE_TIMEOUT /"));
            assert_eq!(
                offender, None,
                "{name} re-derives the baseline query budget as a fraction of \
                 BASELINE_TIMEOUT at the call site. It is BASELINE_QUERY_BUDGET, \
                 reached through baseline_query_deadline(), so the two baseline \
                 surfaces cannot be tuned apart."
            );
        }
    }
}
