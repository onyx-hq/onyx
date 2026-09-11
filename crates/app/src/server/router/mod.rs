//! HTTP router, split into per-concern modules.
//!
//! - [`public`] — unauthenticated routes (health, auth, Slack, current-user)
//! - [`global`] — cloud-only org/workspace CRUD and GitHub integration
//! - [`workspace`] — the per-workspace route tree and its sub-builders
//! - [`secrets`] — secret routes gated behind an admin-only middleware
//! - [`protected`] — cloud/local composition of protected routes + middleware
//! - [`entry`] — [`api_router`] / [`internal_api_router`] public entry points
//! - [`openapi`] — the utoipa OpenAPI router used by Swagger UI

mod entry;
mod global;
mod openapi;
mod protected;
mod public;
pub(crate) mod recovery;
pub(crate) mod role_router;
mod secrets;
mod workspace;
pub(crate) mod workspace_cache;

use axum::Router;
use axum::extract::FromRequestParts;
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use entity::workspaces as workspace_entity;
use std::future::Future;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub use entry::{api_router, internal_api_router};
pub use openapi::{build_openapi_doc, openapi_router};

// `AppState` moved to `oxy-app-core` so the router and future per-surface crates
// can hold it without depending on `oxy-app`. Re-exported here so every existing
// `crate::server::router::AppState` call site (~52 files) is unchanged.
pub use oxy_app_core::AppState;

/// An `AppState` carrying nothing but its caches. The router needs a state value
/// to finish each route's handlers with, and a test that mounts one handler
/// needs the same — the semantic caches are `pub(crate)`, so it cannot build one
/// itself.
pub fn bare_app_state() -> AppState {
    AppState {
        enterprise: false,
        internal: false,
        mode: oxy_app_core::serve_mode::ServeMode::Cloud,
        observability: None,
        startup_cwd: std::path::PathBuf::new(),
        preagg_cache: None,
        preagg_renewal_threshold_secs: None,
        agentic_state: None,
        semantic_layer_cache: workspace_cache::new_semantic_layer_cache(),
        semantic_engine_cache: workspace_cache::new_semantic_engine_cache(),
    }
}

/// An `AgenticState` over a disconnected database, for tests that need to build
/// a router but never reach one.
#[cfg(test)]
pub(crate) fn test_agentic_state() -> std::sync::Arc<agentic_http::AgenticState> {
    use agentic_pipeline::platform::ThreadOwnerLookup;

    struct NoThreadOwner;

    #[async_trait::async_trait]
    impl ThreadOwnerLookup for NoThreadOwner {
        async fn thread_owner(
            &self,
            _thread_id: uuid::Uuid,
        ) -> Result<Option<Option<uuid::Uuid>>, String> {
            Ok(None)
        }
    }

    std::sync::Arc::new(agentic_http::AgenticState::new(
        tokio_util::sync::CancellationToken::new(),
        sea_orm::DatabaseConnection::default(),
        std::sync::Arc::new(NoThreadOwner),
    ))
}

/// Build the protected router purely to read back the roles it declared.
///
/// The server installs the real build's declarations at startup, for the mode it
/// is running; this exists for the guards that assert policy about routes, which
/// need the routes themselves rather than a description of them. It unions both
/// modes, because git features and local setup are mounted by different ones and
/// a guard has to see every route the server can serve.
pub fn route_declarations() -> Vec<role_router::Decl> {
    use agentic_http::AgenticState;
    use agentic_pipeline::platform::ThreadOwnerLookup;
    use std::sync::Arc;

    struct NoThreadOwner;

    #[async_trait::async_trait]
    impl ThreadOwnerLookup for NoThreadOwner {
        async fn thread_owner(
            &self,
            _thread_id: uuid::Uuid,
        ) -> Result<Option<Option<uuid::Uuid>>, String> {
            Ok(None)
        }
    }

    let app_state = bare_app_state();
    let agentic_state = Arc::new(AgenticState::new(
        tokio_util::sync::CancellationToken::new(),
        sea_orm::DatabaseConnection::default(),
        Arc::new(NoThreadOwner),
    ));
    // No surface crates here: this collects oxy-app's OWN declarations for the
    // tests, and a seam's routes are the composition root's to supply.
    let (_, cloud) = protected::build_protected_routes(
        app_state.clone(),
        agentic_state.clone(),
        Router::new(),
        Vec::new(),
    );
    let (_, local) = protected::build_local_protected_routes(
        app_state.clone(),
        agentic_state,
        Router::new(),
        Vec::new(),
    );
    // The public tree is served in BOTH modes and now declares its own routes,
    // so the guards have to see it too — otherwise `classify` answers correctly
    // at runtime while every test still reads the FleetOk default and cannot
    // tell a declaration from an omission.
    let (_, public, _) = public::build_public_routes(&app_state).into_parts();
    cloud
        .into_iter()
        .chain(local)
        .chain(crate::server::role_manifest::api_prefixed(public))
        .collect()
}

/// The two halves of the split. A route's role is a permission — `IdeState`
/// means "this pod has a working copy, so a handler here may reach for one" —
/// and the compiler enforces it: `WorkspaceManagerWorkingCopy` resolves only from
/// `IdeState`, so a handler that asks for a working copy cannot be mounted on a
/// fleet route. `AppState` comes from either, so `State<AppState>` handlers are
/// unaffected.
#[derive(Clone)]
pub struct IdeState(pub AppState);

#[derive(Clone)]
pub struct FleetState(pub AppState);

impl axum::extract::FromRef<IdeState> for AppState {
    fn from_ref(state: &IdeState) -> Self {
        state.0.clone()
    }
}

impl axum::extract::FromRef<FleetState> for AppState {
    fn from_ref(state: &FleetState) -> Self {
        state.0.clone()
    }
}

#[derive(Clone)]
pub struct WorkspaceExtractor(pub workspace_entity::Model);

impl<S> FromRequestParts<S> for WorkspaceExtractor
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let result = parts
            .extensions
            .get::<workspace_entity::Model>()
            .cloned()
            .map(WorkspaceExtractor)
            .ok_or(StatusCode::UNAUTHORIZED);

        async move { result }
    }
}

/// CORS policy for the API surface.
///
/// Customer-app bundles share the SPA's domain in the current model, so
/// real cross-origin browser XHR only happens in local dev (Vite on
/// `:5173` / `:5174` calling oxy on `:3000` when not proxied). Auto-allow
/// that pair plus any same-origin request derived from `Host` /
/// `X-Forwarded-Host`. No env-var setup needed for local or cloud
/// deployments. (When whitelabelling lands, per-app allowed origins
/// belong in the DB, not a global env var.)
pub(crate) fn build_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_private_network(true)
        .allow_origin(AllowOrigin::predicate(|origin, parts| {
            cors_allow(origin, &parts.headers)
        }))
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
            Method::HEAD,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::ACCEPT_LANGUAGE,
            header::ORIGIN,
            HeaderName::from_static("x-requested-with"),
            HeaderName::from_static("x-request-id"),
            // W3C trace context the custom-app SDK sends on every function
            // invoke; same-origin today, but a cross-origin base would
            // otherwise fail its preflight exactly as documented below.
            HeaderName::from_static("traceparent"),
            // Cross-origin SSE for the custom-app shell's Ask Oxygen stream.
            // Its fetch sends `Cache-Control: no-cache` (don't cache the stream)
            // and, on reconnect, `Last-Event-ID` (replay from a known point).
            // Neither is CORS-safelisted, so each triggers a preflight; without
            // them here the browser blocks the request and the run reports
            // "Failed to fetch". Same headers the external CORS layer allows.
            header::CACHE_CONTROL,
            HeaderName::from_static("last-event-id"),
        ])
        // `Link` carries `rel="next"` on the paginated list endpoints
        // (`oxy_app_core::pagination`). A response header is invisible to a
        // cross-origin browser caller unless it is exposed, so without this a
        // custom-app subdomain reading `/api` would page one page and stop —
        // the exact silence that header exists to break. Server-side callers
        // (`oxyc`, curl) are unaffected either way.
        // `x-oxy-request-id` for the same reason, and it matters more: the whole
        // point of the id is that a caller can name the request that went
        // wrong. Devtools and curl show it either way, but a custom app on
        // `<org>--<slug>.customer-apps…` calling `/api` is cross-origin, so
        // `response.headers.get('x-oxy-request-id')` reads null there without
        // this — which is exactly the client that would report it.
        .expose_headers([
            header::LINK,
            HeaderName::from_static(crate::server::api::middlewares::request_id::REQUEST_ID_HEADER),
        ])
}

/// Wide-open CORS for the EXTERNAL API surface (`/external/api/*`).
///
/// Allows ANY origin so a standalone app on any domain (e.g. a Vercel
/// dashboard) can call oxy with `X-API-Key`. This is safe ONLY because that
/// surface is gated by [`oxy_auth::middleware::api_key_only_middleware`] —
/// API-key auth carries no ambient browser credential, so `*`-origin has no
/// CSRF vector. Critically we must NOT set `allow_credentials(true)` here: the
/// CORS spec forbids credentials with a wildcard origin, and there are no
/// cookies to send anyway. Kept entirely separate from [`build_cors_layer`]
/// (the locked-down, cookie-credentialed policy for the main `/api` surface).
pub(crate) fn build_external_cors_layer() -> CorsLayer {
    use tower_http::cors::Any;
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
            Method::HEAD,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::ACCEPT_LANGUAGE,
            header::ORIGIN,
            HeaderName::from_static("x-requested-with"),
            HeaderName::from_static("x-request-id"),
            // W3C trace context the custom-app SDK sends on every function
            // invoke; same-origin today, but a cross-origin base would
            // otherwise fail its preflight exactly as documented below.
            HeaderName::from_static("traceparent"),
            HeaderName::from_static("x-api-key"),
            HeaderName::from_static("last-event-id"),
        ])
        // Same reason as the main layer: a `Link: rel="next"` a browser cannot
        // read is a paginated endpoint that reads as unpaginated, and a
        // request id a browser cannot read cannot be quoted in a bug report.
        .expose_headers([
            header::LINK,
            HeaderName::from_static(crate::server::api::middlewares::request_id::REQUEST_ID_HEADER),
        ])
}

/// Shared predicate body for both `build_cors_layer` (browser CORS preflight)
/// and [`is_allowed_origin`] (server-side gate). Both auto-allow the request's
/// own host, or a request whose two ends are both on this machine.
fn cors_allow(origin: &HeaderValue, headers: &HeaderMap) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    is_self_origin(origin, headers) || is_local_dev_pair(origin, headers)
}

/// The host the request arrived at: `X-Forwarded-Host` when a TLS-terminating
/// reverse proxy set one, else `Host`. Empty when neither is present, which is
/// how a non-browser client shows up.
fn request_host(headers: &HeaderMap) -> &str {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Return `true` when `origin` (e.g. `https://app.oxygen-hq.com`) targets the
/// same host as the incoming request. This covers both direct-bind and
/// behind-a-load-balancer deployments without an env var.
fn is_self_origin(origin: &str, headers: &HeaderMap) -> bool {
    let origin_host = match origin.split_once("://") {
        Some((_, rest)) => rest.trim_end_matches('/'),
        None => return false,
    };
    if origin_host.is_empty() {
        return false;
    }
    let host = request_host(headers);
    !host.is_empty() && origin_host == host
}

/// Both ends of the request are on this machine — the local-development case.
///
/// A dev setup routinely mismatches the browser's origin against the host the
/// backend sees, so [`is_self_origin`] cannot pass it: Vite proxies `/api` with
/// `changeOrigin: true`, so a page loaded from `http://127.0.0.1:5173` reaches
/// the backend as `Host: localhost:3000`.
///
/// This replaced a fixed allowlist of literal `http://localhost:<port>` strings,
/// which failed that pair twice over. `127.0.0.1` and `localhost` are the same
/// machine, but only one of the two spellings was listed — so a developer who
/// typed the IP got 403 "origin not allowed" on every custom-app query, with
/// nothing in the URL to suggest why. And the ports were hard-coded while
/// `OXY_DEV_PORT` / `OXY_DEV_PROXY_TARGET` exist precisely so several dev
/// servers can coexist, so moving off `:5173` failed the same way.
///
/// The rule is a **pair**, not "loopback origins are fine": the origin must be
/// loopback AND the request must have arrived at a loopback host. That keeps it
/// strictly narrower than a bare origin check where it matters — a page on
/// `http://localhost:1234` posting to `app.oxygen-hq.com` carries the viewer's
/// cookie and stays rejected — while being wider everywhere it was costing
/// time: any port, either IPv4 or IPv6 loopback, and the `*.localhost` names
/// org-subdomain testing uses.
///
/// ## What the pair leans on
///
/// The safety half is an **ops** property, not a code property: it holds while
/// `request_host()` is never loopback in production. An edge that sets
/// `X-Forwarded-Host` satisfies that. One that does not may not — nginx
/// defaults to `proxy_set_header Host $proxy_host`, so `proxy_pass
/// http://127.0.0.1:3000` with no explicit `Host` / `X-Forwarded-Host` presents
/// `Host: localhost:3000` to this code, and every loopback origin is then
/// accepted with `allow_credentials(true)`. Still far narrower than the fixed
/// allowlist this replaced — which accepted eight literal `localhost` origins
/// with no reference to the host at all — and the attack needs a server already
/// running on the victim's own machine. But it is conditional, and anyone
/// widening this further should know which condition they are leaning on.
fn is_local_dev_pair(origin: &str, headers: &HeaderMap) -> bool {
    let Some((_, origin_host)) = origin.split_once("://") else {
        return false;
    };
    is_loopback_host(origin_host.trim_end_matches('/')) && is_loopback_host(request_host(headers))
}

/// `true` for a `host[:port]` naming this machine: `localhost`, an RFC 6761
/// `*.localhost` subdomain, or any loopback IP literal (`127.0.0.0/8`, `::1`).
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    // Strip the port. An IPv6 literal is bracketed and its address contains
    // colons, so the closing bracket — not the first colon — is the boundary.
    let hostname = match host.rfind(']') {
        Some(close) => host[..=close].trim_start_matches('[').trim_end_matches(']'),
        // An unbracketed IPv6 literal is all colons, so the port split yields
        // nothing. Rare — a spec-compliant proxy brackets it — but silently
        // reading `::1` as "not loopback" is the wrong way to be wrong.
        None => match host.split(':').next() {
            Some("") | None => host,
            Some(before_port) => before_port,
        },
    };
    if hostname.eq_ignore_ascii_case("localhost")
        || hostname
            .rsplit_once('.')
            .is_some_and(|(_, tld)| tld.eq_ignore_ascii_case("localhost"))
    {
        return true;
    }
    hostname
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Check whether the `Origin` (or `Referer`) header on an incoming request
/// targets a permitted origin. Mirrors the CORS layer's predicate as a
/// belt-and-braces server-side check used by sensitive endpoints (the
/// custom-app data gate, etc.).
///
/// Rules:
///   - No `Origin` and no `Referer` → allowed (programmatic clients
///     legitimately omit both).
///   - Origin matches the request's `Host` / `X-Forwarded-Host` → allowed
///     (the standard same-domain deployment).
///   - Origin AND the request's host are both loopback → allowed (local dev
///     across a proxy that rewrites the host; see [`is_local_dev_pair`]).
///   - Otherwise → rejected.
///
/// If only `Referer` is present, its scheme+host is extracted and used as
/// the candidate origin.
pub(crate) fn is_allowed_origin(headers: &HeaderMap) -> bool {
    let candidate = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .and_then(|r| {
                    // Extract scheme://host[:port] from the Referer URL.
                    let without_scheme = r.find("://").map(|i| &r[i + 3..])?;
                    let host_end = without_scheme.find('/').unwrap_or(without_scheme.len());
                    let host = &without_scheme[..host_end];
                    let scheme = &r[..r.find("://").unwrap()];
                    Some(format!("{scheme}://{host}"))
                })
        });
    match candidate {
        // No Origin or Referer → allow non-browser clients.
        None => true,
        Some(origin) => is_self_origin(&origin, headers) || is_local_dev_pair(&origin, headers),
    }
}

#[cfg(test)]
mod app_state_tests {
    use super::*;
    use oxy_app_core::serve_mode::ServeMode;

    #[test]
    fn app_state_carries_mode() {
        let local = AppState {
            enterprise: false,
            internal: false,
            mode: ServeMode::Local,
            observability: None,
            startup_cwd: std::path::PathBuf::from("/tmp"),
            preagg_cache: None,
            preagg_renewal_threshold_secs: None,
            agentic_state: None,
            semantic_layer_cache: super::workspace_cache::new_semantic_layer_cache(),
            semantic_engine_cache: super::workspace_cache::new_semantic_engine_cache(),
        };
        let cloud = AppState {
            enterprise: false,
            internal: false,
            mode: ServeMode::Cloud,
            observability: None,
            startup_cwd: std::path::PathBuf::new(),
            preagg_cache: None,
            preagg_renewal_threshold_secs: None,
            agentic_state: None,
            semantic_layer_cache: super::workspace_cache::new_semantic_layer_cache(),
            semantic_engine_cache: super::workspace_cache::new_semantic_engine_cache(),
        };
        assert!(local.mode.is_local());
        assert!(!cloud.mode.is_local());
    }
}

#[cfg(test)]
mod router_split_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use oxy_app_core::serve_mode::ServeMode;
    use tower::ServiceExt;

    /// `api_router()` wires database-backed middleware during construction.
    /// Skip the inline router tests when `OXY_DATABASE_URL` is unset so CI
    /// without a Postgres doesn't flag a config gap as a code regression.
    /// (The DB-gated integration tests in `crates/app/tests/` follow the
    /// same convention.)
    fn db_unavailable() -> bool {
        std::env::var("OXY_DATABASE_URL").is_err()
    }

    #[tokio::test]
    async fn local_router_does_not_expose_organizations() {
        if db_unavailable() {
            return;
        }
        let (router, _external_router, _preagg) = api_router(
            ServeMode::Local,
            false,
            None,
            std::path::PathBuf::new(),
            tokio_util::sync::CancellationToken::new(),
            false,
            axum::Router::new(),
            Vec::new(),
            axum::Router::new(),
            Vec::new(),
        )
        .await
        .expect("router built");
        let req = Request::builder().uri("/orgs").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "local mode must not mount /orgs"
        );
    }

    #[tokio::test]
    async fn local_router_serves_health() {
        if db_unavailable() {
            return;
        }
        let (router, _external_router, _preagg) = api_router(
            ServeMode::Local,
            false,
            None,
            std::path::PathBuf::new(),
            tokio_util::sync::CancellationToken::new(),
            false,
            axum::Router::new(),
            Vec::new(),
            axum::Router::new(),
            Vec::new(),
        )
        .await
        .expect("router built");
        // /live always returns 200 regardless of DB availability — confirms
        // that public routes are mounted on the local router.
        let req = Request::builder().uri("/live").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.expect("oneshot");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn local_router_mounts_workspace_routes_under_nil_uuid() {
        if db_unavailable() {
            return;
        }
        use oxy_app_core::serve_mode::LOCAL_WORKSPACE_ID;
        let (router, _external_router, _preagg) = api_router(
            ServeMode::Local,
            false,
            None,
            std::path::PathBuf::new(),
            tokio_util::sync::CancellationToken::new(),
            false,
            axum::Router::new(),
            Vec::new(),
            axum::Router::new(),
            Vec::new(),
        )
        .await
        .expect("router built");
        let uri = format!("/{}/agents", LOCAL_WORKSPACE_ID);
        let req = Request::builder().uri(&uri).body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.expect("oneshot");
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "local mode must mount workspace routes under /{{workspace_id}}, got {} for {}",
            resp.status(),
            uri
        );
    }

    #[tokio::test]
    async fn cloud_router_still_has_organizations_mounted() {
        if db_unavailable() {
            return;
        }
        let (router, _external_router, _preagg) = api_router(
            ServeMode::Cloud,
            false,
            None,
            std::path::PathBuf::new(),
            tokio_util::sync::CancellationToken::new(),
            false,
            axum::Router::new(),
            Vec::new(),
            axum::Router::new(),
            Vec::new(),
        )
        .await
        .expect("router built");
        let req = Request::builder().uri("/orgs").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.expect("oneshot");
        // Route is mounted → request reaches auth/handler, not the router's 404.
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "cloud mode must keep /orgs mounted, got {}",
            resp.status()
        );
    }

    /// Customers do not create orgs — staff and partners onboard them — so
    /// `POST /orgs` is a 405: the path stays mounted for the list, and nothing
    /// handles the write. Built from the global tree alone, with no database:
    /// `api_router`'s auth stack would answer 401 first and hide which it was.
    #[tokio::test]
    async fn self_serve_org_creation_is_not_mounted() {
        let router = global::build_global_routes(&bare_app_state())
            .into_router()
            .with_state(bare_app_state());
        let req = Request::builder()
            .method("POST")
            .uri("/orgs")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "POST /orgs must not reach a handler"
        );
    }

    #[tokio::test]
    async fn external_router_fallback_stays_behind_auth() {
        if db_unavailable() {
            return;
        }
        // The external JSON-404 fallback (protected.rs `external_api_not_found`)
        // must sit BEHIND the api-key auth layer: an unauthenticated request to
        // an *unmatched* external path must be 401, NOT the fallback's 404. If
        // `.fallback(...)` were moved after the `.layer(...)` chain it would stop
        // being wrapped by auth and unmatched paths would leak a 404 unauthed.
        // (The matching valid-key → 404-JSON assertion — proving nested misses
        // reach this fallback and not `with_context`'s own — needs a seeded API
        // key; left as a follow-up.)
        let (_router, external_router, _preagg) = api_router(
            ServeMode::Cloud,
            false,
            None,
            std::path::PathBuf::new(),
            tokio_util::sync::CancellationToken::new(),
            false,
            axum::Router::new(),
            Vec::new(),
            axum::Router::new(),
            Vec::new(),
        )
        .await
        .expect("router built");
        let uri = "/00000000-0000-0000-0000-000000000000/no-such-external-route";
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = external_router.oneshot(req).await.expect("oneshot");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "unauthenticated external miss must be 401 (auth wraps the fallback), got {}",
            resp.status(),
        );
    }
}

#[cfg(test)]
mod cors_tests {
    use super::*;

    fn make_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn no_origin_no_referer_allows_non_browser_clients() {
        // Programmatic clients (CLIs, server-to-server) legitimately omit both.
        assert!(is_allowed_origin(&HeaderMap::new()));
    }

    #[test]
    fn dev_origin_localhost_5173_is_allowed() {
        // Vite's web-app calling the backend on :3000, cross-origin.
        let headers = make_headers(&[
            ("origin", "http://localhost:5173"),
            ("host", "localhost:3000"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn dev_origin_localhost_5174_is_allowed() {
        let headers = make_headers(&[
            ("origin", "http://localhost:5174"),
            ("host", "localhost:3000"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn dev_origin_localhost_3005_is_allowed() {
        // Customer-app dev servers run on the :3000–:3005 range (e.g. the
        // Command Center on :3005) and must reach the main `/api` cross-origin.
        let headers = make_headers(&[
            ("origin", "http://localhost:3005"),
            ("host", "localhost:3000"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    /// THE regression this rule replaced a port allowlist to fix.
    ///
    /// Typing the loopback IP instead of the name is not a different setup, but
    /// only `localhost` was listed — and Vite proxies `/api` with
    /// `changeOrigin: true`, so `Host` arrives as the proxy *target's*
    /// (`localhost:3000`) and the same-origin check can't rescue it either.
    /// Every custom-app query answered 403 "origin not allowed", with nothing
    /// in the URL to suggest the spelling was the problem.
    #[test]
    fn loopback_ip_origin_behind_the_vite_proxy_is_allowed() {
        let headers = make_headers(&[
            ("origin", "http://127.0.0.1:5173"),
            ("host", "localhost:3000"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn unbracketed_ipv6_loopback_is_still_loopback() {
        // Not what a spec-compliant proxy emits, but the bracketed form is
        // handled deliberately and dropping the other silently is a worse
        // failure than handling both.
        let headers = make_headers(&[("origin", "http://[::1]:5173"), ("host", "::1")]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn ipv6_loopback_origin_is_allowed() {
        // Bracketed, and the address is all colons — the port split has to find
        // the closing bracket rather than the first `:`.
        let headers = make_headers(&[("origin", "http://[::1]:5173"), ("host", "[::1]:3000")]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn localhost_subdomain_origin_is_allowed() {
        // Org subdomains are exercised locally as `<slug>.localhost`, which
        // RFC 6761 reserves to loopback.
        let headers = make_headers(&[
            ("origin", "http://acme.localhost:5173"),
            ("host", "localhost:3000"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn any_local_port_is_allowed() {
        // `OXY_DEV_PORT` / `OXY_DEV_PROXY_TARGET` exist so several dev servers
        // can coexist; a hard-coded port list made using them a 403.
        let headers = make_headers(&[
            ("origin", "http://localhost:4321"),
            ("host", "127.0.0.1:9876"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    /// The pair is what makes the loopback allowance safe. A page served from
    /// the viewer's own machine posting at the production host would ride their
    /// session cookie, so the *server* end has to be loopback too.
    #[test]
    fn loopback_origin_against_a_public_host_is_rejected() {
        let headers = make_headers(&[
            ("origin", "http://localhost:1234"),
            ("host", "app.oxygen-hq.com"),
        ]);
        assert!(!is_allowed_origin(&headers));
    }

    /// …and the converse: a public origin does not become allowed just because
    /// the request happens to have arrived on loopback (behind a proxy that
    /// dropped the forwarded host).
    #[test]
    fn public_origin_against_a_loopback_host_is_rejected() {
        let headers = make_headers(&[
            ("origin", "https://evil.example.com"),
            ("host", "localhost:3000"),
        ]);
        assert!(!is_allowed_origin(&headers));
    }

    /// `notlocalhost` and `localhost.evil.com` both contain the magic word.
    /// Only a real label boundary counts.
    #[test]
    fn lookalike_hosts_are_not_loopback() {
        for origin in [
            "http://notlocalhost:5173",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
        ] {
            let headers = make_headers(&[("origin", origin), ("host", "localhost:3000")]);
            assert!(
                !is_allowed_origin(&headers),
                "{origin} must not be loopback"
            );
        }
    }

    #[test]
    fn self_origin_via_host_is_allowed() {
        // Standard same-domain deployment.
        let headers = make_headers(&[
            ("origin", "https://app-dev.oxygen-hq.com"),
            ("host", "app-dev.oxygen-hq.com"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn self_origin_via_x_forwarded_host_is_allowed() {
        // Behind a TLS-terminating reverse proxy, `Host` is the internal
        // address; the public hostname comes in on `X-Forwarded-Host`.
        let headers = make_headers(&[
            ("origin", "https://app.oxygen-hq.com"),
            ("host", "oxy-internal.svc.cluster.local"),
            ("x-forwarded-host", "app.oxygen-hq.com"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn referer_scheme_host_used_when_origin_absent() {
        let headers = make_headers(&[
            ("referer", "https://app.oxygen-hq.com/some/path?q=1"),
            ("host", "app.oxygen-hq.com"),
        ]);
        assert!(is_allowed_origin(&headers));
    }

    #[test]
    fn cross_origin_outside_dev_or_self_is_rejected() {
        // An origin that isn't a canonical dev host and doesn't match the
        // server's own host is rejected — defence-in-depth carries over from
        // the old env-driven allowlist.
        let headers = make_headers(&[
            ("origin", "https://attacker.com"),
            ("host", "app.oxygen-hq.com"),
        ]);
        assert!(!is_allowed_origin(&headers));
    }
}
