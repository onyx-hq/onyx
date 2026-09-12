//! Scope enforcement for app-publish-token-authenticated requests.
//!
//! App publish tokens (`oxypublish_...` bearer) are minted for **machine auth**
//! (primarily `oxyc publish` in CI). They are deliberately narrow: they may act
//! as an app-admin for **publishing and read-only inspection of the
//! customer-apps surface only**. The auth middleware attaches an
//! [`AppPublishTokenAuth`] marker whenever a request authenticated via one of
//! these tokens; this middleware reads that marker and rejects anything outside
//! that narrow grant.
//!
//! Why an allow-list rather than per-route guards: the token resolves to a real
//! app-admin user, so it would otherwise pass every guard that user passes —
//! including destructive routes (delete an app, delete users/orgs) and, worse,
//! `POST /customer-apps/{id}/api-keys`, which mints a *separate persisted*
//! credential (the exact escalation-persistence shape these tokens must not
//! enable). This middleware is the single choke-point that keeps the blast
//! radius to "ship a build + look at the surface". Requests WITHOUT the marker
//! (cookie/JWT/API-key sessions) are unaffected.
//!
//! **Grant (method-aware):**
//!   - `GET`/`HEAD` on `/customer-apps/…` — read-only inspection (list, get,
//!     builds, activity).
//!   - `POST /customer-apps/publish` — the CLI tarball upload.
//!   - `POST /customer-apps/{id}/publish` — promote draft → live.
//!   - everything else (DELETE/unpublish, PATCH/update, create, rollback,
//!     `POST /{id}/api-keys`, preview-draft, and every non-customer-apps path)
//!     → `403`.
//!
//! Runs immediately after `auth_middleware` (which sets the marker). NOTE:
//! `api_router` is mounted with `.nest("/api", …)`, and axum strips the nest
//! prefix before this layer runs — so `request.uri().path()` here is
//! nest-relative (`/customer-apps/…`), WITHOUT the `/api` prefix. The matcher
//! below works on that stripped form (see the nested-router regression tests).

use axum::http::{Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use oxy_auth::types::AppPublishTokenAuth;

pub async fn app_publish_token_scope_middleware(
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Only constrain requests that authenticated via an app publish token.
    // Every other credential passes straight through.
    if request.extensions().get::<AppPublishTokenAuth>().is_none() {
        return Ok(next.run(request).await);
    }

    if is_allowed(request.method(), request.uri().path()) {
        Ok(next.run(request).await)
    } else {
        tracing::warn!(
            method = %request.method(),
            path = %request.uri().path(),
            "app-publish-token request out of scope — rejected (tokens may only \
             publish + read the customer-apps surface)"
        );
        Err(StatusCode::FORBIDDEN)
    }
}

/// The narrow grant: reads anywhere on the customer-apps surface, but only the
/// two publish endpoints may mutate.
fn is_allowed(method: &Method, path: &str) -> bool {
    if !under_custom_apps(path) {
        return false;
    }
    match *method {
        // Read-only inspection — no mutation, no credential mint.
        Method::GET | Method::HEAD => true,
        // The only mutating action a publish token needs: shipping a build.
        Method::POST => is_publish_route(path),
        // DELETE (delete/unpublish), PATCH (update), and any other POST
        // (create, rollback, api-key mint, preview-draft) are out of scope.
        _ => false,
    }
}

/// Segment-boundary prefix check so `/customer-apps-evil` can't pass as
/// `/customer-apps`.
fn under_custom_apps(path: &str) -> bool {
    path == "/customer-apps"
        || path
            .strip_prefix("/customer-apps")
            .is_some_and(|rest| rest.starts_with('/'))
}

/// True only for `POST /customer-apps/publish` (CLI upload) and
/// `POST /customer-apps/{id}/publish` (promote). Matched by exact segment
/// shape so siblings like `/customer-apps/{id}/api-keys` never qualify.
fn is_publish_route(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/customer-apps/") else {
        return false;
    };
    let mut segs = rest.split('/');
    match (segs.next(), segs.next(), segs.next()) {
        // /customer-apps/publish
        (Some("publish"), None, _) => true,
        // /customer-apps/{id}/publish
        (Some(id), Some("publish"), None) => !id.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Paths are nest-relative: axum strips `/api` before this middleware, so
    // the matcher and these tests use `/customer-apps`, not `/api/customer-apps`.
    // The nested-router tests below drive a real request through
    // `.nest("/api", …)` to prove that's the path shape.

    #[test]
    fn allows_publish_and_reads() {
        // Reads: any GET on the surface.
        assert!(is_allowed(&Method::GET, "/customer-apps"));
        assert!(is_allowed(&Method::GET, "/customer-apps/3f2504e0"));
        assert!(is_allowed(&Method::GET, "/customer-apps/3f2504e0/builds"));
        assert!(is_allowed(&Method::HEAD, "/customer-apps/3f2504e0"));
        // Publish: the two POST endpoints.
        assert!(is_allowed(&Method::POST, "/customer-apps/publish"));
        assert!(is_allowed(&Method::POST, "/customer-apps/3f2504e0/publish"));
    }

    #[test]
    fn blocks_destructive_and_credential_mint() {
        // The escalation the reviewer flagged: minting a persisted app API key.
        assert!(!is_allowed(
            &Method::POST,
            "/customer-apps/3f2504e0/api-keys"
        ));
        assert!(!is_allowed(&Method::DELETE, "/customer-apps/3f2504e0"));
        assert!(!is_allowed(
            &Method::DELETE,
            "/customer-apps/3f2504e0/publish"
        ));
        // Mutations that aren't publish.
        assert!(!is_allowed(&Method::PATCH, "/customer-apps/3f2504e0"));
        assert!(!is_allowed(&Method::POST, "/customer-apps")); // create
        assert!(!is_allowed(
            &Method::POST,
            "/customer-apps/3f2504e0/rollback"
        ));
        assert!(!is_allowed(&Method::POST, "/customer-apps/preview-draft"));
    }

    #[test]
    fn blocks_everything_off_the_custom_apps_surface() {
        // Token self-management must never be reachable by a token.
        assert!(!is_allowed(&Method::GET, "/admin/app-publish-tokens"));
        assert!(!is_allowed(&Method::POST, "/admin/app-publish-tokens"));
        // Destructive owner/admin surfaces.
        assert!(!is_allowed(&Method::DELETE, "/admin/orgs/3f2504e0"));
        assert!(!is_allowed(&Method::GET, "/orgs"));
        // Prefix-boundary attack: a sibling that shares the string prefix but
        // not the segment boundary.
        assert!(!is_allowed(&Method::POST, "/customer-apps-evil/publish"));
    }

    // ── Nested-router regression tests ─────────────────────────────────────
    // Drive requests through the real `.nest("/api", …)` so the path the
    // middleware sees is the actual stripped, nest-relative one — the check a
    // bare `is_allowed` unit test can't provide.
    use axum::body::Body;
    use axum::routing::{get, post};
    use axum::{Router, middleware};
    use tower::ServiceExt;

    async fn ok() -> StatusCode {
        StatusCode::OK
    }

    /// Stamps the `AppPublishTokenAuth` marker, standing in for `auth_middleware`
    /// recognizing an `oxypublish_…` bearer.
    async fn inject_marker(mut req: Request<Body>, next: Next) -> Response {
        req.extensions_mut().insert(AppPublishTokenAuth {
            token_id: uuid::Uuid::nil(),
            app_id: None,
        });
        next.run(req).await
    }

    /// Router shaped like production: routes registered WITHOUT `/api`, the
    /// scope middleware layered inside, the whole thing nested under `/api`.
    fn nested_app(with_marker: bool) -> Router {
        let mut inner = Router::new()
            .route("/customer-apps", get(ok))
            .route("/customer-apps/publish", post(ok))
            .route("/customer-apps/{id}", get(ok).delete(ok))
            .route("/customer-apps/{id}/publish", post(ok))
            .route("/customer-apps/{id}/api-keys", post(ok))
            .route("/admin/app-publish-tokens", get(ok))
            .layer(middleware::from_fn(app_publish_token_scope_middleware));
        if with_marker {
            // Layered last → outermost → runs before the scope middleware.
            inner = inner.layer(middleware::from_fn(inject_marker));
        }
        Router::new().nest("/api", inner)
    }

    async fn status_of(app: Router, method: &str, uri: &str) -> StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn publish_upload_and_promote_are_reachable() {
        assert_eq!(
            status_of(nested_app(true), "POST", "/api/customer-apps/publish").await,
            StatusCode::OK,
            "CLI upload must be reachable by a publish token"
        );
        assert_eq!(
            status_of(nested_app(true), "POST", "/api/customer-apps/abc/publish").await,
            StatusCode::OK,
            "promote must be reachable by a publish token"
        );
    }

    #[tokio::test]
    async fn reads_are_reachable() {
        assert_eq!(
            status_of(nested_app(true), "GET", "/api/customer-apps").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn api_key_mint_and_delete_are_blocked() {
        // The escalation-persistence route: minting a persisted app API key.
        assert_eq!(
            status_of(nested_app(true), "POST", "/api/customer-apps/abc/api-keys").await,
            StatusCode::FORBIDDEN,
            "publish tokens must not mint app API keys"
        );
        // Destroying an app registration.
        assert_eq!(
            status_of(nested_app(true), "DELETE", "/api/customer-apps/abc").await,
            StatusCode::FORBIDDEN,
            "publish tokens must not delete apps"
        );
    }

    #[tokio::test]
    async fn token_self_management_is_blocked() {
        assert_eq!(
            status_of(nested_app(true), "GET", "/api/admin/app-publish-tokens").await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn non_publish_token_requests_pass_through() {
        // No marker → not scope-limited → reaches even /admin/*.
        assert_eq!(
            status_of(nested_app(false), "GET", "/api/admin/app-publish-tokens").await,
            StatusCode::OK
        );
    }
}
