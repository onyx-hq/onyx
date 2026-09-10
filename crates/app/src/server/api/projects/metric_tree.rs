//! `/api/projects/{project_id}/semantic/metric-tree*` — metric-tree
//! analysis ops for customer-app bundles (drivers / what-if / RCA /
//! opportunity sizing).
//!
//! These mirror the IDE's workspace-scoped `/semantic/metric-tree*`
//! handlers ([`crate::server::api::metric_tree`]) but enter through the
//! customer-app gate and load the semantic model from the compile boundary
//! (via [`super::semantic_boundary`]) rather than the workspace FS. Request /
//! response shapes are the exact same types the IDE endpoints use — the SDK
//! and the IDE never drift.
//!
//! The scenario projection is the same surface, in the sibling module
//! [`super::metric_tree_projection`] — split out only for file size.
//!
//! **Fleet split (see `role_manifest.rs`).** Pure ops (tree, sensitivity,
//! predict, time-dimensions) need only the parsed layer, so they run on the
//! stateless serve fleet where published apps live (`FleetOk`). Query-executing
//! ops (explain, opportunity, distribution) run through [`OxyMetricTreeRunner`]:
//! the scan-path override pins the *layer* to the materialised boundary tempdir,
//! but the warehouse *connector* is still built from the customer-app
//! WorkspaceManager's config (`OxyProjectContext::build_connector_for`), which
//! resolves via the FS fallback — empty `databases: []` on a serve replica with
//! no working copy. So the query-executing ops are pinned `IdeOnly`, same as
//! `/query` and `/semantic-query`. The boundary guard is held for the whole
//! request so the tempdir outlives the run.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use entity::workspace_members::WorkspaceRole;
use oxy_airlayer_compat::schema::models::DimensionType;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::agentic_wiring::metric_tree_runner::{OxyMetricTreeRunner, build_query_executor};
use crate::server::api::custom_apps_gates::parse_versioned_body;
use crate::server::api::metric_tree::{
    // `BASELINE_TIMEOUT` is THE workspace handler's constant, not a copy of it:
    // both surfaces query the same warehouse, so the deadline is one number in
    // one place rather than two that can be tuned apart.
    self as mt,
    BASELINE_TIMEOUT,
    DistributionRequest,
    ExplainRequest,
    OpportunityRequest,
    PredictRequest,
    TimeDimensionsResponse,
    TreeQuery,
};
use crate::server::api::projects::semantic_boundary::{
    SemanticBoundary, cache_lookup, cache_store, enter_semantic_boundary, err_with_code,
    load_layer, wants_refresh,
};
use crate::server::router::AppState;

/// Soft cap on explain/distribution — matches the workspace handler. A rich
/// schema can fire 50+ warehouse queries; failing loud beats hanging.
const EXPLAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// Enter the boundary and parse the layer — the pure-op prelude.
async fn boundary_with_layer(
    headers: &HeaderMap,
    project_id: Uuid,
) -> Result<(SemanticBoundary, oxy_airlayer_compat::SemanticLayer), Response> {
    let boundary = enter_semantic_boundary(headers, project_id).await?;
    let layer = load_layer(boundary.scan.path_buf()).await?;
    Ok((boundary, layer))
}

/// Build a read-only runner pinned to the boundary's materialised scan path.
/// The caller must keep `boundary` alive for the whole run (tempdir guard).
///
/// The rollup short-circuit is attached here rather than at each call site so
/// every query-executing op on this surface answers from the same tier as its
/// IDE twin in [`crate::server::api::metric_tree`] — the two render the same
/// analysis, and a bundle reading a measure the IDE serves from a rollup should
/// not silently pay for a warehouse scan. These ops are `IdeOnly`
/// (`role_manifest`), so the node running them is the one holding the Layer-1
/// cache; `preagg_context` and `try_resolve_preagg` both fall through to the
/// warehouse when there is no cache or no covering rollup.
///
/// The threshold resolves from THIS workspace's own
/// `pre_aggregations.refresh_worker.renewal_threshold` when the process carries
/// no global value — same rule as `workspace_context` and `semantic_query`, so
/// the read side and the rebuild side never disagree about one setting.
fn runner_for(boundary: &SemanticBoundary, app_state: &AppState) -> OxyMetricTreeRunner {
    let workspace_manager = boundary.proj_ctx.workspace_manager().clone();
    let preagg_ctx = crate::server::api::middlewares::workspace_context::PreaggCacheCtx {
        cache: app_state.preagg_cache.clone(),
        renewal_threshold_secs: app_state.preagg_renewal_threshold_secs,
    };
    let renewal_threshold_secs =
        preagg_ctx.renewal_threshold_secs_or(&workspace_manager.config_manager);
    OxyMetricTreeRunner::new(
        workspace_manager.into_read_only(),
        boundary.app.user.id,
        WorkspaceRole::Viewer,
    )
    .with_scan_path(boundary.scan.path_buf())
    .with_preagg(preagg_ctx.cache, renewal_threshold_secs)
}

// ── Pure ops ────────────────────────────────────────────────────────────────

/// `GET .../metric-tree` — the full tree or `?root=<id>` subtree.
pub async fn get_metric_tree(
    Path(project_id): Path<Uuid>,
    Query(q): Query<TreeQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    // Gate first, THEN read the cache: a cached hit must never bypass the
    // customer-app authorization gates.
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let key = q.root.clone().unwrap_or_default();
    if let Some(hit) = cache_lookup(project_id, "mt-tree", &key, wants_refresh(uri.query())) {
        return hit;
    }
    let layer = match load_layer(boundary.scan.path_buf()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let tree = oxy_semantic::build_metric_tree(&layer);
    let out = match q.root {
        Some(root) => match oxy_semantic::subtree(&tree, &root) {
            Some(sub) => sub,
            None => {
                return err_with_code(
                    StatusCode::NOT_FOUND,
                    format!("measure '{root}' not in tree"),
                    "measure_not_found",
                );
            }
        },
        None => tree,
    };
    cache_store(boundary.project_id(), "mt-tree", &key, &out)
}

/// `GET .../metric-tree/{measure_id}/sensitivity` — ranked drivers.
pub async fn get_sensitivity(
    Path((project_id, measure_id)): Path<(Uuid, String)>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    // Gate before cache read (authorization must gate cached hits too).
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    if let Some(hit) = cache_lookup(
        project_id,
        "mt-sens",
        &measure_id,
        wants_refresh(uri.query()),
    ) {
        return hit;
    }
    let layer = match load_layer(boundary.scan.path_buf()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let tree = oxy_semantic::build_metric_tree(&layer);
    match oxy_semantic::sensitivity(&tree, &measure_id) {
        Ok(res) => cache_store(boundary.project_id(), "mt-sens", &measure_id, &res),
        Err(e) => err_with_code(StatusCode::BAD_REQUEST, e.to_string(), "sensitivity_failed"),
    }
}

/// `POST .../metric-tree/predict` — propagate hypothetical deltas.
pub async fn post_predict(
    Path(project_id): Path<Uuid>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // `_boundary` (the tempdir guard) is held until the layer is parsed;
    // `predict` then operates on the in-memory tree, so it may drop after.
    let (_boundary, layer) = match boundary_with_layer(&headers, project_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let req: PredictRequest = match parse_versioned_body(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let mut tree = oxy_semantic::build_metric_tree(&layer);
    // A fitted coefficient must behave exactly as a declared one, so this
    // lands before propagation — same ordering as the workspace twin.
    oxy_airlayer_compat::engine::metric_tree_fit::apply_fitted_coefficients(
        &mut tree,
        &req.coefficients,
    );
    let changes: Vec<(String, f64)> = req
        .changes
        .into_iter()
        .map(|c| (c.measure, c.delta))
        .collect();
    // Same enforcement as the workspace twin's `post_predict`, and for the
    // same reason: a custom app's Oxy Function or a scheduled run hits this
    // route with no browser client in front of it to run `leverConflicts.ts`
    // first, so refusing an ambiguous pinned-lever pair has to happen here
    // rather than being assumed from the UI.
    if let Err(message) = mt::reject_lever_conflicts(&tree, &changes) {
        return err_with_code(StatusCode::BAD_REQUEST, message, "lever_conflict");
    }
    let result = match req.values {
        Some(values) => oxy_semantic::predict_with_values(&tree, &changes, &values),
        None => oxy_semantic::predict(&tree, &changes),
    };
    match result {
        Ok(res) => axum::Json(res).into_response(),
        Err(e) => err_with_code(StatusCode::BAD_REQUEST, e.to_string(), "predict_failed"),
    }
}

/// `GET .../metric-tree/time-dimensions` — valid time dims per view.
pub async fn get_time_dimensions(Path(project_id): Path<Uuid>, headers: HeaderMap) -> Response {
    let (_boundary, layer) = match boundary_with_layer(&headers, project_id).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
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
    axum::Json(TimeDimensionsResponse { by_view }).into_response()
}

// ── Query-executing ops ──────────────────────────────────────────────────────

/// `POST .../metric-tree/explain` — period-over-period root cause.
pub async fn post_explain(
    Path(project_id): Path<Uuid>,
    uri: Uri,
    headers: HeaderMap,
    State(app_state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    use agentic_analytics::MetricTreeRunner as _;
    // Gate before cache read so a cached hit can't bypass authorization.
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let key = String::from_utf8_lossy(&body).into_owned();
    if let Some(hit) = cache_lookup(project_id, "mt-explain", &key, wants_refresh(uri.query())) {
        return hit;
    }
    let req: ExplainRequest = match parse_versioned_body(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let runner = runner_for(&boundary, &app_state);
    let config = mt::explain_config(req.config);
    let result = tokio::time::timeout(
        EXPLAIN_TIMEOUT,
        runner.run_explain(
            req.target,
            req.time_dimension,
            (req.current_period.0, req.current_period.1),
            (req.previous_period.0, req.previous_period.1),
            // Population-wide: this surface has no segment scope of its own.
            vec![],
            config,
        ),
    )
    .await;
    match result {
        Ok(Ok(r)) => cache_store(boundary.project_id(), "mt-explain", &key, &r),
        Ok(Err(e)) => err_with_code(StatusCode::BAD_REQUEST, e.to_string(), "explain_failed"),
        Err(_) => err_with_code(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "explain timed out after {}s — narrow the target measure",
                EXPLAIN_TIMEOUT.as_secs()
            ),
            "explain_timeout",
        ),
    }
}

/// `POST .../metric-tree/opportunity` — segment opportunity sizing.
pub async fn post_opportunity(
    Path(project_id): Path<Uuid>,
    uri: Uri,
    headers: HeaderMap,
    State(app_state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    use agentic_analytics::MetricTreeRunner as _;
    // Gate before cache read so a cached hit can't bypass authorization.
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let key = String::from_utf8_lossy(&body).into_owned();
    if let Some(hit) = cache_lookup(project_id, "mt-opp", &key, wants_refresh(uri.query())) {
        return hit;
    }
    let req: OpportunityRequest = match parse_versioned_body(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    // This surface sizes across the population; only the world-model instance
    // panel scopes a scan, and it does not come through here. Refuse rather than
    // drop the scope on the floor — silently returning population numbers to a
    // caller that asked about one instance is worse than saying no.
    if req.instance.is_some() {
        return err_with_code(
            StatusCode::BAD_REQUEST,
            "instance-scoped opportunity sizing is not available on this endpoint".to_string(),
            "opportunity_instance_unsupported",
        );
    }
    let runner = runner_for(&boundary, &app_state);
    match runner
        .run_opportunity(
            req.target,
            req.time_dimension,
            (req.period.0, req.period.1),
            req.statistic,
        )
        .await
    {
        Ok(r) => cache_store(boundary.project_id(), "mt-opp", &key, &r),
        Err(e) => err_with_code(StatusCode::BAD_REQUEST, e.to_string(), "opportunity_failed"),
    }
}

/// `POST .../metric-tree/distribution` — single-period distribution
/// (explain against an auto-derived immediately-prior baseline).
pub async fn post_distribution(
    Path(project_id): Path<Uuid>,
    uri: Uri,
    headers: HeaderMap,
    State(app_state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    use agentic_analytics::MetricTreeRunner as _;
    // Gate before cache read so a cached hit can't bypass authorization.
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let key = String::from_utf8_lossy(&body).into_owned();
    if let Some(hit) = cache_lookup(project_id, "mt-dist", &key, wants_refresh(uri.query())) {
        return hit;
    }
    let req: DistributionRequest = match parse_versioned_body(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let baseline = match mt::derive_baseline_period(&req.period.0, &req.period.1) {
        Some(b) => b,
        None => {
            return err_with_code(
                StatusCode::BAD_REQUEST,
                "invalid period dates (expected YYYY-MM-DD)",
                "invalid_period",
            );
        }
    };
    let runner = runner_for(&boundary, &app_state);
    let result = tokio::time::timeout(
        EXPLAIN_TIMEOUT,
        runner.run_explain(
            req.target,
            req.time_dimension,
            (req.period.0.clone(), req.period.1.clone()),
            baseline,
            // Population-wide: this surface has no segment scope of its own.
            vec![],
            Default::default(),
        ),
    )
    .await;
    match result {
        Ok(Ok(r)) => cache_store(boundary.project_id(), "mt-dist", &key, &r),
        Ok(Err(e)) => err_with_code(
            StatusCode::BAD_REQUEST,
            e.to_string(),
            "distribution_failed",
        ),
        Err(_) => err_with_code(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "distribution timed out after {}s",
                EXPLAIN_TIMEOUT.as_secs()
            ),
            "distribution_timeout",
        ),
    }
}

// ── Baseline (scenario simulation) ──────────────────────────────────────────

/// Resolve `req.instance` into an engine scope, `Response`-erroring instead
/// of `MetricTreeError`. Mirrors the workspace handler's inline scope
/// resolution in `crate::server::api::metric_tree::post_baseline` — same
/// rule: an instance that cannot be resolved is an error, never a silently
/// dropped scope.
fn baseline_scope(
    layer: &oxy_airlayer_compat::SemanticLayer,
    req: &mt::BaselineRequest,
) -> Result<Vec<oxy_airlayer_compat::engine::query::QueryFilter>, Response> {
    mt::baseline_scope_core(layer, req.instance.as_ref()).map_err(|message| {
        err_with_code(
            StatusCode::NOT_FOUND,
            message,
            "baseline_instance_not_found",
        )
    })
}

/// Build the executor and run the baseline's per-view value and per-group fit
/// queries (see `mt::BASELINE_TIMEOUT`), off the async runtime. Split out of
/// [`post_baseline`] to stay under the file's function-length budget.
///
/// Unlike the other query-executing ops on this surface, this does not go
/// through `OxyMetricTreeRunner` — the fan-out `mt::baseline_reads` drives
/// isn't part of the `MetricTreeRunner` trait — so the executor is built
/// directly here, the same way the workspace handler's `post_baseline` does.
async fn run_baseline_query(
    boundary: &SemanticBoundary,
    layer: oxy_airlayer_compat::SemanticLayer,
    req: &mt::BaselineRequest,
    scope: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
) -> Result<mt::BaselineReads, Response> {
    let databases = OxyMetricTreeRunner::list_databases_sync(boundary.proj_ctx.workspace_manager());
    let engine = std::sync::Arc::new(
        oxy_airlayer_compat::SemanticEngine::from_semantic_layer(
            layer.clone(),
            oxy_airlayer_compat::DatasourceDialectMap::from_config_databases(&databases),
        )
        .map_err(|e| {
            err_with_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
                "baseline_engine_build_failed",
            )
        })?,
    );
    let tree = oxy_semantic::build_metric_tree(&layer);
    // `build_query_executor` asks for the read-only capability: it needs the
    // workspace id, config and secrets, never the disk. Downgrading here is
    // what states that, rather than handing it a working copy it must not use.
    let workspace_manager = boundary
        .proj_ctx
        .workspace_manager()
        .clone()
        .into_read_only();
    let user_id = boundary.app.user.id;
    let handle = tokio::runtime::Handle::current();
    // This surface pins the layer to the compile-boundary tempdir and carries
    // no pre-aggregation cache of its own, so the short-circuit is off and the
    // other two fields are inert; `freshness` is stated anyway so a later
    // caller that DOES attach a cache inherits the read-surface posture.
    let preagg = crate::agentic_wiring::metric_tree_runner::RunnerPreagg {
        cache: None,
        renewal_threshold_secs: 120,
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
                // Customer-app requests carry no workspace role of their own
                // to read through; Viewer matches `runner_for`'s read-only
                // posture for this surface.
                WorkspaceRole::Viewer,
                handle,
                preagg,
            );
            mt::baseline_reads(
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
        err_with_code(
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "baseline timed out after {}s — narrow the period or the scope",
                BASELINE_TIMEOUT.as_secs()
            ),
            "baseline_timeout",
        )
    })?
    .map_err(|e| {
        err_with_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("baseline task panicked: {e}"),
            "baseline_task_panicked",
        )
    })
}

/// Validate the request's lever roots against the tree, and resolve its
/// scope. Split out of [`post_baseline`] to stay under the file's
/// function-length budget now that the cache lookup/store adds to it.
fn validate_and_scope(
    layer: &oxy_airlayer_compat::SemanticLayer,
    tree: &oxy_airlayer_compat::engine::metric_tree::MetricTree,
    req: &mt::BaselineRequest,
) -> Result<Vec<oxy_airlayer_compat::engine::query::QueryFilter>, Response> {
    // Reject unknown levers up front: a typo must not read as "this measure
    // has no value", a completely different message in the UI.
    for root in &req.roots {
        if !tree.nodes.iter().any(|n| &n.id == root) {
            return Err(err_with_code(
                StatusCode::NOT_FOUND,
                format!("measure '{root}' not in tree"),
                "measure_not_found",
            ));
        }
    }
    baseline_scope(layer, req)
}

/// `POST .../metric-tree/baseline` — current values for the levers and
/// everything downstream of them.
pub async fn post_baseline(
    Path(project_id): Path<Uuid>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Gate before cache read so a cached hit can't bypass authorization.
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };
    let key = String::from_utf8_lossy(&body).into_owned();
    if let Some(hit) = cache_lookup(project_id, "mt-baseline", &key, wants_refresh(uri.query())) {
        return hit;
    }
    let req: mt::BaselineRequest = match parse_versioned_body(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let layer = match load_layer(boundary.scan.path_buf()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let tree = oxy_semantic::build_metric_tree(&layer);

    let scope = match validate_and_scope(&layer, &tree, &req) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let reads = match run_baseline_query(&boundary, layer.clone(), &req, scope).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Now that the read runs one query per view, a warehouse failure need not
    // reach the outcome at all: one view answering makes it `Valued` while
    // another errored. Both have to suppress the cache, or a transient failure
    // on one view gets remembered as that view having no values.
    let failure = match &reads.outcome {
        oxy_airlayer_compat::engine::metric_tree_ops::BaselineOutcome::ExecutorError(e) => {
            Some(e.clone())
        }
        _ => reads
            .skipped
            .iter()
            .find(|s| s.kind == mt::SkipKind::QueryFailed)
            .map(|s| format!("`{}`: {}", s.view, s.reason)),
    };

    // Ask the engine WHY rather than inferring it from an empty map — an
    // executor error and an empty window need opposite fixes.
    let unvalued = mt::classify_unvalued(
        &tree,
        &req.roots,
        &reads.values,
        &reads.outcome,
        &reads.skipped,
    );

    let body = mt::BaselineResponse {
        values: reads.values,
        unvalued,
        resolved_period: req.period,
        baseline_note: mt::baseline_note(&reads.outcome, &req.time_dimension, &reads.skipped),
        fitted: reads.fitted,
    };

    // A warehouse that was down when this ran is not a fact about the
    // workspace, and caching it means `?refresh` is the only way back — every
    // sibling op on this router stores on `Ok` only. The failure still
    // *responds*, with its note; it just doesn't get remembered.
    if let Some(e) = failure {
        tracing::warn!(
            error = %e,
            project_id = %boundary.project_id(),
            "metric-tree baseline query failed; responding without caching"
        );
        return axum::Json(body).into_response();
    }

    cache_store(boundary.project_id(), "mt-baseline", &key, &body)
}
