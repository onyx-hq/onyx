//! `/api/admin/*` — Oxy-staff admin surface. The outer guard in
//! `router::global` is `oxy_owner_or_app_admin_guard_middleware` so both
//! OXY_OWNER staff and members of the `app_admins` table can reach most
//! admin features. One subset — billing operations — escalates to a strict
//! OXY_OWNER guard via `route_layer` below; that inner layer runs after the
//! outer permissive check and denies app-admin callers with 403. Every other
//! section escalates to the capability its surface is about, including the
//! airway admission config (`Action::PlatformOperate`).

pub mod airhouse;
pub(crate) mod airway_config;
pub mod app_admins;
pub mod app_publish_tokens;
pub mod apps;
pub mod assume;
pub mod audit;
pub mod billing;
pub mod compiles;
pub mod delegation;
pub mod explorer;
pub mod internal_jobs;
pub mod metrics;
pub mod oltp;
pub mod org_subdomains;
pub mod orgs_admin;
pub mod oxy_access;
pub mod partners;
pub mod routing;
pub mod scope;
pub mod users_admin;
// `pub(crate)`: the module's handlers stay internal — only the two readers the
// extracted `oxy-api-partner-console` surface needs are re-exported below, matching
// the narrower `admin::apps` pattern (export the fns, not the whole module).
pub(crate) mod workspace_health;
pub mod workspaces_admin;

pub use workspace_health::{WorkspaceHealthRow, health_rollup};

use axum::Router;
use axum::middleware;

use oxy_shared::fleet_role::{RouteRole, RouteRoleDecl};

use crate::server::api::middlewares::{app_scope_guard, oxy_owner_guard, platform_cap_guard};
use crate::server::authz::Action;
use crate::server::feature_flags;
use crate::server::router::AppState;

/// Admin routes are flat under `/api/admin/*` after the 2026-04-28 redesign.
/// Endpoints:
///   - GET    /admin/orgs?status=...
///   - GET    /admin/billing/prices
///   - GET    /admin/orgs/{org_id}/billing/subscription
///   - POST   /admin/orgs/{org_id}/billing/provision-subscription
///   - POST   /admin/orgs/{org_id}/billing/provision-checkout
///   - GET    /admin/orgs/{org_id}/billing/checkout
///   - POST   /admin/orgs/{org_id}/billing/checkout/resend
///   - POST   /admin/orgs/{org_id}/billing/checkout/cancel
///   - POST   /admin/orgs/{org_id}/billing/resync
///   - GET    /admin/feature-flags
///   - PATCH  /admin/feature-flags/{key}
///   - POST   /admin/apps
///   - GET    /admin/apps
///   - GET    /admin/apps/{id}
///   - PATCH  /admin/apps/{id}
///   - DELETE /admin/apps/{id}
///   - GET    /admin/app-admins
///   - POST   /admin/app-admins
///   - DELETE /admin/app-admins/{id}
///   - GET    /admin/internal-jobs/queue-stats
///   - GET    /admin/internal-jobs/recent-failures
///   - GET    /admin/internal-jobs/dead-letter
///   - POST   /admin/internal-jobs/dead-letter/{task_id}/reenqueue
///   - DELETE /admin/internal-jobs/dead-letter/{task_id}
///   - GET    /admin/internal-jobs/workers
///   - GET    /admin/internal-jobs/scheduled
///   - POST   /admin/internal-jobs/run-reaper
///   - GET    /admin/orgs-meta
///   - GET    /admin/orgs/{org_id}/detail
///   - PATCH  /admin/orgs/{org_id}
///   - DELETE /admin/orgs/{org_id}
///   - POST   /admin/orgs/{org_id}/transfer-ownership
///   - GET    /admin/users
///   - GET    /admin/users/{user_id}
///   - PATCH  /admin/users/{user_id}/status
///   - GET    /admin/users/{user_id}/org-memberships
///   - POST   /admin/users/{user_id}/org-memberships
///   - PATCH  /admin/users/{user_id}/org-memberships/{org_id}
///   - DELETE /admin/users/{user_id}/org-memberships/{org_id}
///   - GET    /admin/workspaces-meta
///   - GET    /admin/workspaces/{workspace_id}/detail
///   - PATCH  /admin/workspaces/{workspace_id}
///   - DELETE /admin/workspaces/{workspace_id}
///   - POST   /admin/workspaces/{workspace_id}/transfer-org
///   - POST   /admin/workspace-health/{workspace_id}/eval
///   - GET    /admin/airway/config
///   - PUT    /admin/airway/config/{source_kind}
///   - DELETE /admin/airway/config/{source_kind}
///   - PUT    /admin/airway/config/{source_kind}/workspaces/{workspace_id}
///   - DELETE /admin/airway/config/{source_kind}/workspaces/{workspace_id}
///   - GET    /admin/airway/config/{source_kind}/preview
///   - GET    /admin/airway/deployment-config
///   - PUT    /admin/airway/deployment-config
///   - DELETE /admin/airway/deployment-config
///   - GET    /admin/airhouse
///   - POST   /admin/workspaces/{workspace_id}/airhouse/provision
///   - GET    /admin/oltp
///   - GET    /admin/orgs/{org_id}/oltp
///   - POST   /admin/orgs/{org_id}/oltp/provision
///   - POST   /admin/orgs/{org_id}/oltp/credentials
///   - POST   /admin/orgs/{org_id}/oltp/visibility
///   - POST   /admin/orgs/{org_id}/oltp/deprovision-writer
///   - DELETE /admin/orgs/{org_id}/oltp
///
/// Admin routes. The outer nest layer in `router::global` is the **door**
/// (`oxy_owner_or_app_admin_guard`): it answers "are you Oxy staff at all". Each
/// sub-router below then escalates to the capability its surface is actually about,
/// via `route_layer` — the inner layer runs after the outer one, so a request that
/// passed the door still gets a 403 here without the capability.
///
/// That per-section escalation is not new machinery: `billing` and `app_admins` have
/// always escalated to strict OXY_OWNER this way. What changed is that the *rest* of
/// the console used to escalate to nothing, so any staff standing reached all of it —
/// which is why an app publisher had the same authority as someone entitled to delete
/// a tenant. `require(Action::Platform*)` generalises the pattern that was already here.
///
/// One gate remains owner-only rather than capability-gated, deliberately:
/// * `billing` — the Billing queue, `Ring::GlobalOwnerOnly`.
///
/// `app_admins` — the **grant table itself** — used to be the second, on the reasoning
/// that "a capability that could edit the grant table would let its holder widen their
/// own grant, and the ceiling would mean nothing". That objection is real and is
/// answered by bounding the write rather than withholding the capability: `may_delegate`
/// admits only a grant strictly weaker than the writer's own, so the one row a holder
/// can never touch is their own, and only the owner can mint a peer. See
/// `admin::delegation`. **The capability gate here is a door, not the control** — the
/// handlers carry the row-level half (`actor_facts` once, then `refuse(may_delegate(..))`
/// per row), exactly as scope works.
///
/// **Scope is not enforced here** — see `platform_cap_guard`. A scoped operator passes
/// these gates and the handler filters its rows.
///
/// `airway_config` is deliberately *not* a third owner-only gate: it is deployment-wide
/// operational config, so it takes `Action::PlatformOperate` beside `workspace_health` /
/// `routing` / `metrics`. The two exceptions above are exceptions for reasons that do not
/// apply to it — billing is `Ring::GlobalOwnerOnly`, and `app_admins` is the grant table
/// itself, where a capability could widen its own holder.
///
/// `internal_jobs::router()` is mounted separately at `/admin/internal-jobs`
/// in `router::global` because its routes were flattened during the
/// app-admin opening.
pub(crate) fn router() -> Router<AppState> {
    // route_layer applied per sub-router so only billing gets the strict
    // owner guard; everything else escalates to a capability, and anything
    // not named here runs only under the outer permissive guard.
    let strict = middleware::from_fn(oxy_owner_guard::oxy_owner_guard_middleware);
    let cap = |action| middleware::from_fn(platform_cap_guard::require(action));

    // The staff surface. Everything here is refused while the caller is acting as
    // a tenant: you cannot wield staff powers and wear a customer's identity in
    // the same breath. Ending the session (below) restores all of it.
    let staff_surface = feature_flags::routes::router()
        .route_layer(cap(Action::PlatformOperate))
        // Two layers, two questions: the capability admits you to the section, then the
        // scope guard fences which apps you may touch inside it. Layered here rather
        // than in ~20 handlers — see `app_scope_guard`.
        .merge(
            apps::router()
                .route_layer(middleware::from_fn(app_scope_guard::enforce_app_scope))
                .route_layer(cap(Action::PlatformApps)),
        )
        .merge(audit::router().route_layer(cap(Action::PlatformAudit)))
        .merge(app_publish_tokens::router().route_layer(cap(Action::PlatformApps)))
        .merge(explorer::router().route_layer(cap(Action::PlatformExplorer)))
        .merge(metrics::router().route_layer(cap(Action::PlatformOperate)))
        // Org administration and creation are one router but two capabilities; the
        // router-level gate is the broader `PlatformOrgs`, and `create_org` asks for
        // `PlatformOrgCreate` inside the handler where the verb is known.
        .merge(orgs_admin::router().route_layer(cap(Action::PlatformOrgs)))
        .merge(org_subdomains::router().route_layer(cap(Action::PlatformOrgs)))
        .merge(users_admin::router().route_layer(cap(Action::PlatformUsers)))
        .merge(workspaces_admin::router().route_layer(cap(Action::PlatformOrgs)))
        .merge(routing::router().route_layer(cap(Action::PlatformOperate)))
        // Per-org OLTP: provisioning creates a billable project at the
        // provider, so it sits behind `OperatePlatform`, not the staff door —
        // an App Operator ships apps and must not spend money here.
        // The scope-fenced shim, NOT `oxy_oltp::api::admin::router()` directly.
        // `cap(..)` decides on a nil-org resource, so it lets a bounded
        // `global_admin` through for every org; `oltp::router` adds the
        // per-org fence. See that module's header.
        .merge(oltp::router().route_layer(cap(Action::PlatformOltp)))
        // Same RING as OLTP — both are "operate the platform's data plane on a
        // tenant's behalf" — but its own ACTION, because the action name is what
        // lands in an audit row and in the grant UI.
        .merge(airhouse::router().route_layer(cap(Action::PlatformAirhouse)))
        .merge(workspace_health::router().route_layer(cap(Action::PlatformOperate)))
        .merge(partners::router().route_layer(cap(Action::PlatformPartners)))
        .merge(billing::router().route_layer(strict))
        .merge(app_admins::router().route_layer(cap(Action::PlatformGrants)))
        // Deployment-wide operational config — which source resources every
        // tenant's pipelines may use. Same shape as workspace_health /
        // routing / metrics, so it takes the same capability rather than
        // becoming a second owner-only gate beside billing.
        //
        // The per-workspace half of this surface carries its own scope fence,
        // because this layer cannot: `Resource::platform()` has no org, so a
        // bounded grant passes every capability gate here (see
        // `platform_cap_guard`). `airway_config::handlers` fences both override
        // writes with `deny_out_of_scope_for_workspace` and filters the
        // overrides `get_config` returns; `airway_config::preview` narrows the
        // cross-tenant pipeline scan with the same `scope_org_filter` and
        // reports the withheld remainder as a bare count.
        //
        // KNOWN LIMITATION, accepted rather than overlooked: the **global row
        // stays fleet-wide**. A grant bounded to two orgs can still `PUT
        // /airway/config/{kind}` and change the policy every tenant's pipelines
        // resolve against. There is nothing to fence it on — the global row is
        // `workspace_id IS NULL`, it belongs to no org, and a scope check has
        // no org to ask about. The three ways to close it were each worse than
        // the gap: split the global row behind `Ring::GlobalOwnerOnly` (a
        // third owner-only gate, re-creating exactly the escalation this mount
        // removed, on the field operators most need to reach); refuse the
        // global write for any bounded grant (the surface then silently does
        // half of what its UI shows, for the operators most likely to use it);
        // or make the row per-org (a schema change that contradicts what the
        // row IS — airway resolves admission per source kind, fleet-wide, in
        // `agentic_pipeline::airway_config::resolve_admission`).
        //
        // What makes it acceptable: `PlatformOperate` is already the capability
        // for fleet-wide operational config (`routing`, `metrics`,
        // `workspace_health`), a scoped grant is issued to staff, not tenants,
        // and the save-confirmation gate in front of this write previews the
        // fleet-wide blast radius before it happens. If that stops holding, the
        // per-org row is the direction to take — not a second owner-only gate.
        .merge(airway_config::router().route_layer(cap(Action::PlatformOperate)))
        .route_layer(middleware::from_fn(assume::block_admin_while_acting));

    // Assume-role itself lives at `/api/assume`, NOT here — see `assume::router`.
    // It has to be reachable while acting (that's where the exit is) and by
    // partners (who are not staff and would be 403'd by this surface's guard).
    staff_surface
}

/// Which pod serves each route [`router`] mounts, relative to its `/admin` nest.
///
/// FleetOk wholesale: the console is Postgres CRUD, and the one filesystem read
/// under it (`reconcile.yml`, for workspace health) runs on the worker, not the
/// request path. The exceptions are declared beside their routes and win on
/// specificity over the wildcard.
pub(crate) fn router_roles() -> &'static [RouteRoleDecl] {
    const ROLES: &[RouteRoleDecl] = &[
        RouteRoleDecl {
            method: "*",
            path: "",
            role: RouteRole::FleetOk,
        },
        RouteRoleDecl {
            method: "*",
            path: "/{*rest}",
            role: RouteRole::FleetOk,
        },
        orgs_admin::CREATE_ORG_ROLE,
    ];
    ROLES
}

#[cfg(test)]
mod tests {
    //! Regression tests for the route-layer escalation pattern used by `billing`.
    //! These pin the property that even when the outer permissive layer in
    //! `router::global` admits a Global Admin (a non-Owner staff member), a route
    //! nested under `route_layer(oxy_owner_guard_middleware)` still rejects with 403.
    //!
    //! `app_admins` was the second such surface and is no longer: it is capability-gated
    //! plus row-fenced (see `admin::delegation`). These tests build their own router, so
    //! they cannot notice that change — which is exactly why the assertion that the real
    //! mount matches lives in `crates/app/tests/app_scope_boundary.rs` as a source scan.
    //! A fixture that mounts its own probe under `strict` proves the *middleware* works
    //! and nothing whatsoever about what ships.
    //!
    //! The escalation does NOT touch the database — `oxy_owner_guard`
    //! consults only the `OXY_OWNER` env var — so we can pin the layering
    //! behavior with a plain Tower service test rather than a full
    //! integration harness.
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use entity::users::UserStatus;
    use oxy_auth::types::AuthenticatedUser;
    use tower::ServiceExt;
    use uuid::Uuid;

    /// Set / unset env vars for the duration of one test. Necessary
    /// because the guard reads `OXY_OWNER` at request time, not boot.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn stub_user(email: &str) -> AuthenticatedUser {
        AuthenticatedUser {
            id: Uuid::new_v4(),
            email: Some(email.to_string()),
            name: "stub".to_string(),
            picture: None,
            status: UserStatus::Active,
        }
    }

    /// Build the same router shape as `admin::router()` minus the leaf
    /// handlers (DB-heavy). One stub handler per surface so we can assert
    /// per-path response codes.
    fn test_router() -> Router {
        let strict = middleware::from_fn(oxy_owner_guard::oxy_owner_guard_middleware);
        let billing = Router::new()
            .route("/billing/probe", get(|| async { StatusCode::OK }))
            .route_layer(strict.clone());
        // `partners` and `app-admins` mount WITHOUT the strict layer — a Global Admin
        // must reach both. (Partners is the regression guard for the 403-on-partners
        // bug; app-admins is capability-gated now, with the delegation bound doing the
        // narrowing inside the handler rather than at the door.)
        let open = Router::new()
            .route("/feature-flags/probe", get(|| async { StatusCode::OK }))
            .route("/partners/probe", get(|| async { StatusCode::OK }))
            .route("/app-admins/probe", get(|| async { StatusCode::OK }));
        Router::new().merge(billing).merge(open)
    }

    async fn request_as(router: Router, path: &str, user: Option<AuthenticatedUser>) -> StatusCode {
        let mut req = Request::builder().uri(path).body(Body::empty()).unwrap();
        if let Some(u) = user {
            req.extensions_mut().insert(u);
        }
        router.oneshot(req).await.unwrap().status()
    }

    /// A non-owner caller gets 403 on billing, but 200 on the non-escalated paths.
    /// This pins the route_layer escalation itself.
    #[tokio::test]
    async fn strict_layer_rejects_non_owner_on_billing() {
        let _g = EnvGuard::set("OXY_OWNER", "owner@example.com");
        let app = test_router();

        let admin = stub_user("admin@example.com");

        assert_eq!(
            request_as(app.clone(), "/billing/probe", Some(admin.clone())).await,
            StatusCode::FORBIDDEN,
            "Global Admin must NOT reach billing through the strict route_layer"
        );
        assert_eq!(
            request_as(app.clone(), "/app-admins/probe", Some(admin.clone())).await,
            StatusCode::OK,
            "app_admins is capability-gated, not owner-strict: a Global Admin reaches \
             the console and `may_delegate` decides which rows they may write"
        );
        assert_eq!(
            request_as(app.clone(), "/partners/probe", Some(admin.clone())).await,
            StatusCode::OK,
            "Global Admin MUST reach partners — provisioning is not owner-strict"
        );
        assert_eq!(
            request_as(app, "/feature-flags/probe", Some(admin)).await,
            StatusCode::OK,
            "non-escalated paths stay reachable by any authenticated caller"
        );
    }

    /// Sanity: an Owner reaches every surface — confirms the strict
    /// layer doesn't accidentally reject owners.
    #[tokio::test]
    async fn strict_layer_allows_owner_everywhere() {
        let _g = EnvGuard::set("OXY_OWNER", "owner@example.com");
        let app = test_router();
        let owner = stub_user("owner@example.com");

        for path in [
            "/billing/probe",
            "/app-admins/probe",
            "/feature-flags/probe",
        ] {
            assert_eq!(
                request_as(app.clone(), path, Some(owner.clone())).await,
                StatusCode::OK,
                "Global Owner must reach {path}"
            );
        }
    }

    /// Missing auth extension → 401 from the strict guard. Protects
    /// against a deploy that accidentally drops the auth middleware.
    #[tokio::test]
    async fn strict_layer_rejects_unauthenticated() {
        let _g = EnvGuard::set("OXY_OWNER", "owner@example.com");
        let app = test_router();

        assert_eq!(
            request_as(app, "/billing/probe", None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    fn test_app_state() -> AppState {
        AppState {
            enterprise: false,
            internal: false,
            mode: oxy_app_core::serve_mode::ServeMode::Cloud,
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

    /// The same guard for the per-org OLTP routes, which shipped 404ing.
    ///
    /// They were declared as `/admin/orgs/{id}/oltp` while `admin::router()` is
    /// already nested under `/admin` in `router::global` — so the real path was
    /// `/admin/admin/orgs/...`. It compiled, the handler was reachable in
    /// principle, and every request 404'd. Nothing but driving the real router
    /// catches a doubled prefix.
    #[tokio::test]
    async fn oltp_admin_routes_are_mounted_on_the_real_router() {
        let real_router = router().with_state(test_app_state());
        let org = "d9830be4-c6a4-4c1c-9c1e-000000000002";

        for (method, path) in [
            // All seven, not the original three. The three that matter most to
            // get wrong sit together: visibility grants analytics read access to
            // a tenant's live tables, `deprovision-writer` drops one app's
            // schema, and the DELETE destroys the whole database. The last two
            // are one path segment apart and differ by the entire tenant, which
            // is why both are pinned here rather than trusted to review.
            // `GET /oltp` additionally changed crates when the scope-fenced shim
            // took over the mount.
            ("GET", "/oltp".to_string()),
            ("GET", format!("/orgs/{org}/oltp")),
            ("POST", format!("/orgs/{org}/oltp/provision")),
            ("POST", format!("/orgs/{org}/oltp/credentials")),
            ("POST", format!("/orgs/{org}/oltp/visibility")),
            ("POST", format!("/orgs/{org}/oltp/deprovision-writer")),
            ("DELETE", format!("/orgs/{org}/oltp")),
            // Airhouse rides the same mount and would 404 the same way.
            ("GET", "/airhouse".to_string()),
            ("POST", format!("/workspaces/{org}/airhouse/provision")),
        ] {
            let req = Request::builder()
                .method(method.as_ref() as &str)
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            let resp = real_router.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} must be mounted on admin::router() — a 404 here means the \
                 merge is missing or the route carries its own /admin prefix"
            );
        }

        // Control: the doubled-prefix shape must NOT match, or the assertions
        // above would pass for the very bug they exist to catch.
        let doubled = Request::builder()
            .uri(format!("/admin/orgs/{org}/oltp").as_str())
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            real_router.oneshot(doubled).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "the /admin/admin/... shape must not be mounted"
        );
    }

    /// The route list in this file's header must name every route the fenced
    /// shims actually mount.
    ///
    /// That header is the inventory an operator greps, and it drifted twice in
    /// one review — the routes were missing from it, and the path recorded in
    /// `internal-docs/admin-surfaces.md` was `/admin/airhouse/{id}/provision`
    /// when the real one is `/admin/workspaces/{id}/airhouse/provision`. A
    /// wrong path in an inventory is worse than an absent one: it reads as
    /// checked.
    #[test]
    fn the_header_inventory_names_every_shim_route() {
        for shim in [include_str!("airhouse.rs"), include_str!("oltp.rs")] {
            assert_header_lists_routes_of(shim);
        }
    }

    fn assert_header_lists_routes_of(shim: &str) {
        // Everything above `router()`: the module list and the route inventory
        // that documents it. Deliberately NOT the whole file — the tests below
        // quote paths in their own doc comments, and searching those would let
        // this pass on the strength of a comment about the bug.
        let header = include_str!("mod.rs")
            .split_once("pub(crate) fn router()")
            .expect("the inventory sits above `router()`")
            .0;
        assert!(
            header.contains("/admin/orgs"),
            "the header slice missed the route inventory entirely"
        );

        // Every `"/..."` literal inside the shim's `router()`. Matching
        // `.route("` misses the shape rustfmt gives a long call — the path
        // ends up on its own line — which would have left this guard checking
        // one of the two routes and passing.
        let body = shim
            .split_once("pub fn router<S>()")
            .expect("the shim mounts its routes in `router`")
            .1;
        let body = body.split_once("\nasync fn ").map_or(body, |(b, _)| b);
        let mounted: Vec<&str> = body
            .match_indices('"')
            .filter_map(|(i, _)| {
                let rest = &body[i + 1..];
                let end = rest.find('"')?;
                let lit = &rest[..end];
                lit.starts_with('/').then_some(lit)
            })
            .collect();
        assert!(
            !mounted.is_empty(),
            "the `.route(..)` scan matched nothing — the mount syntax changed \
             and this guard is now vacuous"
        );
        for path in mounted {
            // `admin::router()` supplies the `/admin` prefix at the mount, and
            // the header records the externally visible path — which is the
            // one an operator greps for and the one the docs got wrong.
            let external = format!("/admin{path}");
            assert!(
                header.contains(&external),
                "`{external}` is mounted but absent from the route list at the \
                 top of this file"
            );
        }
    }

    /// The Airhouse admin routes, driven through the REAL `router()`.
    ///
    /// `test_router()` elsewhere in this file is a hand-rolled stub that
    /// re-declares probe routes, so it cannot catch a missing `.merge(..)`
    /// line. A 404 on a real path means the route is not mounted; the bogus
    /// sibling below stays 404 as the control, because `route_layer` only wraps
    /// MATCHED routes and an unmatched path bypasses it to axum's fallback.
    #[tokio::test]
    async fn airhouse_admin_routes_are_mounted_on_the_real_router() {
        let real_router = router().with_state(test_app_state());
        let ws = "3c6e0b8a-9c15-224a-8236-000000000001";

        for (method, path) in [
            ("GET", "/airhouse".to_string()),
            ("POST", format!("/workspaces/{ws}/airhouse/provision")),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            let resp = real_router.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} is not mounted"
            );
        }

        // The control: an unmounted sibling must still 404, or the assertions
        // above would pass for the very bug they exist to catch.
        let bogus = Request::builder()
            .uri("/airhouse/not-a-route")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            real_router.oneshot(bogus).await.unwrap().status(),
            StatusCode::NOT_FOUND,
            "an unmounted path must 404, otherwise this test proves nothing"
        );
    }

    /// Proves `airway_config` is mounted on the REAL `admin::router()` tree —
    /// not just a compiling-but-orphaned module. `test_router()` above is a
    /// hand-rolled stub (re-declares probe routes rather than calling
    /// `router()`), so it can't catch "the merge line is missing." This test
    /// calls the actual `router()` function this file exports and drives a
    /// request through it with `tower::ServiceExt::oneshot`.
    ///
    /// No `OXY_OWNER` / auth setup needed: `block_admin_while_acting`'s
    /// `AuthenticatedUserExtractor` rejects with 401 *before* the handler or
    /// the inner strict guard ever runs (no DB call either — extraction
    /// fails on the missing request extension). `route_layer` — used by both
    /// that layer and the strict OXY_OWNER escalation — only wraps MATCHED
    /// routes, so an unmatched path bypasses it entirely and axum's fallback
    /// 404s directly. That's the discriminator: 404 on a real path means the
    /// route isn't mounted; a bogus sibling path stays 404 as the control.
    ///
    /// Covers Task 1's read route (`GET /config`), Task 2's four write routes
    /// (`PUT`/`DELETE` on both the global and per-workspace-override path
    /// shapes), and Task 3's `GET /config/{kind}/preview` — a route that
    /// compiles but was never added to `airway_config::router()`'s
    /// `.route(...)` chain is exactly the failure this test exists to catch,
    /// and that applies just as much to a new write or preview route as it did
    /// to the original read one.
    #[tokio::test]
    async fn airway_config_is_mounted_on_the_real_router() {
        let real_router = router().with_state(test_app_state());
        let ws = "d9830be4-c6a4-4c1c-9c1e-000000000001";

        for (method, path) in [
            ("GET", "/airway/config".to_string()),
            ("PUT", "/airway/config/toast".to_string()),
            ("DELETE", "/airway/config/toast".to_string()),
            ("PUT", format!("/airway/config/toast/workspaces/{ws}")),
            ("DELETE", format!("/airway/config/toast/workspaces/{ws}")),
            ("GET", "/airway/config/toast/preview".to_string()),
            // The operational tier. A sibling of `/config`, so it is also the
            // case where a bad `.nest`/`.route` shape would have it matched by
            // `/config/{source_kind}` instead of its own handler.
            ("GET", "/airway/deployment-config".to_string()),
            ("PUT", "/airway/deployment-config".to_string()),
            ("DELETE", "/airway/deployment-config".to_string()),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(&path)
                .body(Body::empty())
                .unwrap();
            let resp = real_router.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} must be mounted on admin::router() — got 404, so the \
                 merge in router() is missing or the path doesn't match"
            );
        }

        let bogus = Request::builder()
            .uri("/airway/does-not-exist")
            .body(Body::empty())
            .unwrap();
        let bogus_resp = real_router.oneshot(bogus).await.unwrap();
        assert_eq!(
            bogus_resp.status(),
            StatusCode::NOT_FOUND,
            "control: an unregistered sibling path must still 404, or the assertion \
             above isn't actually discriminating"
        );
    }
}
