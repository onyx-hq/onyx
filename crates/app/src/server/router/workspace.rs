//! The per-workspace route tree. Mounted under `/{workspace_id}` in both
//! cloud and local modes (local always uses the nil UUID).
//!
//! This module owns the tree shape plus all per-resource sub-builders
//! (automations, threads, agents, files, etc.). Secrets live in their own
//! module because they ship with an admin-gated middleware.

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post, put};

use agentic_http::{AgenticState, airway_router, automation_router, router as agentic_router};

use crate::api::{
    agent, api_keys, app, apps, artifacts, automation, chart, competitors, compile,
    custom_apps_secrets, data, data_repo, database, execution_analytics, exported_chart, file,
    foot_traffic, integration, local_setup, message, metric_anomalies, metric_tree,
    metric_tree_probe, metric_tree_projection, metrics, modeling, org_subdomain, pipeline, preagg,
    result_files, run, schedules, semantic, simulation, task, test_file, test_project_run,
    test_run, thread, traces, video, workspace_custom_apps, workspace_logo, workspace_members,
    workspace_oxy_access, workspaces, world_model, world_model_graph,
};

use oxy_shared::fleet_role::RouteRole;

use super::AppState;
use super::role_router::RoleRouter;
use super::secrets::build_secret_routes;

pub(super) fn build_workspace_routes(
    app_state: AppState,
    agentic_state: Arc<AgenticState>,
    include_git_features: bool,
    include_local_setup: bool,
) -> RoleRouter {
    let mut router = RoleRouter::new(app_state.clone())
        // `/details` served both halves and was the last route needing the
        // ide-down degrade mechanism. The frontend fetches these two instead.
        .route_fleet("/meta", get(workspaces::get_workspace_meta))
        .route_ide("/git-state", get(workspaces::get_workspace_git_state))
        .route_ide("/status", get(workspaces::get_workspace_status))
        // Diagnostic: the workspace's live git-worktree lifecycle on the ide
        // (branch / idle / clean / would-reap). IdeOnly — the registry is
        // ide-local, so the serve fleet forwards here.
        .route_ide(
            "/worktrees",
            get(crate::server::worktree_registry::get_worktree_status),
        )
        // Camera fleet operator endpoints (sites / cameras / edge-boxes /
        // UniFi integration). Mounted here so `workspace_middleware`
        // (cloud) / `local_context_middleware` (local) gate them, and
        // the URL's `workspace_id` flows into the service layer for
        // resource-ownership checks. The edge-facing `/control/*` tree
        // is mounted separately at the app root (bearer auth resolves
        // workspace_id from the device token).
        .merge_undeclared(
            oxy_cameras::routes::workspace_routes::<AppState>(agentic_state.db.clone()),
            "oxy-cameras owns these paths; every one reads Postgres or S3",
        )
        // Legacy `/workflows` and `/automations` routes have been retired.
        // Workflow execution + the single-file fetch live under
        // `/agentic-workflows` (IdeOnly), mounted below. The automation LIST,
        // however, is served HERE from the compile boundary (FleetOk) so the
        // customer-nav sidebar renders on a stateless serve replica with no
        // working copy — `/agentic-workflows` is IdeOnly and lives in a crate
        // that can't reach `compiled_reader`.
        .route_fleet("/procedures", get(automation::list_automations))
        // Single-automation YAML, also from the boundary, so clicking an automation
        // renders its diagram on a serve node when the ide is down.
        .route_fleet("/procedures/{path_b64}", get(automation::get_automation))
        // Canonical "automations" aliases for the procedure list/get (the term
        // Procedure/Workflow was renamed to Automation). Same FleetOk handlers,
        // classified identically in `role_manifest.rs`. `/procedures` is kept
        // for backward compatibility.
        .route_fleet("/automations", get(automation::list_automations))
        .route_fleet("/automations/{path_b64}", get(automation::get_automation))
        // Same boundary-backed pattern for Airway pipelines (`/agentic-airway`
        // is IdeOnly + in a crate that can't reach `compiled_reader`).
        .route_fleet("/airway-pipelines", get(pipeline::list_pipelines))
        // Static connector metadata for the New Pipeline wizard's marketplace
        // picker. Served rather than duplicated in the frontend so the list
        // has one source of truth (`source_factory::NA_MARKETPLACES`).
        .route_fleet(
            "/airway-pipelines/sp-api/marketplaces",
            get(pipeline::list_sp_api_marketplaces),
        )
        .nest("/threads", build_thread_routes(&app_state))
        .nest("/agents", build_agent_routes(&app_state))
        .nest("/api-keys", build_api_key_routes(&app_state))
        .nest(
            "/files",
            build_file_routes(&app_state, include_git_features),
        )
        // Compile boundary surface: the user-facing Compile button in
        // the IDE header. Gated by `WorkspaceEditor` and the active
        // branch must match the workspace's default branch.
        .route_ide("/compile", post(compile::enqueue_compile))
        .route_ide("/compile/status", get(compile::compile_status))
        .nest("/databases", build_database_routes(&app_state))
        .nest("/integrations", build_integration_routes(&app_state))
        .nest("/secrets", build_secret_routes(app_state.clone()))
        .route_fleet("/members", get(workspace_members::list_workspace_members))
        .route_fleet(
            "/members/{user_id}",
            put(workspace_members::set_workspace_role_override)
                .delete(workspace_members::remove_workspace_role_override),
        )
        // POST = lock Oxy staff OUT; DELETE = lift the lockdown (default is
        // access-granted). Only a REAL org officer may do either.
        .route_fleet(
            "/oxy-access",
            get(workspace_oxy_access::get_oxy_access)
                .post(workspace_oxy_access::lock_oxy_access)
                .delete(workspace_oxy_access::unlock_oxy_access),
        )
        .route_fleet("/org-subdomain", get(org_subdomain::get_org_subdomain))
        .route_fleet("/custom-apps", get(workspace_custom_apps::list_custom_apps))
        // Tenant-side app-secret CREATION (`apps/<app_id>/<KEY>`). POST only:
        // list / reveal / rotate / delete already work on the project-secrets
        // routes, which address a row by id and so never hit the name validator
        // that rejects the `/`. Same handler as the staff
        // `/customer-apps/{id}/secrets` mount, with a `WorkspaceAdmin` guard and
        // a check that the app belongs to the workspace in the path. Postgres
        // only, so `FleetOk`. See `custom_apps_secrets`.
        .route_fleet(
            "/custom-apps/{app_id}/secrets",
            post(custom_apps_secrets::workspace_set),
        )
        .route_fleet("/logo", get(workspace_logo::get_workspace_logo))
        .nest("/apps", build_app_routes(&app_state))
        // NOT under `/agentic-airway`, which is an `IdeOnly` `{*rest}` wildcard
        // for execution safety. This writes to S3 and reads nothing node-local,
        // so pinning it to the singleton would cost HA for no reason.
        .nest_all(
            "/source-uploads",
            RouteRole::FleetOk,
            build_source_upload_routes(),
            "report uploads write to the shared S3 landing zone, nothing node-local",
        )
        .nest(
            "/app-integrations",
            build_app_integration_routes(&app_state),
        )
        .nest("/tests", build_test_file_routes(&app_state))
        .nest_all(
            "/traces",
            RouteRole::FleetOk,
            traces::traces_routes(),
            "trace reads come from the runtime tables",
        )
        .nest_all(
            "/metrics",
            RouteRole::FleetOk,
            metrics::metrics_routes(),
            "metric reads come from the runtime tables",
        )
        .nest_all(
            "/execution-analytics",
            RouteRole::FleetOk,
            execution_analytics::execution_analytics_routes(),
            "run analytics aggregate the runtime tables",
        )
        .route_fleet("/artifacts/{id}", get(artifacts::get_artifact))
        .route_fleet("/charts/{file_path}", get(chart::get_chart))
        .route_ide(
            "/exported-charts/{file_name}",
            get(exported_chart::get_exported_chart),
        )
        .route_fleet("/logs", get(thread::get_logs))
        .route_ide("/events", get(run::automation_events))
        .route_ide("/events/lookup", get(task::agentic_events))
        .route_ide("/events/sync", get(run::automation_events_sync))
        .route_fleet("/blocks", get(run::get_blocks))
        .route_fleet(
            "/runs/{source_id}/{run_index}",
            delete(run::cancel_automation_run),
        )
        .route_fleet(
            "/builder-availability",
            get(agent::check_builder_availability),
        )
        // `/onboarding-readiness`, `/onboarding/github-setup` and the
        // `/onboarding/*` subtree moved to the `oxy-api-onboarding` sibling
        // crate; they are merged into this nest by `oxy-server` and declared
        // at that seam, since `route_ide` cannot reach across the crate line.
        .route_fleet("/sql/{pathb64}", post(data::execute_sql))
        .route_fleet("/sql/query", post(data::execute_sql_query))
        // Semantic-model endpoints the IDE uses. The legacy `/semantic`
        // execute route was retired alongside `oxy-workflow`; execution
        // now flows through the agentic pipeline. Compile + the
        // read-only file handlers stay here so the IDE's topic / view
        // explorer + SQL preview panel keep working without re-introducing
        // a workflow dependency on the request path. Compile reaches
        // into airlayer via `agentic_automation::semantic_bridge`.
        .route_fleet(
            "/semantic/topic/{file_path_b64}",
            get(semantic::get_topic_details),
        )
        .route_fleet(
            "/semantic/view/{file_path_b64}",
            get(semantic::get_view_details),
        )
        // Moved to `api::preagg` by #2989, which also added the rebuild verb.
        // Both stay FleetOk: the list comes from the DECLARATIONS (resolved
        // through the compile boundary first), and a rollup built on another
        // node is readable here when a blob bucket is configured.
        .route_fleet("/semantic/preagg-status", get(preagg::get_preagg_status))
        .route_fleet("/semantic/preagg-rebuild", post(preagg::rebuild_preagg))
        .route_fleet("/semantic/compile", post(semantic::compile_semantic_query))
        .route_fleet("/semantic", post(semantic::execute_semantic_query))
        // Metric tree — structure + pure analysis ops over the semantic model.
        // Merged as its own sub-router so the diagnostic probe layers over
        // these ten routes and nothing else.
        //
        // FleetOk on every route: the scan root resolves through the compile
        // boundary first (`semantic::resolve_query_scan_source`), working copy
        // second, and warehouse execution needs only config + secrets — so a
        // replica answers these. Pinned by
        // `workspace_metric_tree_routes_are_fleet_ok`; the outage behind it is
        // oxy-hq/oxygen#878 (every call 500'd on the serve fleet when these
        // read `semantics_scan_path()` directly).
        .merge(build_metric_tree_routes(&app_state))
        // Declared worlds and their runs. Reads come from the compile boundary
        // (`simulation_definitions`) and Postgres (`simulation_run*`); the POST
        // only enqueues a TaskSpec. A run DOES touch local disk — a per-run
        // TempDir — but that happens on the worker, never in a handler, which
        // is the whole reason it is queued work rather than a spawn.
        .route_fleet("/simulations", get(simulation::list_simulations))
        .route_fleet(
            "/simulations/validate",
            post(simulation::validate_simulation),
        )
        .route_fleet("/simulations/runs", get(simulation::list_runs))
        .route_fleet("/simulations/runs/{run_id}", get(simulation::get_run))
        .route_fleet("/simulations/{name}/runs", post(simulation::enqueue_run))
        // The paired profit race over one world's runs. A `workspace_id`-
        // scoped join of `simulation_runs` onto `simulation_run_periods` and
        // nothing else — no working copy, no `.git`, no state dir — so it is
        // FleetOk like every other read here, and pinning it to the ide
        // would make *viewing* a finished race need the singleton.
        .route_fleet("/simulations/{name}/race", get(simulation::get_profit_race))
        .route_ide(
            "/semantic/world-model",
            get(world_model_graph::get_world_model),
        )
        .route_ide(
            "/semantic/world-model/instances",
            get(world_model_graph::get_world_model_instances),
        )
        .route_ide(
            "/semantic/world-model/filter-instances",
            get(world_model_graph::get_world_model_filter_instances),
        )
        .route_ide(
            "/semantic/world-model/filter-counts",
            post(world_model_graph::post_world_model_filter_counts),
        )
        .route_ide(
            "/semantic/world-model/instance-detail",
            get(world_model_graph::get_world_model_instance_detail),
        )
        .route_ide(
            "/semantic/world-model/measure-breakdown",
            get(world_model_graph::get_world_model_measure_breakdown),
        )
        // Anomaly inbox — backed by oxy-metric-monitoring. Nested so the
        // `Extension<Arc<AgenticState>>` layer scopes only to these routes
        // (same pattern as `build_schedule_routes` below).
        //
        // `/semantic/monitors` is merged rather than plainly routed because it
        // also extracts `Arc<AgenticState>` (for the per-segment scan coverage
        // it returns alongside the config) and so needs the same Extension
        // layer. Merging a one-route sub-router keeps the layer scoped to it
        // and preserves the exact path — nesting would also answer on
        // `/semantic/monitors/`, which nothing calls.
        .merge({
            use axum::Extension;
            RoleRouter::new(app_state.clone())
                .route_fleet("/semantic/monitors", get(metric_anomalies::list_monitors))
                .map_router(|r| r.layer(Extension(agentic_state.clone())))
        })
        .nest(
            "/semantic/anomalies",
            build_metric_anomaly_routes(&app_state, agentic_state.clone()),
        )
        // FleetOk since the live feed stopped being process-local: publishers
        // append to `world_model_events` and every pod tails that table onto
        // its own bus, so any replica can serve a subscriber. Pinning it to the
        // ide would make watching the panel depend on the singleton for no
        // remaining reason. The handler reads Postgres and nothing else.
        .route_fleet(
            "/world-model/events",
            get(world_model::world_model_events_sse),
        )
        .route_fleet("/world-model/cameras", get(video::list_cameras))
        .route_fleet(
            "/world-model/weather/{layer}/{z}/{x}/{y}",
            get(video::weather_tile),
        )
        .route_fleet(
            "/world-model/weather/current",
            post(video::weather_current_batch),
        )
        .route_fleet(
            "/world-model/foot-traffic/current",
            post(foot_traffic::foot_traffic_current_batch),
        )
        .route_fleet(
            "/world-model/foot-traffic/radar",
            post(foot_traffic::foot_traffic_radar_batch),
        )
        .route_fleet(
            "/world-model/competitors",
            post(competitors::get_competitors),
        )
        .route_fleet(
            "/results/files/{file_id}",
            get(result_files::get_result_file).delete(result_files::delete_result_file),
        )
        .nest_declared(
            "/analytics",
            agentic_router(agentic_state.clone()),
            agentic_http::router_roles(),
        )
        // New agentic-workflow surface — coexists with the legacy `/workflows`
        // routes during migration. Will subsume them in the cleanup task.
        .nest_declared(
            "/agentic-workflows",
            automation_router(agentic_state.clone()),
            agentic_http::automation_router_roles(),
        )
        // Canonical execution surface alias (Procedures/Workflows -> Automations).
        // Same handlers as `/agentic-workflows`, mirrored in `role_manifest.rs`.
        .nest_declared(
            "/agentic-automations",
            automation_router(agentic_state.clone()),
            agentic_http::automation_router_roles(),
        )
        .nest_declared(
            "/agentic-airway",
            airway_router(agentic_state.clone()),
            agentic_http::airway_router_roles(),
        )
        // Relocated from agentic-http (§12 FU4b): the schedule handlers
        // need WorkspaceAdmin from app/role_guards which agentic-http
        // cannot depend on.
        .nest(
            "/agentic-schedules",
            build_schedule_routes(&app_state, agentic_state),
        )
        .nest_typed(
            "/modeling",
            RouteRole::IdeOnly,
            modeling::build_modeling_routes(),
            super::IdeState(app_state.clone()),
        );

    if include_git_features {
        router = router
            .merge(build_git_routes(&app_state))
            .nest("/repositories", build_data_repo_routes(&app_state));
    }

    if include_local_setup {
        router = router
            .route_fleet("/setup/empty", post(local_setup::setup_empty))
            .route_fleet("/setup/demo", post(local_setup::setup_demo));
    }

    router
}

/// Curated subset of workspace routes for the EXTERNAL API surface
/// (`/external/api/{workspace_id}/...`): just the query + agent + world-model
/// endpoints a standalone app needs, WITHOUT the IDE / file / git / admin /
/// settings surface. Mounted with API-key-only auth + wide-open CORS by
/// [`super::protected::build_external_api_router`]. Reuses the exact same
/// handler functions as `build_workspace_routes`, so behavior is identical —
/// only the auth + CORS wrapper differs.
pub(super) fn build_external_workspace_routes(
    app_state: &AppState,
    agentic_state: Arc<AgenticState>,
) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_fleet("/sql/query", post(data::execute_sql_query))
        .route_fleet("/semantic", post(semantic::execute_semantic_query))
        .route_fleet("/semantic/compile", post(semantic::compile_semantic_query))
        // FleetOk since the live feed stopped being process-local: publishers
        // append to `world_model_events` and every pod tails that table onto
        // its own bus, so any replica can serve a subscriber. Pinning it to the
        // ide would make watching the panel depend on the singleton for no
        // remaining reason. The handler reads Postgres and nothing else.
        .route_fleet(
            "/world-model/events",
            get(world_model::world_model_events_sse),
        )
        .route_fleet("/world-model/cameras", get(video::list_cameras))
        // Camera live streaming for standalone apps: registry, WHEP
        // signaling, HLS proxy. Same handlers as the operator tree —
        // only the auth wrapper differs.
        .nest_all(
            "/world-model/camera-stream",
            RouteRole::FleetOk,
            oxy_cameras::routes::external_stream_routes::<AppState>(agentic_state.db.clone()),
            "camera streaming signals against Postgres + the media edge",
        )
        // Evidence/compliance clip playback for standalone apps — mints a
        // presigned S3 GET (workspace-prefix checked). Without this the
        // customer app's clip-URL call falls through to the SPA.
        //
        // Path is intentionally `/cameras/clips/...` at the tree root
        // (mirrors the operator route), NOT under `/world-model/camera-stream`
        // like the streaming nest — already-shipped customer apps call this
        // exact path. Don't "tidy" it under the stream prefix; that silently
        // re-breaks playback and would need a coordinated app+server deploy.
        .merge_undeclared(
            oxy_cameras::routes::external_clip_routes::<AppState>(),
            "oxy-cameras clip playback mints a presigned S3 GET",
        )
        .route_fleet(
            "/world-model/weather/{layer}/{z}/{x}/{y}",
            get(video::weather_tile),
        )
        .route_fleet(
            "/world-model/weather/current",
            post(video::weather_current_batch),
        )
        .route_fleet(
            "/world-model/foot-traffic/current",
            post(foot_traffic::foot_traffic_current_batch),
        )
        .route_fleet(
            "/world-model/foot-traffic/radar",
            post(foot_traffic::foot_traffic_radar_batch),
        )
        .route_fleet(
            "/world-model/competitors",
            post(competitors::get_competitors),
        )
        // Thin LLM passthrough for the standalone voice assistant: takes an
        // Anthropic Messages payload (system + messages + optional tools),
        // resolves the workspace Anthropic key server-side, and returns the raw
        // content blocks — so the app never ships a provider key in the browser.
        .route_fleet(
            "/world-model/llm/messages",
            post(world_model::proxy_llm_messages),
        )
        // Anomaly monitoring: list, scan, update status, explain — used by
        // standalone apps (e.g. world-model-app) that can't reach the internal
        // /api surface. Mirrors `build_metric_anomaly_routes` exactly, so the
        // TypeScript SDK's `client.anomalies.*` (list/scan/updateStatus/explain)
        // works against `/external/api` as well as `/api`.
        //
        // Nested with an explicit Extension layer because every handler extracts
        // Arc<AgenticState> for db access (same pattern as build_metric_anomaly_routes).
        //
        // `/scan` and `/{id}/explain` are long-running (a scan waits up to 55 s
        // before returning `pending: true`; an uncached explain runs a 20-30 s
        // recursive search) but bounded by the same `timeout_middleware` the rest
        // of this surface carries — no separate budget needed.
        // The same builder the internal surface nests, rather than a copy of
        // it. The two differed only in a path-parameter NAME (`{id}` vs
        // `{anomaly_id}`), which no handler reads — both take
        // `Path<(Uuid, Uuid)>`, positionally. Keeping the copy also hid these
        // routes from `route_role_derivation`, whose parser resolves a nest
        // prefix through the `build_*` call and cannot see into an inline
        // block: `/scan` reached that guard with no prefix and classified as
        // FleetOk by default.
        .nest(
            "/semantic/anomalies",
            build_metric_anomaly_routes(&app_state, agentic_state.clone()),
        )
        // Agentic analytics: POST /analytics/runs, the SSE events stream,
        // /answer, /cancel, /threads/* — the chat surface external apps drive.
        .nest_declared(
            "/analytics",
            agentic_router(agentic_state),
            agentic_http::router_roles(),
        )
}

/// Git-backed workspace routes: local and remote git operations on the
/// workspace itself. Mounted only when `include_git_features` is true —
/// local mode (`ServeMode::Local`) omits the entire set.
fn build_git_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_ide("/branches", get(workspaces::get_workspace_branches))
        .route_ide("/branches/{branch_name}", delete(workspaces::delete_branch))
        .route_ide("/switch-branch", post(workspaces::switch_workspace_branch))
        .route_ide("/pull-changes", post(workspaces::pull_changes))
        .route_ide("/fetch", post(workspaces::fetch_changes))
        .route_ide("/push-changes", post(workspaces::push_changes))
        .route_ide("/abort-rebase", post(workspaces::abort_rebase))
        .route_ide("/continue-rebase", post(workspaces::continue_rebase))
        .route_ide(
            "/resolve-conflict-file",
            post(workspaces::resolve_conflict_file),
        )
        .route_ide(
            "/unresolve-conflict-file",
            post(workspaces::unresolve_conflict_file),
        )
        .route_ide(
            "/resolve-conflict-with-content",
            post(workspaces::resolve_conflict_with_content),
        )
        .route_ide("/force-push", post(workspaces::force_push_branch))
        .route_ide("/discard-all", post(workspaces::discard_all_changes))
        .route_ide("/recent-commits", get(workspaces::get_recent_commits))
        .route_ide("/revision-info", get(workspaces::get_revision_info))
        .route_ide("/reset-to-commit", post(workspaces::reset_to_commit))
}

// `build_workflow_routes` and `build_automation_routes` were retired
// alongside `oxy-workflow`. The agentic-pipeline workflow surface mounted
// at `/agentic-workflows` replaces every endpoint they exposed.

fn build_thread_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_fleet(
            "/",
            get(thread::get_threads)
                .post(thread::create_thread)
                .delete(thread::delete_all_threads),
        )
        .route_fleet("/bulk-delete", post(thread::bulk_delete_threads))
        .route_fleet(
            "/{id}",
            get(thread::get_thread).delete(thread::delete_thread),
        )
        // Thread-bound legacy `/workflow` and `/workflow-sync` routes were
        // retired with `oxy-workflow`. Use the agentic-pipeline workflow
        // surface (`/agentic-workflows/runs`) instead.
        .route_fleet("/{id}/messages", get(message::get_messages_by_thread))
        .route_fleet("/{id}/stop", post(thread::stop_thread))
}

fn build_agent_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone()).route_fleet("/", get(agent::get_agents))
}

/// Metric-tree structure + analysis ops, carrying the diagnostic probe.
///
/// Kept as a sub-router purely so [`metric_tree_probe::probe`] covers exactly
/// these routes: layering it on the workspace router would time every
/// workspace endpoint, and the question it answers is about these ten.
/// Paths stay absolute (`.merge`, not `.nest`) so the role manifest's
/// classification of each one is unchanged.
fn build_metric_tree_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_fleet("/semantic/metric-tree", get(metric_tree::get_metric_tree))
        .route_fleet(
            "/semantic/metric-tree/{measure_id}/sensitivity",
            get(metric_tree::get_sensitivity),
        )
        .route_fleet(
            "/semantic/metric-tree/predict",
            post(metric_tree::post_predict),
        )
        .route_fleet(
            "/semantic/metric-tree/explain",
            post(metric_tree::post_explain),
        )
        .route_fleet(
            "/semantic/metric-tree/opportunity",
            post(metric_tree::post_opportunity),
        )
        .route_fleet(
            "/semantic/metric-tree/drill",
            post(metric_tree::post_opportunity_drill),
        )
        .route_fleet(
            "/semantic/metric-tree/time-dimensions",
            get(metric_tree::get_time_dimensions),
        )
        .route_fleet(
            "/semantic/metric-tree/distribution",
            post(metric_tree::post_distribution),
        )
        .route_fleet(
            "/semantic/metric-tree/baseline",
            post(metric_tree::post_baseline),
        )
        .route_fleet(
            "/semantic/metric-tree/projection",
            post(metric_tree_projection::post_projection),
        )
        .map_router(|r| r.layer(axum::middleware::from_fn(metric_tree_probe::probe)))
}

fn build_api_key_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_fleet(
            "/",
            get(api_keys::list_api_keys).post(api_keys::create_api_key),
        )
        .route_fleet(
            "/{id}",
            get(api_keys::get_api_key).delete(api_keys::delete_api_key),
        )
}

/// Schedule CRUD + run-now (§12 FU4b). Lives in the app crate so the
/// handlers can use `WorkspaceAdmin` from `role_guards` (agentic-http is
/// a lower layer and must not depend on `app`). AgenticState is attached
/// as an Extension here so the handlers can extract it.
fn build_metric_anomaly_routes(
    app_state: &AppState,
    agentic_state: Arc<AgenticState>,
) -> RoleRouter {
    use axum::Extension;
    RoleRouter::new(app_state.clone())
        .route_fleet("/", get(metric_anomalies::list_anomalies))
        // See the mirror of these two in `build_external_workspace_routes`:
        // the runner reads the semantic model off the workspace root, so
        // FleetOk was a promise a replica could not keep.
        .route_ide("/scan", post(metric_anomalies::run_scan))
        // Static `/status` (bulk) and `/{anomaly_id}/status` (single) differ in
        // segment count, so they don't compete for a match. Bulk triage writes
        // Postgres rows only — any replica can take it.
        .route_fleet("/status", post(metric_anomalies::update_status_bulk))
        .route_fleet(
            "/{anomaly_id}/status",
            post(metric_anomalies::update_status),
        )
        .route_ide(
            "/{anomaly_id}/explain",
            post(metric_anomalies::explain_anomaly),
        )
        .map_router(|r| r.layer(Extension(agentic_state)))
}

fn build_schedule_routes(app_state: &AppState, agentic_state: Arc<AgenticState>) -> RoleRouter {
    use axum::Extension;
    RoleRouter::new(app_state.clone())
        .route_fleet("/", get(schedules::list).post(schedules::create))
        .route_fleet(
            "/{id}",
            get(schedules::get)
                .patch(schedules::update)
                .delete(schedules::delete),
        )
        .route_fleet("/{id}/run-now", post(schedules::run_now))
        .route_fleet("/{id}/backfill", post(schedules::backfill))
        .map_router(|r| r.layer(Extension(agentic_state)))
}

fn build_file_routes(app_state: &AppState, include_git_features: bool) -> RoleRouter {
    let mut router = RoleRouter::new(app_state.clone())
        .route_ide("/", get(file::get_file_tree))
        .route_ide("/{pathb64}", get(file::get_file).post(file::save_file))
        .route_ide("/{pathb64}/delete-file", delete(file::delete_file))
        .route_ide("/{pathb64}/delete-folder", delete(file::delete_folder))
        .route_ide("/{pathb64}/rename-file", put(file::rename_file))
        .route_ide("/{pathb64}/rename-folder", put(file::rename_folder))
        .route_ide("/{pathb64}/new-file", post(file::create_file))
        .route_ide("/{pathb64}/new-folder", post(file::create_folder));

    if include_git_features {
        router = router
            .route_ide("/diff-summary", get(file::get_diff_summary))
            .route_ide("/{pathb64}/from-git", get(file::get_file_from_git))
            .route_ide("/{pathb64}/revert", post(file::revert_file));
    }

    router
}

fn build_database_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        // The list degrades to `datasets: null` without a working copy and the
        // launcher readiness check calls it on every page load; creating one
        // writes `config.yml`.
        .route_split(
            "/",
            "POST",
            post(database::create_database_config),
            "GET",
            get(database::list_databases),
        )
        .route_ide("/test-connection", post(database::test_database_connection))
        .route_fleet("/inspect", post(database::inspect_database_handler))
        .route_fleet("/inspect-schemas", post(database::inspect_schemas_handler))
        .route_fleet(
            "/inspect-schema-tables",
            post(database::inspect_schema_tables_handler),
        )
        .route_ide("/sync", post(database::sync_database))
        .route_ide("/build", post(data::build_embeddings))
        .route_ide("/clean", post(database::clean_data))
        .route_fleet(
            "/{database_name}/schema",
            get(database::get_database_schema),
        )
}

fn build_data_repo_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_ide(
            "/",
            get(data_repo::list_repositories).post(data_repo::add_repository),
        )
        .route_ide("/{name}", delete(data_repo::remove_repository))
        .route_ide("/{name}/branch", get(data_repo::get_repo_branch))
        .route_ide("/{name}/branches", get(data_repo::list_repo_branches))
        .route_ide("/{name}/checkout", post(data_repo::checkout_repo_branch))
        .route_ide("/{name}/diff", get(data_repo::get_repo_diff))
        .route_ide("/{name}/commit", post(data_repo::commit_repo))
        .route_ide("/{name}/files", get(data_repo::get_repo_file_tree))
        .route_ide("/github", post(data_repo::add_repo_from_github))
}

fn build_integration_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_ide("/looker", get(integration::list_looker_integrations))
        .route_ide("/looker/query", post(integration::execute_looker_query))
        .route_ide("/looker/query/sql", post(integration::compile_looker_query))
        .route_fleet(
            "/quickbooks/authorize",
            post(crate::integrations::quickbooks::oauth::authorize::authorize),
        )
        // Uniform OAuth connect for any provider in `integrations::oauth_provider`.
        // FleetOk like its QuickBooks predecessor: Postgres state row + secret
        // manager, no filesystem and no local state dir.
        .route_fleet(
            "/oauth/{provider}/authorize",
            post(crate::integrations::quickbooks::oauth::authorize::authorize_by_slug),
        )
}

// MIXED role builder. IdeOnly (must reach the ide singleton): `/source` + `/file`
// read the working copy / local state dir; `/{pathb64}` + `/run` + `/result`
// EXECUTE the inline workflow over the local DuckDB connector; `/publish`,
// `/unpublish`, `/save-from-run/{run_id}` WRITE the working copy. FleetOk
// (compile boundary + S3 + Postgres): `/` (list), `/displays`, `/data-cached`,
// `/charts/...`. Any FS-reading/-writing route added here MUST be classified by
// hand in `server/role_manifest.rs` — the `fully_fs_builder_routes_classify_ide_only`
// drift test only covers FULLY-fs builders, so it can't catch a miss here.
// Skipping it is how `/apps/source` shipped FleetOk and 404'd on the serve fleet;
// the `every_app_sub_route_is_classified` test now backstops THIS builder so a
// new sub-route fails CI unless it is IdeOnly or explicitly acknowledged FleetOk.
// See `.claude/skills/oxy-route-classification/SKILL.md`.
fn build_app_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_fleet("/", get(app::list_apps))
        .route_ide("/{pathb64}", get(app::get_app_data))
        .route_ide("/{pathb64}/run", post(app::run_app))
        .route_ide("/{pathb64}/result", post(app::get_app_result))
        .route_fleet("/{pathb64}/displays", get(app::get_displays))
        // Read-only cached data (boundary def + disk/S3 cache, no execution), so
        // a serve replica shows a dashboard's last data when the ide is down.
        .route_fleet("/{pathb64}/data-cached", get(app::get_app_data_cached))
        .route_fleet("/{pathb64}/charts/{chart_path}", get(app::get_chart_image))
        .route_ide("/{pathb64}/publish", post(app::publish_app))
        .route_ide("/{pathb64}/unpublish", post(app::unpublish_app))
        .route_ide("/file/{pathb64}", get(app::get_data))
        .route_ide("/source/{pathb64}", get(app::get_source_file))
        .route_ide("/save-from-run/{run_id}", post(app::save_app_builder_run))
}

/// Report uploads for file-based sources, into the shared landing zone.
///
/// Deliberately its own nest rather than a child of `/agentic-airway`: that
/// prefix is classified `IdeOnly` for every method so a live run stays on the
/// instance holding the working copy, and this handler touches no working copy.
fn build_source_upload_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/reports",
            post(crate::server::api::source_upload::upload_report),
        )
        // Without this, axum 0.8's 2 MiB default governs and the handler's own
        // ceiling is unreachable — a larger report fails inside `field.bytes()`
        // as a 400, never the 413 the handler writes. At `Router` level rather
        // than on the `MethodRouter` for the same reason `oxy-api-onboarding`
        // gives: the latter can interact unexpectedly with outer CORS preflight
        // handling on axum 0.8.
        // `MAX_REPORT_BYTES` plus slack, because this bounds the whole
        // multipart body while the constant bounds ONE FILE: boundaries, field
        // names, `pipeline_ref`, `workflow_id` and the period all ride along.
        // Set equal, a file a few hundred bytes under the ceiling passed the
        // client-side check and the handler's own check and still died here —
        // answered by this layer's terse 400, never the handler's 413 that
        // names the size. The handler stays the authority on the file itself.
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::server::api::source_upload::MAX_REPORT_BYTES + 64 * 1024,
        ))
}

fn build_test_file_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        // `.test.yml` is the one workspace artifact the compile boundary
        // deliberately does not carry: `oxy_compile::walker` skips every path
        // containing `.test.`, so there is nothing in Postgres to fall back to
        // and `list_tests`/`resolve_test` are `ConfigManager<WorkingCopy>`
        // methods by construction. `IdeOnly` is the honest classification —
        // a replica forwards to the ide upstream instead of reporting an
        // absent directory as an empty test suite. Not a hot path: only the
        // ide opens these.
        .route_ide("/", get(test_file::list_test_files))
        .route_fleet(
            "/project-runs",
            get(test_project_run::list_project_runs).post(test_project_run::create_project_run),
        )
        .route_fleet(
            "/project-runs/{project_run_id}",
            delete(test_project_run::delete_project_run),
        )
        .route_ide("/{pathb64}", get(test_file::get_test_file))
        .route_ide(
            "/{pathb64}/cases/{case_index}",
            post(test_file::run_test_case),
        )
        .route_fleet(
            "/{pathb64}/runs",
            get(test_run::list_runs).post(test_run::create_run),
        )
        .route_fleet(
            "/{pathb64}/runs/{run_index}",
            get(test_run::get_run).delete(test_run::delete_run),
        )
        .route_fleet(
            "/{pathb64}/runs/{run_index}/human-verdicts",
            get(test_run::list_human_verdicts),
        )
        .route_fleet(
            "/{pathb64}/runs/{run_index}/cases/{case_index}/human-verdict",
            put(test_run::set_human_verdict),
        )
}

/// World-model "Apps" configuration routes — Toast / OpenWeatherMap /
/// BestTime integration entries surfaced in the Workspace Settings →
/// Apps tab. Separate from `build_app_routes` (data apps).
fn build_app_integration_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_split(
            "/",
            "POST",
            post(apps::upsert_app),
            "GET",
            get(apps::list_apps),
        )
        .route_ide("/{kind}", delete(apps::delete_app))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentic_pipeline::platform::ThreadOwnerLookup;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sea_orm::DatabaseConnection;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use oxy_app_core::serve_mode::ServeMode;

    fn test_app_state() -> AppState {
        AppState {
            enterprise: false,
            internal: false,
            mode: ServeMode::Local,
            observability: None,
            startup_cwd: std::path::PathBuf::new(),
            preagg_cache: None,
            preagg_renewal_threshold_secs: None,
            agentic_state: None,
            semantic_layer_cache: crate::server::router::workspace_cache::new_semantic_layer_cache(
            ),
            semantic_engine_cache:
                crate::server::router::workspace_cache::new_semantic_engine_cache(),
        }
    }

    struct StubThreadOwner;

    #[async_trait]
    impl ThreadOwnerLookup for StubThreadOwner {
        async fn thread_owner(
            &self,
            _thread_id: uuid::Uuid,
        ) -> Result<Option<Option<uuid::Uuid>>, String> {
            Ok(None)
        }
    }

    fn test_agentic_state() -> Arc<AgenticState> {
        Arc::new(AgenticState::new(
            CancellationToken::new(),
            DatabaseConnection::default(),
            Arc::new(StubThreadOwner),
        ))
    }

    async fn status_for(router: axum::Router, method: &str, path: &str) -> StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        router.oneshot(req).await.unwrap().status()
    }

    /// Every git-shaped route must 404 when `include_git_features: false`.
    /// This is the invariant that guarantees local mode cannot reach a
    /// git handler regardless of how the caller is wired.
    #[tokio::test]
    async fn git_routes_absent_when_flag_disabled() {
        let state = test_app_state();
        let router = build_workspace_routes(state.clone(), test_agentic_state(), false, false)
            .into_router()
            .with_state(state);

        let cases: &[(&str, &str)] = &[
            ("GET", "/branches"),
            ("DELETE", "/branches/foo"),
            ("POST", "/switch-branch"),
            ("POST", "/pull-changes"),
            ("POST", "/fetch"),
            ("POST", "/push-changes"),
            ("POST", "/force-push"),
            ("POST", "/discard-all"),
            ("POST", "/abort-rebase"),
            ("POST", "/continue-rebase"),
            ("POST", "/resolve-conflict-file"),
            ("POST", "/unresolve-conflict-file"),
            ("POST", "/resolve-conflict-with-content"),
            ("GET", "/recent-commits"),
            ("GET", "/revision-info"),
            ("POST", "/reset-to-commit"),
            ("GET", "/repositories"),
            ("GET", "/files/Zm9vLnltbA==/from-git"),
            ("POST", "/files/Zm9vLnltbA==/revert"),
        ];

        for (method, path) in cases {
            let router = router.clone();
            let status = status_for(router, method, path).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} must 404 when git is disabled (got {status})"
            );
        }
    }

    /// Sanity check: when the flag is on, the same routes are mounted.
    /// We only assert `!= 404` — the actual status depends on handler
    /// behavior under a stripped harness, which is out of scope.
    #[tokio::test]
    async fn git_routes_present_when_flag_enabled() {
        let state = test_app_state();
        let router = build_workspace_routes(state.clone(), test_agentic_state(), true, false)
            .into_router()
            .with_state(state);

        let status = status_for(router, "GET", "/branches").await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "/branches must be mounted when git is enabled"
        );
    }

    /// Setup endpoints are mounted when `include_local_setup: true` (local mode).
    #[tokio::test]
    async fn setup_routes_present_when_include_local_setup_true() {
        let state = test_app_state();
        let router = build_workspace_routes(state.clone(), test_agentic_state(), false, true)
            .into_router()
            .with_state(state);

        for path in ["/setup/empty", "/setup/demo"] {
            let status = status_for(router.clone(), "POST", path).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{} must be mounted when include_local_setup=true (got {})",
                path,
                status
            );
        }
    }

    /// Setup endpoints are absent when `include_local_setup: false` (cloud mode).
    #[tokio::test]
    async fn setup_routes_absent_when_include_local_setup_false() {
        let state = test_app_state();
        let router = build_workspace_routes(state.clone(), test_agentic_state(), true, false)
            .into_router()
            .with_state(state);

        for path in ["/setup/empty", "/setup/demo"] {
            let status = status_for(router.clone(), "POST", path).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{} must 404 when include_local_setup=false (got {})",
                path,
                status
            );
        }
    }

    /// The camera fleet operator surface (sites / cameras / edge boxes /
    /// UniFi integration / preview proxies) MUST be merged into
    /// `build_workspace_routes`. That's the only thing keeping it
    /// behind `workspace_middleware`/`local_context_middleware` +
    /// `auth_middleware` — see the cross-workspace write incident
    /// closed by commit `21735f047`. If someone re-merges these routes
    /// at the app root (or under an un-authed mount), this test fails
    /// fast instead of waiting for a real cross-workspace breach.
    #[tokio::test]
    async fn camera_operator_routes_mounted_inside_workspace_tree() {
        let state = test_app_state();
        let router = build_workspace_routes(state.clone(), test_agentic_state(), false, false)
            .into_router()
            .with_state(state);

        // One representative from each operator sub-area. Wildcard
        // routes (HLS / recording) aren't worth poking here — the
        // mount-point check on the non-wildcard variants is enough.
        let cases: &[(&str, &str)] = &[
            ("GET", "/cameras/sites"),
            ("GET", "/cameras/edge-boxes"),
            ("GET", "/cameras"),
            ("POST", "/integrations/unifi/preview"),
        ];
        for (method, path) in cases {
            let status = status_for(router.clone(), method, path).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} must be mounted inside build_workspace_routes (got {status}); \
                 mounting it elsewhere bypasses workspace_middleware + auth_middleware"
            );
        }
    }

    /// The external API surface must expose the FULL anomaly inbox — list,
    /// scan, status, explain — not just the read half. Standalone custom apps
    /// can't reach `/api`, so a missing verb here means the TypeScript SDK's
    /// `client.anomalies.scan()` / `.explain()` 404 against `/external/api`
    /// while the same calls work in the IDE.
    #[tokio::test]
    async fn external_surface_exposes_full_anomaly_inbox() {
        let state = test_app_state();
        let router = build_external_workspace_routes(&state, test_agentic_state())
            .into_router()
            .with_state(state);

        let anomaly_id = "3f2504e0-4f89-11d3-9a0c-0305e82c3301";

        // `POST /{id}/status` was already mounted externally BEFORE this change
        // and is known to work in production, so whatever a bare test router
        // returns for it ("routed fine, failed later on absent workspace
        // context") is the reference for a correctly-resolving route.
        let baseline = status_for(
            router.clone(),
            "POST",
            &format!("/semantic/anomalies/{anomaly_id}/status"),
        )
        .await;
        assert!(
            !baseline.is_success() && baseline != StatusCode::NOT_FOUND,
            "baseline POST /semantic/anomalies/{{id}}/status returned {baseline}; this test \
             assumes a bare router routes the request and then fails on missing context"
        );

        let cases: &[(&str, String)] = &[
            ("GET", "/semantic/anomalies".to_string()),
            ("POST", "/semantic/anomalies/scan".to_string()),
            // Bulk status — `client.anomalies.updateStatusBulk()`. One segment,
            // so it must not be swallowed by the two-segment `/{id}/status`.
            ("POST", "/semantic/anomalies/status".to_string()),
            ("POST", format!("/semantic/anomalies/{anomaly_id}/status")),
            ("POST", format!("/semantic/anomalies/{anomaly_id}/explain")),
        ];
        for (method, path) in cases {
            let status = status_for(router.clone(), method, path).await;
            assert_ne!(
                status,
                StatusCode::NOT_FOUND,
                "{method} {path} must be mounted on the external API surface (got {status}); \
                 custom apps reach anomalies only through /external/api"
            );
            assert_ne!(
                status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} is mounted on the external API surface under a DIFFERENT \
                 verb (got {status}); the SDK calls it with {method}"
            );
            // Equality with the baseline is what rules out a *silently broken*
            // mount: a route that resolves but whose role extractor stopped
            // resolving would diverge here (401/403) while still passing the
            // 404/405 checks above.
            assert_eq!(
                status, baseline,
                "{method} {path} returned {status} but the already-proven \
                 POST /semantic/anomalies/{{id}}/status returned {baseline}. These handlers all \
                 take the same EffectiveWorkspaceRole extractor, so a divergence means this \
                 route resolves differently — check the extractor stack, not just the mount."
            );
        }
    }
}
