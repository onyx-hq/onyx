//! Routes that do not require authentication: health probes, auth endpoints,
//! current-user lookup, and provider-originated webhooks/callbacks (Slack,
//! Toast, and per-app function webhooks).
//!
//! "No auth gate" here means no *user session*, not "no caller check". The
//! webhook receivers authenticate the SENDER instead, by HMAC over the raw
//! body — see `webhooks::app_function`, which fails closed twice (undeclared
//! webhook → 404, unresolvable secret → 401) precisely because it is anonymous.
//!
//! Every route here is `route_fleet`, and that is a declaration of what was
//! already true rather than a change: this file used to mount 51 routes with a
//! bare `.route(`, so every one of them took `classify`'s FleetOk default. The
//! difference is that the default is silent and a declaration is not — and
//! `route_fleet` takes a `MethodRouter<FleetState>`, so a handler added here
//! that asks for `WorkspaceManagerWorkingCopy` now fails to compile instead of
//! shipping as a route a stateless replica cannot serve.
//!
//! None of the current handlers asks for one (checked), which is why the
//! conversion is behaviour-preserving. What it does NOT fix is a handler that
//! reaches for disk without an extractor — `POST /projects/{id}/query` answers
//! `cannot resolve path '.db/'` on a replica while returning rows on the ide,
//! and stays FleetOk deliberately: `role_manifest_tests`'
//! `the_customer_app_data_plane_is_fleet_ok` pins that, and the DuckDB-`local`
//! limitation is recorded in `customer-apps-oxy-starter-fleet.flow.test.yml`.

use axum::routing::{get, post};

use crate::api::{auth, billing, healthcheck, user, webhooks};
use crate::server::api::admin::apps::handlers::{get_build_config, get_org_for_project};
use crate::server::api::{custom_apps_debug, frontline, frontline_devices, projects};

use super::AppState;
use super::role_router::RoleRouter;

pub(super) fn build_public_routes(app_state: &AppState) -> RoleRouter {
    RoleRouter::new(app_state.clone())
        .route_fleet("/health", get(healthcheck::health_check))
        .route_fleet("/ready", get(healthcheck::readiness_check))
        .route_fleet("/live", get(healthcheck::liveness_check))
        .route_fleet("/version", get(healthcheck::version_info))
        // Trusted-publishing OIDC exchange — unauthenticated by construction: the
        // GitHub Actions OIDC JWT in the Authorization header IS the credential.
        // Returns a short-lived, app-scoped publish token. See
        // `custom_apps_publish_oidc`.
        .route_fleet(
            "/customer-apps/publish/oidc-exchange",
            post(crate::api::custom_apps_publish_oidc::oidc_exchange_handler),
        )
        .route_fleet("/auth/config", get(auth::get_config))
        .route_fleet("/auth/session", get(auth::get_session))
        .route_fleet("/auth/oauth/state", post(auth::issue_oauth_state))
        .route_fleet("/auth/google", post(auth::google_auth))
        .route_fleet("/auth/github", post(auth::github_auth))
        .route_fleet("/auth/okta", post(auth::okta_auth))
        .route_fleet("/auth/magic-link/request", post(auth::request_magic_link))
        // Frontline sign-in. Public because a worker has nothing to
        // authenticate with until they have signed in; `route_fleet` because
        // both handlers read and write only Postgres — and because signing in
        // has to survive the ide restarting. Pinning login to the singleton
        // would mean a deploy locks every store out of its own checklists.
        .route_fleet("/frontline/roster", get(frontline::roster))
        .route_fleet("/frontline/login", post(frontline::login))
        // The kiosk binding both of those require. `device` tells the login
        // page whether it is on an enrolled kiosk; `devices/bind` is the
        // one-time enrol link an admin opens on the tablet — GET shows a
        // confirm page, POST binds, because the link travels through things
        // that unfurl URLs. Public for the same reason as login: the tablet
        // has nothing else to present.
        .route_fleet("/frontline/device", get(frontline_devices::device_status))
        .route_fleet(
            "/frontline/devices/bind",
            get(frontline_devices::bind_page).post(frontline_devices::bind_submit),
        )
        .route_fleet("/auth/magic-link/verify", post(auth::verify_magic_link))
        .route_fleet("/auth/return-to/validate", get(auth::validate_return_to))
        // Dev-only sign-in bypass. Public by necessity (it IS the login), but
        // 404s unless an allow-list resolves — `OXY_DEV_LOGIN_EMAILS` in any
        // build, or the staff roster on a debug build for a loopback caller —
        // and only ever issues a session for an address on it. The three rungs
        // and why they differ: `api::auth::dev_login`.
        .route_fleet(
            "/auth/dev-login",
            get(auth::dev_login_get).post(auth::dev_login),
        )
        .route_fleet("/user", get(user::get_current_user_public))
        .route_fleet("/webhooks/stripe", post(billing::webhook::stripe_webhook))
        .route_fleet(
            "/webhooks/toast/orders",
            post(webhooks::toast::toast_order_webhook),
        )
        // Custom-app webhooks. Unauthenticated by design — the sender is proven
        // by an HMAC the PLATFORM verifies against a secret the app's manifest
        // declares, before any app code runs. A function that declares no
        // `webhook:` block is a 404 here, so this cannot be walked to enumerate
        // an app's functions.
        //
        // `route_fleet`: it must answer on every replica. A provider retrying
        // against whichever instance it reaches has no idea which one owns a
        // workspace, and a webhook that only works on the singleton is a webhook
        // that silently drops events.
        .route_fleet(
            "/webhooks/apps/{org_slug}/{app_slug}/{function_name}",
            post(webhooks::app_function::app_function_webhook),
        )
        // Slack-originated traffic. None of these carry a user Authorization
        // header; they're either signature-verified (webhooks) or reached
        // via a browser redirect from slack.com (OAuth callback / magic-link
        // landing page, which uses OptionalAuthenticatedUser to handle
        // logged-in vs logged-out cases itself).
        .route_fleet(
            "/slack/oauth/callback",
            get(crate::integrations::slack::oauth::callback::callback),
        )
        // Public: Intuit redirects the bare browser here after consent. The
        // state nonce (single-use, 15-min TTL) is the CSRF/replay guard.
        .route_fleet(
            "/quickbooks/oauth/callback",
            get(crate::integrations::quickbooks::oauth::callback::callback),
        )
        // Uniform callback for every other provider. Public for the same reason:
        // the vendor redirects a bare browser here with no Oxy session, and the
        // single-use nonce — now also matched on provider — is the guard.
        // QuickBooks keeps its own path above because that URL is registered in
        // every customer's Intuit app and can never be renamed.
        .route_fleet(
            "/oauth/{provider}/callback",
            get(crate::integrations::quickbooks::oauth::callback::callback_by_slug),
        )
        .route_fleet(
            "/slack/events",
            post(crate::integrations::slack::webhooks::events::handle_events_route),
        )
        .route_fleet(
            "/slack/interactivity",
            post(crate::integrations::slack::webhooks::interactivity::handle_interactivity),
        )
        .route_fleet(
            "/slack/link",
            get(crate::integrations::slack::linking::landing::landing),
        )
        .route_fleet(
            "/slack/link/confirm",
            post(crate::integrations::slack::linking::landing::confirm),
        )
        // Public build-config endpoint — no auth required. CI reads this at
        // build time so no per-app env vars need to live in customer-apps repo.
        .route_fleet(
            "/apps/{org_slug}/{app_slug}/build-config",
            get(get_build_config),
        )
        // Public: resolve a workspace's org slug so `oxy publish --project <id>`
        // can bake the /customer-apps/<org>/<app>/ base path without a hardcoded
        // orgSlug (same rationale as build-config — ids/slugs aren't secrets).
        .route_fleet("/org-for-project/{project_id}", get(get_org_for_project))
        // Diagnostic snapshot for admins — what the server sees about
        // this app + its manifest. Same cookie-auth + org-membership
        // gate; human inspection only, shape not guaranteed stable.
        .route_fleet(
            "/customer-apps/{org_slug}/{app_slug}/debug",
            get(custom_apps_debug::get_debug),
        )
        // External liveness for ONE published custom app. In the public router
        // because it authenticates inline (session cookie or bearer token) like
        // its neighbours — and the token is mandatory: auth runs before the app
        // lookup so an unauthenticated caller gets 401 whether or not the app
        // exists, which is what keeps this from becoming an app-enumeration
        // oracle. Two forms: slug-explicit (pollable from the admin host, one
        // monitor for every app) and Host-resolved (`/customer-apps/health` on
        // an `<org>--<slug>.customer-apps.<zone>` subdomain, where `/api/*` is
        // the only prefix the subdomain rewrite passes through). NOT a k8s
        // probe — see the module docs.
        .route_fleet(
            "/customer-apps/{org_slug}/{app_slug}/health",
            get(crate::server::api::custom_apps_health::get_health),
        )
        .route_fleet(
            "/customer-apps/health",
            get(crate::server::api::custom_apps_health::get_health_for_host),
        )
        // The SERVING answer, as opposed to `/health`'s deployment-integrity
        // ladder: the success ratio of real traffic over several windows, plus
        // a multi-window error-budget burn verdict. A ClickHouse read behind
        // the same inline auth gate as its neighbours, so FleetOk — an
        // availability endpoint pinned to the singleton would go dark exactly
        // when that singleton is the thing in trouble.
        .route_fleet(
            "/customer-apps/{org_slug}/{app_slug}/availability",
            get(crate::server::api::custom_apps_availability::get_availability),
        )
        // Per-app debuggability: persisted Oxy Function output, and browser
        // errors with source maps applied server-side. Same inline
        // AUTHENTICATION as their neighbours, but a narrower AUTHORIZATION:
        // both call `require_app_admin`, because function log output is what
        // the author printed while debugging, not the app's own data that
        // every org member may already see. FleetOk — ClickHouse reads plus
        // build-store maps, which every replica has.
        .route_fleet(
            "/customer-apps/{org_slug}/{app_slug}/logs",
            get(crate::server::api::custom_apps_logs::get_logs),
        )
        .route_fleet(
            "/customer-apps/{org_slug}/{app_slug}/errors",
            get(crate::server::api::custom_apps_logs::get_errors),
        )
        // Project-scoped proxies for custom-app bundles. Auth is
        // performed inline (session cookie or API key) so these sit
        // in the public router rather than under workspace middleware.
        // Shared gate chain lives in `custom_apps_gates.rs`.
        .route_fleet(
            "/projects/{project_id}/query",
            post(projects::query::run_query),
        )
        .route_fleet(
            "/projects/{project_id}/semantic-query",
            post(projects::semantic_query::run_semantic_query),
        )
        // Metric-tree analysis ops for bundles — drivers / what-if / RCA /
        // opportunity sizing. Same airlayer analyses as the IDE's workspace
        // `/semantic/metric-tree*` routes, but gated for customer apps and
        // loading the layer from the compile boundary (stateless-fleet safe).
        // SDK exposes via `useMetricTree` / `useSensitivity` / `usePredict` /
        // `useExplain` / `useOpportunity` / `useDistribution` /
        // `useTimeDimensions` / `useBaseline` / `useProjection`.
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree",
            get(projects::metric_tree::get_metric_tree),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/{measure_id}/sensitivity",
            get(projects::metric_tree::get_sensitivity),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/predict",
            post(projects::metric_tree::post_predict),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/explain",
            post(projects::metric_tree::post_explain),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/opportunity",
            post(projects::metric_tree::post_opportunity),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/time-dimensions",
            get(projects::metric_tree::get_time_dimensions),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/baseline",
            post(projects::metric_tree::post_baseline),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/distribution",
            post(projects::metric_tree::post_distribution),
        )
        // Scenario forecasting's time axis: bucketed history + forward curve
        // for the levers and everything downstream. SDK exposes via
        // `useProjection` / `client.metricTree.getProjection`.
        .route_fleet(
            "/projects/{project_id}/semantic/metric-tree/projection",
            post(projects::metric_tree_projection::post_projection),
        )
        // World-model graph + instances + per-instance driver-tree for
        // bundles — the entity/measure map the IDE's World Model surface
        // renders. SDK exposes via `useWorldModel` / `useWorldModelInstances`
        // / `useMeasureBreakdown`.
        // Peer cohorts — cross-sectional comparison (airlayer PR #116). Sibling
        // of the metric-tree ops above: same customer-app gate, same
        // compile-boundary layer. Scoped to the caller's reach, so unlike the
        // population-wide metric-tree surfaces it refuses rather than answering
        // when the entity is not bound to the locations registry.
        .route_fleet(
            "/projects/{project_id}/semantic/cohort",
            post(projects::cohort::post_cohort),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/world-model",
            get(projects::world_model::get_world_model),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/world-model/instances",
            get(projects::world_model::get_world_model_instances),
        )
        .route_fleet(
            "/projects/{project_id}/semantic/world-model/measure-breakdown",
            get(projects::world_model::get_measure_breakdown),
        )
        // Shell bootstrap for `@oxy-hq/sdk/shell` chrome inside bundles:
        // workspace/org identity + published-apps list + host-aware
        // product links. Pure Postgres reads behind the same gate chain →
        // FleetOk (deliberately not pinned in role_manifest.rs).
        .route_fleet(
            "/projects/{project_id}/shell-context",
            get(crate::server::api::custom_apps_shell_context::get_shell_context),
        )
        // Bundle chat history for the SDK Ask dock. Pure Postgres reads
        // behind the same gate chain → FleetOk (not pinned in
        // role_manifest.rs). `/threads` lists the viewer's threads;
        // `/threads/{id}` rebuilds a transcript for restore.
        .route_fleet(
            "/projects/{project_id}/threads",
            get(crate::server::api::custom_apps_threads::list_threads),
        )
        .route_fleet(
            "/projects/{project_id}/threads/{thread_id}",
            get(crate::server::api::custom_apps_threads::get_thread_transcript),
        )
        // Bundle-SDK custom event ingest — `useTrackEvent("name", {...})`.
        // Sits next to /query because both share the same gate chain
        // (cookie or bearer auth + org-member/app-admin access check).
        // See `custom_apps_activity::post_event`.
        .route_fleet(
            "/customer-apps/{project_id}/events",
            post(crate::server::api::custom_apps_activity::post_event),
        )
        // Phase 2 — one-shot ask. POST starts a run, GET polls for state.
        // Bundle SDK exposes both behind `useAsk({agentId})`.
        .route_fleet(
            "/projects/{project_id}/agents/{agent_id}/asks",
            post(projects::agent_ask::start_ask),
        )
        // No GET poll endpoint — the SDK consolidated on
        // `useAgentRun` (SSE) for chat. Bundles that want one-shot
        // ask UX use the same SSE stream and bail out on the first
        // terminal event; bundles that don't want streaming use
        // `useProcedureRun` for their batch case. Removing the
        // poll endpoint keeps the surface small and removes a
        // second source of truth for run state.
        .route_fleet(
            "/projects/{project_id}/agents/asks/{run_id}/cancel",
            post(projects::agent_ask::cancel_ask),
        )
        // Phase 3 — long-running automations. POST starts, GET polls,
        // POST .../cancel stops in-flight. Persistent state in
        // `customer_app_procedure_runs` survives server restarts.
        .route_fleet(
            "/projects/{project_id}/procedures/{procedure_id}/runs",
            post(projects::automation_run::start_automation_run),
        )
        .route_fleet(
            "/projects/{project_id}/procedures/runs/{run_id}",
            get(projects::automation_run::poll_automation_run),
        )
        .route_fleet(
            "/projects/{project_id}/procedures/runs/{run_id}/cancel",
            post(projects::automation_run::cancel_automation_run),
        )
        // Phase 4 — agent-run SSE stream. Same pipeline as `useAsk`
        // but emitting events in real time. Bundle SDK exposes via
        // `useAgentRun({agentId})`.
        .route_fleet(
            "/projects/{project_id}/agents/runs/{run_id}/events",
            get(projects::agent_run_stream::stream_agent_run),
        )
}
