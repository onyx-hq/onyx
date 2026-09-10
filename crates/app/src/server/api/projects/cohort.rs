//! `/api/projects/{project_id}/semantic/cohort` — peer-cohort comparison for
//! customer-app bundles.
//!
//! Sibling of [`super::metric_tree`]: same customer-app gate, same
//! compile-boundary layer load, same `route_fleet` mount. Where the metric
//! tree decomposes ONE subject over time, this compares MANY subjects against
//! each other at a point in time — airlayer's `engine::cohort`, added in
//! airlayer PR #116.
//!
//! Three contracts this module exists to preserve, all of which are easy to
//! break by "tidying" the response:
//!
//!  1. **`excluded` is not optional detail.** A subject that cannot be
//!     compared comes back with a reason. The feature exists partly because in
//!     the hand-rolled predecessor a store "simply did not appear in the list
//!     and no screen said why".
//!  2. **`min_peers` is a reporting predicate, never a filter.** A subject
//!     below the floor is returned with its comparison computed and
//!     `sufficient: false`. Filtering it here would relocate a product
//!     decision into the transport layer.
//!  3. **The cohort name and band window are echoed back**, so a UI rendering
//!     them cannot drift from the query that produced the numbers.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use std::collections::{BTreeSet, HashMap};

use oxy_airlayer_compat::engine::cohort::{
    CohortSubject, ExcludedSubject, PeerCohortResult, augment_failure_reason,
    augment_layer_for_cohort,
};
use oxy_airlayer_compat::engine::metric_tree_ops::BenchmarkStatistic;
use oxy_airlayer_compat::engine::query::{FilterOperator, QueryFilter};
use oxy_airlayer_compat::{BoundView, SemanticLayer};

use crate::server::api::custom_apps_gates::parse_versioned_body;
use crate::server::api::operating_graph::binding::NO_REACH_SENTINEL;
use crate::server::api::projects::semantic_boundary::{
    cache_lookup, cache_store, enter_semantic_boundary, err_with_code, load_layer, wants_refresh,
};
use crate::server::router::AppState;

// ── Wire types ──────────────────────────────────────────────────────────────

/// Request body for a cohort resolution.
#[derive(Debug, Clone, Deserialize)]
pub struct CohortRequest {
    /// Entity the cohort is declared on, e.g. `"restaurant_id"`.
    pub entity: String,
    /// Fully-qualified measure to compare, e.g. `"sales.wage_pct"`.
    pub measure: String,
    /// Time dimension to filter the period on.
    pub time_dimension: String,
    /// `[start, end]`, inclusive, `YYYY-MM-DD`.
    pub period: (String, String),
    /// Cohort as `"entity.cohort_name"`. Falls back to the measure's
    /// `default_cohort:` when omitted.
    #[serde(default)]
    pub cohort: Option<String>,
    /// `median` (default) | `p75` | `best_peer`.
    #[serde(default)]
    pub statistic: Option<String>,
    /// The custom app asking, when there is one. Both siblings
    /// (`semantic_query.rs`, `world_model.rs`) pass it, and reach consults it
    /// for app-admin standing — omitting it silently narrows a per-app admin
    /// to their own roster.
    #[serde(default)]
    pub app: Option<Uuid>,
}

/// Oxy's mirror of airlayer's `PeerCohortResult`.
///
/// Deliberately a mirror rather than a re-serialization of the airlayer type:
/// airlayer is a git pin, and its `skip_serializing_if` attributes are "a
/// serde attribute today, not a guarantee". Owning the wire shape here means a
/// pin bump cannot silently drop a field the SDK and UI depend on, and lets
/// the contract be asserted by a test in this repo.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CohortResponse {
    pub entity: String,
    /// The cohort actually used — echoed so a caller that passed `None` and
    /// got the measure's `default_cohort:` can render which one answered.
    pub cohort: String,
    pub measure: String,
    pub statistic: BenchmarkStatistic,
    pub period: (String, String),
    /// The window the BAND was measured over. `Some(period)` when the cohort
    /// declares a band without its own `window:`; absent only when the cohort
    /// declares no band at all. Always contains `period` — it is anchored at
    /// the period START and extended backward, not shifted.
    ///
    /// Emitted as `null` rather than omitted: the field's absence is
    /// meaningful, so it must not be confusable with a pin-bump regression.
    pub band_window: Option<(String, String)>,
    pub subjects: Vec<CohortSubjectDto>,
    /// Subjects dropped before comparison, each with a reason. Disjoint from
    /// `subjects`. Never empty-by-omission — see the module docs.
    pub excluded: Vec<ExcludedSubjectDto>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CohortSubjectDto {
    pub key: String,
    pub value: f64,
    /// `0.0` when `peer_count == 0` — there is no baseline, not a baseline of
    /// zero. Read it together with `peer_count`.
    pub baseline: f64,
    /// Oriented so positive always means opportunity, whichever direction the
    /// measure improves in.
    pub gap: f64,
    pub peers: Vec<String>,
    pub peer_count: usize,
    /// Whether `peer_count` met the cohort's `min_peers`. NOT a gate: an
    /// insufficient subject is still here, with its comparison computed.
    pub sufficient: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExcludedSubjectDto {
    pub key: String,
    pub reason: String,
}

impl From<&CohortSubject> for CohortSubjectDto {
    fn from(s: &CohortSubject) -> Self {
        Self {
            key: s.key.clone(),
            value: s.value,
            baseline: s.baseline,
            gap: s.gap,
            peers: s.peers.clone(),
            peer_count: s.peer_count,
            sufficient: s.sufficient,
        }
    }
}

impl From<&ExcludedSubject> for ExcludedSubjectDto {
    fn from(e: &ExcludedSubject) -> Self {
        Self {
            key: e.key.clone(),
            reason: e.reason.clone(),
        }
    }
}

impl From<&PeerCohortResult> for CohortResponse {
    fn from(r: &PeerCohortResult) -> Self {
        Self {
            entity: r.entity.clone(),
            cohort: r.cohort.clone(),
            measure: r.measure.clone(),
            statistic: r.statistic,
            period: r.period.clone(),
            band_window: r.band_window.clone(),
            // Every subject, in pull order — no server-side filtering. See the
            // module docs: `sufficient` reports, it does not gate.
            subjects: r.subjects.iter().map(CohortSubjectDto::from).collect(),
            excluded: r.excluded.iter().map(ExcludedSubjectDto::from).collect(),
        }
    }
}

// ── Pure helpers ────────────────────────────────────────────────────────────

/// Parse the statistic, defaulting to `median`.
pub(crate) fn parse_statistic(raw: Option<&str>) -> Result<BenchmarkStatistic, String> {
    match raw.unwrap_or("median") {
        "median" => Ok(BenchmarkStatistic::Median),
        "p75" => Ok(BenchmarkStatistic::P75),
        "best_peer" => Ok(BenchmarkStatistic::BestPeer),
        other => Err(format!(
            "unknown statistic `{other}` — expected one of median, p75, best_peer"
        )),
    }
}

/// Clone `layer` and install the synthetic `__cohort_total__` measure that the
/// truncation guard depends on.
///
/// **This is the load-bearing call.** `augment_layer_for_cohort` returning
/// `false` does not fail loudly: skipping it silently disables the guard that
/// stops a truncated entity pull being medianed as if it were the whole
/// population. So a `false` becomes an `Err` here and the caller must not
/// build an engine.
pub(crate) fn prepare_cohort_layer(
    layer: &SemanticLayer,
    entity: &str,
) -> Result<SemanticLayer, String> {
    let mut augmented = layer.clone();
    if !augment_layer_for_cohort(&mut augmented, entity) {
        // Surface it. Proceeding here would resolve the cohort with the
        // truncation guard silently inert.
        return Err(augment_failure_reason(layer, entity));
    }
    Ok(augmented)
}

/// The bound views that can carry this cohort's entity.
///
/// Several views routinely bind the SAME entity through DIFFERENT integrations
/// — `sales`/`toast` and `labor`/`payroll` both declaring `store`. So this
/// returns all of them and the choice is made per query, not once per request.
pub(crate) fn cohort_bound_views(bound: &[BoundView], entity: &str) -> Vec<BoundView> {
    bound
        .iter()
        .filter(|b| b.entity == entity)
        .cloned()
        .collect()
}

/// One reach filter per bound view the pull actually reads.
///
/// Which view a filter lands on decides which id space it is compared against:
/// `binding.system` selects the external ids, so filtering `labor` with Toast
/// restaurant ids matches nothing and reads as an empty cohort rather than an
/// error. `resolve_cohort` issues several pulls over different views, so the
/// filter is chosen against each pull's own members.
///
/// Refuses when the pull reads no bound view at all — answering it unscoped
/// would hand a location-scoped caller the whole population.
pub(crate) fn scope_filters_for_request(
    cohort_bound: &[BoundView],
    referenced: &BTreeSet<String>,
    keys_by_system: &HashMap<String, Vec<String>>,
) -> Result<Vec<QueryFilter>, String> {
    let touched: Vec<&BoundView> = cohort_bound
        .iter()
        .filter(|b| referenced.contains(&b.view))
        .collect();
    if touched.is_empty() {
        return Err(
            "this cohort pull reads no view bound to the locations registry, so it cannot \
             be limited to your locations"
                .to_string(),
        );
    }
    Ok(touched
        .into_iter()
        .map(|b| {
            // Per bound view, in ITS OWN id space: `binding.system` selects the
            // external ids, so a `labor` view scoped with Toast ids would match
            // nothing and read as an empty cohort rather than an error.
            let keys = keys_by_system
                .get(&b.binding.system)
                .cloned()
                .unwrap_or_default();
            reach_filter(&b.view, &b.key, &keys)
        })
        .collect())
}

/// The `view` half of every member a pull names.
pub(crate) fn referenced_views(
    q: &oxy_airlayer_compat::engine::query::QueryRequest,
) -> BTreeSet<String> {
    q.measures
        .iter()
        .chain(q.dimensions.iter())
        .chain(q.time_dimensions.iter().map(|t| &t.dimension))
        .chain(q.filters.iter().filter_map(|f| f.member.as_ref()))
        .filter_map(|m| m.split_once('.').map(|(v, _)| v.to_string()))
        .collect()
}

/// The reach filter pinning a cohort pull to the caller's stores.
///
/// An empty `keys` yields the sentinel rather than an empty `IN ()`, which is
/// not SQL in every dialect.
pub(crate) fn reach_filter(view: &str, key: &str, keys: &[String]) -> QueryFilter {
    let values = if keys.is_empty() {
        vec![NO_REACH_SENTINEL.to_string()]
    } else {
        keys.to_vec()
    };
    QueryFilter {
        // `equals` with several values compiles to an IN list.
        member: Some(format!("{view}.{key}")),
        operator: Some(FilterOperator::Equals),
        values,
        and: None,
        or: None,
    }
}

/// Resolve which cohort to use: the explicit request value, else the measure's
/// own `default_cohort:`.
///
/// `default_cohort` is stored as `"entity.cohort_name"`. Comparability varies
/// per measure, not per entity, which is why the fallback lives on the measure
/// — and why a mismatch between the declared entity and the requested one is
/// refused rather than coerced.
pub(crate) fn resolve_cohort_name(
    layer: &SemanticLayer,
    entity: &str,
    measure: &str,
    explicit: Option<&str>,
) -> Result<String, String> {
    if let Some(c) = explicit {
        // Accept either a bare name or a qualified `entity.cohort`.
        return match c.split_once('.') {
            None => Ok(c.to_string()),
            Some((decl_entity, name)) if decl_entity == entity => Ok(name.to_string()),
            Some((decl_entity, _)) => Err(format!(
                "cohort `{c}` is declared on entity `{decl_entity}`, but the request asks about `{entity}`"
            )),
        };
    }
    let (view_name, measure_name) = measure
        .split_once('.')
        .ok_or_else(|| format!("measure `{measure}` is not fully qualified as `view.measure`"))?;
    let declared = layer
        .views
        .iter()
        .find(|v| v.name == view_name)
        .and_then(|v| v.measures.as_ref())
        .and_then(|ms| ms.iter().find(|m| m.name == measure_name))
        .and_then(|m| m.default_cohort.clone())
        .ok_or_else(|| {
            format!(
                "no cohort given and measure `{measure}` declares no `default_cohort:` — \
                 name one explicitly"
            )
        })?;
    match declared.split_once('.') {
        Some((decl_entity, name)) if decl_entity == entity => Ok(name.to_string()),
        Some((decl_entity, _)) => Err(format!(
            "measure `{measure}` defaults to a cohort on entity `{decl_entity}`, \
             but the request asks about `{entity}`"
        )),
        None => Err(format!(
            "measure `{measure}` declares `default_cohort: {declared}`, which is not \
             in `entity.cohort_name` form"
        )),
    }
}

/// A stable string identifying the population a caller may see, for use in a
/// cache key, so two differently-scoped callers can never share an entry.
///
/// Keyed on the reach's location UUIDs rather than the resolved warehouse
/// keys, matching `semantic_query.rs`: an external id is warehouse-supplied and
/// may contain the separator, so two distinct reach sets could serialize
/// identically. A UUID cannot.
pub(crate) fn scope_cache_discriminator(
    reach: &crate::server::api::operating_graph::reach::Reach,
) -> String {
    format!(
        "reach:{}:{}",
        reach.everywhere,
        reach
            .locations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Soft cap — a cohort resolution fires two entity-grain pulls; failing loud
/// beats hanging. Matches the metric-tree explain deadline.
const COHORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

/// `POST /api/projects/{project_id}/semantic/cohort`
pub async fn post_cohort(
    Path(project_id): Path<Uuid>,
    uri: Uri,
    headers: HeaderMap,
    State(app_state): State<AppState>,
    body: axum::body::Bytes,
) -> Response {
    // Gate before the cache read so a cached hit cannot bypass authorization.
    let boundary = match enter_semantic_boundary(&headers, project_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    let req: CohortRequest = match parse_versioned_body(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let statistic = match parse_statistic(req.statistic.as_deref()) {
        Ok(s) => s,
        Err(msg) => return err_with_code(StatusCode::BAD_REQUEST, msg, "invalid_statistic"),
    };

    let layer = match load_layer(boundary.scan.path_buf()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };

    let cohort_name =
        match resolve_cohort_name(&layer, &req.entity, &req.measure, req.cohort.as_deref()) {
            Ok(c) => c,
            Err(msg) => {
                return err_with_code(StatusCode::BAD_REQUEST, msg, "cohort_not_resolved");
            }
        };

    // THE GUARD. `augment_layer_for_cohort` installs the synthetic
    // `__cohort_total__` measure the truncation check reads. It fails quietly,
    // so a `false` must become a refusal: without it a truncated entity pull
    // would be medianed as though it were the whole population.
    let augmented = match prepare_cohort_layer(&layer, &req.entity) {
        Ok(l) => l,
        Err(reason) => {
            return err_with_code(
                StatusCode::BAD_REQUEST,
                format!(
                    "cannot resolve a cohort on entity `{}`: {reason}",
                    req.entity
                ),
                "cohort_augment_failed",
            );
        }
    };

    // ── Population scoping ──────────────────────────────────────────────
    // A cohort baseline is computed over a population, so the population is
    // the security boundary, not just the row set. Scope it to the caller's
    // reach the way `semantic_query.rs` does.
    let reach = crate::server::api::operating_graph::reach::reach_for_viewer(
        &boundary.app.db,
        boundary.app.org_id,
        &boundary.app.user,
        project_id,
        req.app,
    )
    .await;

    // Resolve the binding BEFORE branching on `everywhere`. `apply_reach_scope`
    // refuses an unscopable query even for an everywhere caller, and for the
    // same reason: otherwise a workspace with nothing bound ships green and the
    // first location-scoped viewer in production is the one who discovers it,
    // mid-session, as a 403.
    let cohort_bound =
        cohort_bound_views(&oxy_airlayer_compat::bound_views(&augmented), &req.entity);
    if cohort_bound.is_empty() {
        return err_with_code(
            StatusCode::FORBIDDEN,
            format!(
                "entity `{}` is not bound to the locations registry, so a cohort over it \
                 cannot be limited to a caller's locations",
                req.entity
            ),
            "cohort_scope_unavailable",
        );
    }

    // One key list per bound system: two views can bind the same entity through
    // different integrations, and each pull is filtered in its own id space.
    let keys_by_system = if reach.everywhere {
        None
    } else {
        let mut by_system: HashMap<String, Vec<String>> = HashMap::new();
        for system in cohort_bound
            .iter()
            .map(|b| b.binding.system.clone())
            .collect::<BTreeSet<_>>()
        {
            match crate::server::api::operating_graph::binding::external_ids_for_locations(
                &boundary.app.db,
                boundary.app.org_id,
                &system,
                &reach.locations,
            )
            .await
            {
                Ok(k) => {
                    by_system.insert(system, k);
                }
                Err(e) => {
                    return err_with_code(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to resolve your location scope: {e}"),
                        "cohort_scope_failed",
                    );
                }
            }
        }
        Some(by_system)
    };

    // Cache key includes the caller's scope, not just the body. Two callers
    // with different reach ask the same question and must NOT get the same
    // answer — keying on the body alone would serve one caller's population
    // to another. Read it only now that the scope is known.
    let cache_key = format!(
        "{}|{}",
        scope_cache_discriminator(&reach),
        String::from_utf8_lossy(&body)
    );
    if let Some(hit) = cache_lookup(project_id, "cohort", &cache_key, wants_refresh(uri.query())) {
        return hit;
    }

    // ── Executor ────────────────────────────────────────────────────────
    let workspace_manager = boundary.proj_ctx.workspace_manager().clone();
    let databases =
        crate::agentic_wiring::metric_tree_runner::OxyMetricTreeRunner::list_databases_sync(
            &workspace_manager,
        );
    let dialects = oxy_airlayer_compat::DatasourceDialectMap::from_config_databases(&databases);
    let engine =
        match oxy_airlayer_compat::SemanticEngine::from_semantic_layer(augmented.clone(), dialects)
        {
            Ok(e) => std::sync::Arc::new(e),
            Err(e) => {
                return err_with_code(
                    StatusCode::BAD_REQUEST,
                    format!("failed to build the semantic engine: {e}"),
                    "semantic_engine_build_failed",
                );
            }
        };

    let preagg_ctx = crate::server::api::middlewares::workspace_context::PreaggCacheCtx {
        cache: app_state.preagg_cache.clone(),
        renewal_threshold_secs: app_state.preagg_renewal_threshold_secs,
    };
    let renewal_threshold_secs =
        preagg_ctx.renewal_threshold_secs_or(&workspace_manager.config_manager);

    let inner = crate::agentic_wiring::metric_tree_runner::build_query_executor(
        engine,
        databases,
        workspace_manager.into_read_only(),
        boundary.app.user.id,
        entity::workspace_members::WorkspaceRole::Viewer,
        tokio::runtime::Handle::current(),
        crate::agentic_wiring::metric_tree_runner::RunnerPreagg {
            cache: preagg_ctx.cache,
            renewal_threshold_secs,
            // A read surface, same as `/semantic-query` and `ctx.semantic`.
            freshness: crate::server::preagg_context::RollupFreshness::ServeStale,
        },
    );

    // Wrap rather than pre-filter: `resolve_cohort` builds its own
    // `QueryRequest`s internally (the entity-grain pull and the band-window
    // pull), so the executor is the only place that sees both. Choosing the
    // filter per pull — rather than once per request — is what keeps a pull
    // over a differently-bound view in its own id space.
    let executor: Box<oxy_airlayer_compat::engine::metric_tree_ops::QueryExecutor> =
        match keys_by_system {
            None => inner,
            Some(keys) => Box::new(
                move |q: &oxy_airlayer_compat::engine::query::QueryRequest| {
                    let filters =
                        scope_filters_for_request(&cohort_bound, &referenced_views(q), &keys)
                            .map_err(oxy_airlayer_compat::engine::EngineError::QueryError)?;
                    let mut scoped = q.clone();
                    scoped.filters.extend(filters);
                    inner(&scoped)
                },
            ),
        };

    let entity = req.entity.clone();
    let measure = req.measure.clone();
    let time_dimension = req.time_dimension.clone();
    let period = req.period.clone();
    let cohort_for_run = cohort_name.clone();

    let run = tokio::task::spawn_blocking(move || {
        oxy_airlayer_compat::engine::cohort::resolve_cohort(
            &augmented,
            &entity,
            &cohort_for_run,
            &measure,
            &time_dimension,
            (period.0.as_str(), period.1.as_str()),
            statistic,
            &*executor,
        )
    });

    match tokio::time::timeout(COHORT_TIMEOUT, run).await {
        Ok(Ok(Ok(result))) => cache_store(
            boundary.project_id(),
            "cohort",
            &cache_key,
            &CohortResponse::from(&result),
        ),
        Ok(Ok(Err(e))) => err_with_code(StatusCode::BAD_REQUEST, e.to_string(), "cohort_failed"),
        Ok(Err(e)) => err_with_code(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cohort task panicked: {e}"),
            "cohort_panicked",
        ),
        Err(_) => err_with_code(
            StatusCode::GATEWAY_TIMEOUT,
            "cohort resolution timed out".to_string(),
            "cohort_timeout",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(key: &str, peer_count: usize, sufficient: bool) -> CohortSubject {
        CohortSubject {
            key: key.to_string(),
            value: 10.0,
            baseline: 8.0,
            gap: 2.0,
            peers: (0..peer_count).map(|i| format!("peer{i}")).collect(),
            peer_count,
            sufficient,
        }
    }

    fn result_with(
        subjects: Vec<CohortSubject>,
        excluded: Vec<ExcludedSubject>,
    ) -> PeerCohortResult {
        PeerCohortResult {
            entity: "restaurant_id".to_string(),
            cohort: "size_matched".to_string(),
            measure: "sales.wage_pct".to_string(),
            statistic: BenchmarkStatistic::Median,
            period: ("2026-08-01".to_string(), "2026-08-31".to_string()),
            band_window: Some(("2026-05-03".to_string(), "2026-08-31".to_string())),
            subjects,
            excluded,
        }
    }

    #[test]
    fn statistic_accepts_the_three_values_and_defaults_to_median() {
        assert_eq!(parse_statistic(None).unwrap(), BenchmarkStatistic::Median);
        assert_eq!(
            parse_statistic(Some("median")).unwrap(),
            BenchmarkStatistic::Median
        );
        assert_eq!(
            parse_statistic(Some("p75")).unwrap(),
            BenchmarkStatistic::P75
        );
        assert_eq!(
            parse_statistic(Some("best_peer")).unwrap(),
            BenchmarkStatistic::BestPeer
        );
    }

    #[test]
    fn statistic_rejects_anything_else_naming_the_valid_values() {
        let err = parse_statistic(Some("mean")).unwrap_err();
        assert!(
            err.contains("median"),
            "error should name the valid values: {err}"
        );
        assert!(
            err.contains("p75"),
            "error should name the valid values: {err}"
        );
        assert!(
            err.contains("best_peer"),
            "error should name the valid values: {err}"
        );
    }

    /// The reason the feature exists: a dropped subject must arrive with a
    /// reason, not vanish.
    #[test]
    fn excluded_subjects_survive_the_mirror_with_their_reasons() {
        let r = result_with(
            vec![subject("a", 4, true)],
            vec![
                ExcludedSubject {
                    key: "b".to_string(),
                    reason: "opened mid-period".to_string(),
                },
                ExcludedSubject {
                    key: "(null) [direct_only]".to_string(),
                    reason: "entity key was NULL".to_string(),
                },
            ],
        );
        let dto = CohortResponse::from(&r);
        assert_eq!(dto.excluded.len(), 2);
        assert_eq!(dto.excluded[0].key, "b");
        assert_eq!(dto.excluded[0].reason, "opened mid-period");
        // The synthesized id for a NULL key must survive verbatim — it is what
        // makes a keyless row addressable at all.
        assert_eq!(dto.excluded[1].key, "(null) [direct_only]");
    }

    /// `min_peers` is a reporting predicate. An insufficient subject is
    /// returned with its comparison computed; the server does not filter it.
    #[test]
    fn insufficient_subjects_are_returned_not_filtered() {
        let r = result_with(
            vec![subject("thin", 1, false), subject("thick", 9, true)],
            vec![],
        );
        let dto = CohortResponse::from(&r);
        assert_eq!(
            dto.subjects.len(),
            2,
            "an insufficient subject must not be dropped"
        );
        let thin = dto.subjects.iter().find(|s| s.key == "thin").unwrap();
        assert!(!thin.sufficient);
        assert_eq!(thin.peer_count, 1);
        // Its comparison is still computed — the caller decides whether to act.
        assert_eq!(thin.gap, 2.0);
        assert_eq!(thin.baseline, 8.0);
    }

    #[test]
    fn cohort_name_and_band_window_are_echoed_back() {
        let r = result_with(vec![subject("a", 3, true)], vec![]);
        let dto = CohortResponse::from(&r);
        assert_eq!(dto.cohort, "size_matched");
        assert_eq!(dto.entity, "restaurant_id");
        assert_eq!(
            dto.band_window,
            Some(("2026-05-03".to_string(), "2026-08-31".to_string()))
        );
        // The band window always contains the period.
        let (bs, _) = dto.band_window.clone().unwrap();
        assert!(bs <= dto.period.0);
    }

    /// `band_window` must serialize as an explicit `null`, never vanish: a UI
    /// distinguishing "no band declared" from "field missing after a pin bump"
    /// needs the key present.
    #[test]
    fn absent_band_window_serializes_as_null() {
        let mut r = result_with(vec![], vec![]);
        r.band_window = None;
        let json = serde_json::to_value(CohortResponse::from(&r)).unwrap();
        assert!(
            json.get("band_window").is_some(),
            "band_window key must be present even when null"
        );
        assert!(json["band_window"].is_null());
    }

    fn bound(view: &str, key: &str, system: &str) -> BoundView {
        BoundView {
            view: view.into(),
            entity: "store".into(),
            key: key.into(),
            binding: oxy_airlayer_compat::EntityBinding {
                registry: "locations".into(),
                system: system.into(),
            },
        }
    }

    fn two_systems() -> (Vec<BoundView>, HashMap<String, Vec<String>>) {
        let bound = vec![
            bound("sales", "restaurant_id", "toast"),
            bound("labor", "site", "payroll"),
        ];
        let keys = HashMap::from([
            (
                "toast".to_string(),
                vec!["t1".to_string(), "t2".to_string()],
            ),
            ("payroll".to_string(), vec!["p1".to_string()]),
        ]);
        (bound, keys)
    }

    fn refs(vs: &[&str]) -> BTreeSet<String> {
        vs.iter().map(|v| v.to_string()).collect()
    }

    /// Two views can bind the SAME entity through different integrations.
    /// The filter must follow the view the pull reads, or it compares one
    /// system's ids against another's and silently matches nothing.
    #[test]
    fn scope_filter_follows_the_view_the_pull_reads() {
        let (b, keys) = two_systems();
        let cohort_bound = cohort_bound_views(&b, "store");

        let f = scope_filters_for_request(&cohort_bound, &refs(&["sales"]), &keys).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].member.as_deref(), Some("sales.restaurant_id"));
        assert_eq!(f[0].values, vec!["t1".to_string(), "t2".to_string()]);

        let f = scope_filters_for_request(&cohort_bound, &refs(&["labor"]), &keys).unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(
            f[0].member.as_deref(),
            Some("labor.site"),
            "a pull over `labor` must be scoped with payroll ids, not Toast ids"
        );
        assert_eq!(f[0].values, vec!["p1".to_string()]);
    }

    #[test]
    fn scope_filters_every_bound_view_a_pull_joins() {
        let (b, keys) = two_systems();
        let cohort_bound = cohort_bound_views(&b, "store");
        let f =
            scope_filters_for_request(&cohort_bound, &refs(&["sales", "labor"]), &keys).unwrap();
        assert_eq!(f.len(), 2, "both bound views the pull reads must be pinned");
    }

    /// Refuse rather than answer a scoped caller with the whole population.
    #[test]
    fn scope_refuses_a_pull_that_reads_no_bound_view() {
        let (b, keys) = two_systems();
        let cohort_bound = cohort_bound_views(&b, "store");
        assert!(scope_filters_for_request(&cohort_bound, &refs(&["unbound"]), &keys).is_err());
    }

    #[test]
    fn cohort_bound_views_keeps_every_view_binding_the_entity() {
        let (b, _) = two_systems();
        assert_eq!(cohort_bound_views(&b, "store").len(), 2);
        assert!(cohort_bound_views(&b, "employee").is_empty());
    }

    /// The wire value is the contract the SDK pins as a literal union; an
    /// upstream rename must fail here, not in a consumer.
    #[test]
    fn statistic_serializes_to_the_documented_wire_string() {
        let mut r = result_with(vec![], vec![]);
        r.statistic = BenchmarkStatistic::P75;
        let json = serde_json::to_value(CohortResponse::from(&r)).unwrap();
        assert_eq!(json["statistic"], "p75");
        r.statistic = BenchmarkStatistic::BestPeer;
        let json = serde_json::to_value(CohortResponse::from(&r)).unwrap();
        assert_eq!(json["statistic"], "best_peer");
    }

    #[test]
    fn reach_filter_pins_the_entity_key_to_the_callers_stores() {
        let f = reach_filter("restaurants", "restaurant_id", &["g1".into(), "g2".into()]);
        assert_eq!(f.member.as_deref(), Some("restaurants.restaurant_id"));
        assert_eq!(f.operator, Some(FilterOperator::Equals));
        assert_eq!(f.values, vec!["g1".to_string(), "g2".to_string()]);
    }

    /// A scoped caller whose places carry no key in the bound system must
    /// match nothing — never an empty `IN ()`, and never the whole population.
    #[test]
    fn reach_filter_with_no_keys_matches_nothing_rather_than_everything() {
        let f = reach_filter("restaurants", "restaurant_id", &[]);
        assert_eq!(f.values, vec![NO_REACH_SENTINEL.to_string()]);
    }
}
