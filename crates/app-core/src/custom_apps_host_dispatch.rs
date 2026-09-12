//! Host-based dispatch for v0 / Vercel custom-app bundle subdomains.
//!
//! Lets a v0-source app be reachable at
//! `https://<org>--<slug>.customer-apps[-<env>].<zone>/...` in addition
//! to the existing path-prefix URL
//! `https://<admin-host>/customer-apps/<org>/<slug>/...`.
//!
//! ## Why
//!
//! On path-prefix mode every Vercel app must set `basePath` /
//! `assetPrefix` in `next.config.js` so root-relative asset URLs
//! (`/_next/...`) and runtime `fetch("/api/...")` resolve under the
//! prefix. On a dedicated subdomain root is root for the upstream —
//! v0.dev-generated apps work out of the box with zero per-app config,
//! and runtime JS calls that construct URLs at execution time (which a
//! path-rewriting reverse proxy can't reach) also resolve correctly.
//!
//! ## Design choices
//!
//! - **Pure structural match for dispatch.** The middleware does not
//!   read any env var. It treats any Host of shape
//!   `<org>--<slug>.customer-apps[-<env>]?.<zone>` as a custom-app
//!   subdomain. This means the same image works in dev, staging, prod,
//!   and any custom domain the cluster operator points at it — no
//!   per-env config drift. The `customer-apps` second label is the
//!   discriminator (an admin host like `app-dev.oxygen-hq.com` has no
//!   `--` in the first label and no `customer-apps` second label, so
//!   it never matches).
//!
//! - **URL generation auto-derives from `OXY_API_URL`.** The admin UI
//!   needs an absolute subdomain URL to render alongside the subpath
//!   URL. We derive it by looking at the cluster's admin host
//!   (extracted from `OXY_API_URL`, which every cloud deployment
//!   already sets) and applying the `app{-env}` ↔ `customer-apps{-env}`
//!   convention. No new env var. Returns `None` when the admin host
//!   doesn't fit the convention (e.g. local `http://localhost:3000` or
//!   a custom-branded host), in which case the admin UI just hides the
//!   subdomain URL row.
//!
//! ## How
//!
//! A tower middleware sits in front of the router. On every request:
//!
//!   1. Check the `Host` header. If structural parse yields
//!      `(org, slug)`, the URI path is rewritten from `/<rest>` to
//!      `/customer-apps/<org>/<slug>/<rest>` so the existing
//!      `/customer-apps/{*path}` route handler (`serve_dispatch`)
//!      takes over with no other changes — including the proxy to the
//!      upstream Vercel URL for `AppSource::V0`.
//!   2. Otherwise the request passes through unchanged.
//!
//! Critical exception: requests with path `/api/...` are left
//! untouched even on a subdomain host. Bundle SDK calls like
//! `fetch("/api/projects/.../query")` from a bundle hosted at
//! `mars--app.customer-apps-dev.oxygen-hq.com` must land on the data
//! API (this same oxy backend), NOT be proxied to the upstream Vercel.
//! Keeping the URI unchanged lets the normal `/api/*` router handle
//! them with the existing cookie- or API-key-based auth.

use axum::extract::Request;
use axum::http::Uri;
use axum::middleware::Next;
use axum::response::Response;

/// Parse `<org>--<slug>.customer-apps[-<env>]?.<rest>` from a `Host`
/// header value. Returns `(org_slug, app_slug)` on a match, `None`
/// otherwise.
///
/// Rules:
/// - Strips any `:port` tail (so dev-server hosts work).
/// - The first label must split into `<org>--<slug>` on the FIRST
///   `--`. Slugs may contain `-` but not `--` (we control slug
///   validation; split_once at first `--` means engineering org names
///   like `acme--internal` would collide, so we disallow `--` in org
///   slugs in the registration form — see admin/apps/handlers.rs).
/// - The second label must be exactly `customer-apps` or start with
///   `customer-apps-` (i.e. `customer-apps`, `customer-apps-dev`,
///   `customer-apps-staging`, `customer-apps-prod`, …).
/// - Both parts non-empty; neither contains `.` (defense against a
///   crafted host like `evil.mars--app.customer-apps-dev.oxygen-hq.com`
///   trying to ride a wildcard).
pub fn parse_subdomain(host: &str) -> Option<(String, String)> {
    let host_no_port = host.split(':').next().unwrap_or(host);
    let (prefix, rest) = host_no_port.split_once('.')?;
    let second_label = rest.split('.').next()?;
    if second_label != "customer-apps" && !second_label.starts_with("customer-apps-") {
        return None;
    }
    let (org, slug) = prefix.split_once("--")?;
    if org.is_empty() || slug.is_empty() {
        return None;
    }
    if org.contains('.') || slug.contains('.') {
        return None;
    }
    Some((org.to_string(), slug.to_string()))
}

/// Build the absolute subdomain URL for an app, e.g.
/// `https://mars--command-center.customer-apps-dev.oxygen-hq.com/`.
///
/// Auto-derives the customer-apps zone from `OXY_API_URL` by stripping
/// the leading `app` (`app-dev`/`app-staging`/`app` → `customer-apps-dev`
/// /`customer-apps-staging`/`customer-apps`). Returns `None` when:
/// - `OXY_API_URL` is unset / malformed (no env var fallback exists),
/// - the admin host has no `.` (e.g. `localhost`),
/// - the admin host's first label doesn't start with `app` (custom
///   branded host — operator should configure DNS + UI separately if
///   they want subdomain URLs in this case).
///
/// In any of those cases the admin UI hides the "Subdomain URL" row;
/// the path-prefix URL still works for both v0 and S3 sources.
pub fn subdomain_url_for(org_slug: &str, app_slug: &str) -> Option<String> {
    let zone = custom_apps_zone()?;
    Some(format!("https://{org_slug}--{app_slug}.{zone}/"))
}

/// Compute the customer-apps zone for this deployment by mapping the
/// admin host's `app{-env}` first label to `customer-apps{-env}`.
///
/// `OXY_API_URL = https://app-dev.oxygen-hq.com/api`
///   → admin host `app-dev.oxygen-hq.com`
///   → first label `app-dev`, suffix `-dev`
///   → zone `customer-apps-dev.oxygen-hq.com`
///
/// `OXY_API_URL = https://app.oxygen-hq.com/api`
///   → first label `app`, suffix ``
///   → zone `customer-apps.oxygen-hq.com`
///
/// Cluster operators on a custom-branded host (no `app` prefix) get
/// `None`; subdomain URLs are simply not surfaced.
fn custom_apps_zone() -> Option<String> {
    let api_url = std::env::var("OXY_API_URL").ok()?;
    let parsed: url::Url = api_url.parse().ok()?;
    let admin_host = parsed.host_str()?;
    let (first_label, rest) = admin_host.split_once('.')?;
    let env_suffix = first_label.strip_prefix("app")?;
    Some(format!("customer-apps{env_suffix}.{rest}"))
}

/// Absolute base URL of the admin host this cluster serves, e.g.
/// `https://app-staging.oxygen-hq.com`. Derived from `OXY_API_URL`
/// (same source as [`custom_apps_zone`]). Returns `None` when
/// `OXY_API_URL` is unset / malformed.
///
/// Used by the customer-apps serve path to bounce unauthenticated
/// visitors hitting a subdomain URL to the admin host's `/login`
/// page. The subdomain doesn't serve the login SPA, so echoing the
/// request Host back into the redirect produces an infinite loop:
/// 302 → dispatcher rewrites `/login` to
/// `/customer-apps/<org>/<slug>/login` → another 302 with a longer
/// `?return_to=` appended → … until nginx returns 414 Request-URI
/// Too Large.
pub fn admin_base_url() -> Option<String> {
    let api_url = std::env::var("OXY_API_URL").ok()?;
    let parsed: url::Url = api_url.parse().ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Tower middleware that rewrites subdomain requests to the path-prefix
/// form so the existing `/customer-apps/{*path}` route handles them.
///
/// Applied to the outer router so every request is inspected. Cheap:
/// one Host-header read + at most two `split_once` calls on the
/// non-matching fast path.
pub async fn subdomain_rewrite_middleware(request: Request, next: Next) -> Response {
    let Some(host) = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return next.run(request).await;
    };
    let Some((org, slug)) = parse_subdomain(host) else {
        return next.run(request).await;
    };

    // Bundle SDK calls land on the data API, NOT on the upstream
    // Vercel. Keep `/api/*` requests untouched even on a subdomain
    // host so the normal `/api/*` router handles them.
    let original_path = request.uri().path();
    if original_path == "/api" || original_path.starts_with("/api/") {
        return next.run(request).await;
    }

    // If the request already carries the path-prefix form
    // `/customer-apps/<org>/<slug>/...` for THIS subdomain's
    // (org, slug) pair, pass it through unchanged — see
    // [`already_canonicalized`] for the rationale.
    if already_canonicalized(original_path, &org, &slug) {
        return next.run(request).await;
    }

    let new_path = format!("/customer-apps/{org}/{slug}{original_path}");
    let Some(new_uri) = rewrite_uri_path(request.uri(), &new_path) else {
        return next.run(request).await;
    };

    let (mut parts, body) = request.into_parts();
    parts.uri = new_uri;
    let new_request = Request::from_parts(parts, body);
    next.run(new_request).await
}

/// True when `path` already matches the path-prefix form
/// `/customer-apps/<org>/<slug>(/...)` for THIS subdomain's
/// (org, slug) pair — in which case the middleware must NOT rewrite,
/// because doing so would produce
/// `/customer-apps/<org>/<slug>/customer-apps/<org>/<slug>/...` and
/// 404 every asset.
///
/// This is the common case for S3-source apps (`oxyc publish`): the
/// bundle was built with `OXY_APP_BASE_PATH=/customer-apps/<org>/<slug>/`
/// baked into every asset URL, so the very first HTML response on the
/// subdomain contains `<script src="/customer-apps/<org>/<slug>/_next/...">`.
/// The browser then fetches those URLs on the subdomain host; without
/// this guard the middleware would double-prefix them, every asset
/// would 404, and the page would render blank.
///
/// (v0-source apps don't hit this because their upstream HTML is
/// root-rooted — `/_next/...` not `/customer-apps/.../_next/...` —
/// and the middleware correctly prefixes them once. S3 apps were
/// built before the subdomain existed and bake the subpath into the
/// bytes.)
fn already_canonicalized(path: &str, org: &str, slug: &str) -> bool {
    let target = format!("/customer-apps/{org}/{slug}");
    path == target || path.starts_with(&format!("{target}/"))
}

/// Replace only the path of `original`, preserving scheme, authority,
/// and query. Returns `None` if the resulting URI is invalid (which
/// shouldn't happen given the inputs but we fail open rather than
/// panic).
fn rewrite_uri_path(original: &Uri, new_path: &str) -> Option<Uri> {
    let path_and_query = match original.query() {
        Some(q) => format!("{new_path}?{q}"),
        None => new_path.to_string(),
    };
    let mut builder = Uri::builder().path_and_query(path_and_query);
    if let Some(scheme) = original.scheme() {
        builder = builder.scheme(scheme.clone());
    }
    if let Some(authority) = original.authority() {
        builder = builder.authority(authority.clone());
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes env-mutation in the URL-helper tests so they don't race
    // with each other. Per Rust nightly recommendation, swap to
    // `std::sync::Mutex` lazily so we don't pull in once_cell.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn parse_dev_host() {
        let got = parse_subdomain("mars--command-center.customer-apps-dev.oxygen-hq.com");
        assert_eq!(
            got,
            Some(("mars".to_string(), "command-center".to_string()))
        );
    }

    #[test]
    fn parse_staging_host() {
        let got = parse_subdomain("acme--store.customer-apps-staging.oxygen-hq.com");
        assert_eq!(got, Some(("acme".to_string(), "store".to_string())));
    }

    #[test]
    fn parse_prod_host_no_env_label() {
        let got = parse_subdomain("acme--store.customer-apps.oxygen-hq.com");
        assert_eq!(got, Some(("acme".to_string(), "store".to_string())));
    }

    #[test]
    fn parse_strips_port() {
        let got = parse_subdomain("mars--command-center.customer-apps-dev.oxygen-hq.com:443");
        assert_eq!(
            got,
            Some(("mars".to_string(), "command-center".to_string()))
        );
    }

    #[test]
    fn parse_slug_with_internal_hyphens() {
        // split_once("--") splits at FIRST occurrence — slug retains
        // any further hyphens. Round-trips correctly.
        let got = parse_subdomain("acme--my-data-app.customer-apps-dev.oxygen-hq.com");
        assert_eq!(got, Some(("acme".to_string(), "my-data-app".to_string())));
    }

    #[test]
    fn parse_works_on_alternative_zones() {
        // The zone is not hard-coded — any `<o>--<s>.customer-apps*.X`
        // matches. Lets the same binary work on custom-branded hosts
        // that still follow the `customer-apps[-env]` second-label
        // convention.
        let got = parse_subdomain("acme--store.customer-apps.example.io");
        assert_eq!(got, Some(("acme".to_string(), "store".to_string())));
    }

    #[test]
    fn parse_rejects_admin_host() {
        // No `--` and second label isn't `customer-apps*`.
        assert_eq!(parse_subdomain("app-dev.oxygen-hq.com"), None);
        assert_eq!(parse_subdomain("app.oxygen-hq.com"), None);
    }

    #[test]
    fn parse_rejects_wrong_second_label() {
        // Looks similar but isn't `customer-apps*` — reject.
        assert_eq!(parse_subdomain("mars--app.customer.oxygen-hq.com"), None);
        assert_eq!(
            parse_subdomain("mars--app.custom-app.oxygen-hq.com"), // singular — typo, reject
            None
        );
    }

    #[test]
    fn parse_rejects_missing_delimiter() {
        let got = parse_subdomain("marscommand-center.customer-apps-dev.oxygen-hq.com");
        assert_eq!(got, None);
    }

    #[test]
    fn parse_rejects_empty_org() {
        let got = parse_subdomain("--command-center.customer-apps-dev.oxygen-hq.com");
        assert_eq!(got, None);
    }

    #[test]
    fn parse_rejects_empty_slug() {
        let got = parse_subdomain("mars--.customer-apps-dev.oxygen-hq.com");
        assert_eq!(got, None);
    }

    #[test]
    fn parse_rejects_multi_label_prefix() {
        // The prefix is just the FIRST label. `evil.mars--app...`
        // splits into `evil` as the first label and `mars--app...` as
        // rest — `evil` has no `--`, so it fails. A crafted prefix
        // can't ride the wildcard cert to impersonate a real app.
        let got = parse_subdomain("evil.mars--command-center.customer-apps-dev.oxygen-hq.com");
        assert_eq!(got, None);
    }

    #[test]
    fn already_canonicalized_matches_exact_prefix() {
        // S3-source apps' baked-in asset URLs land here on the
        // subdomain. Without skipping the rewrite, the doubled-prefix
        // bug bites and the page is blank.
        assert!(already_canonicalized(
            "/customer-apps/poke-house/command-center/_next/static/x.js",
            "poke-house",
            "command-center"
        ));
        assert!(already_canonicalized(
            "/customer-apps/poke-house/command-center/",
            "poke-house",
            "command-center"
        ));
        assert!(already_canonicalized(
            "/customer-apps/poke-house/command-center",
            "poke-house",
            "command-center"
        ));
    }

    #[test]
    fn already_canonicalized_rejects_other_apps_path() {
        // A subdomain for `mars/store` mustn't serve a path that
        // claims to be for `evil/leak` — the rewrite-and-dispatch
        // gives the operator's intended (org, slug) the only
        // authority over what's served.
        assert!(!already_canonicalized(
            "/customer-apps/evil/leak/secret",
            "mars",
            "store"
        ));
    }

    #[test]
    fn already_canonicalized_rejects_root_or_unrelated() {
        // The common rewrite case: subdomain hit for `/`, `/_next/...`,
        // `/assets/foo.css` etc. all need to be rewritten to add the
        // canonical prefix. Don't skip.
        assert!(!already_canonicalized("/", "poke-house", "command-center"));
        assert!(!already_canonicalized(
            "/_next/static/x.js",
            "poke-house",
            "command-center"
        ));
        assert!(!already_canonicalized(
            "/assets/main.css",
            "poke-house",
            "command-center"
        ));
    }

    #[test]
    fn already_canonicalized_rejects_partial_prefix_match() {
        // `/customer-apps/poke-houseFOO/...` shouldn't be treated as
        // belonging to `poke-house`. Only an exact path-segment match
        // counts — the `/` boundary check guards against it.
        assert!(!already_canonicalized(
            "/customer-apps/poke-houseFOO/command-center/foo",
            "poke-house",
            "command-center"
        ));
    }

    #[test]
    fn rewrite_uri_preserves_query() {
        let original: Uri = "https://x/foo?bar=1".parse().unwrap();
        let got = rewrite_uri_path(&original, "/customer-apps/o/s/foo").unwrap();
        assert_eq!(got.path(), "/customer-apps/o/s/foo");
        assert_eq!(got.query(), Some("bar=1"));
    }

    #[test]
    fn rewrite_uri_root_path() {
        let original: Uri = "/".parse().unwrap();
        let got = rewrite_uri_path(&original, "/customer-apps/o/s/").unwrap();
        assert_eq!(got.path(), "/customer-apps/o/s/");
        assert_eq!(got.query(), None);
    }

    // ── URL-helper auto-derivation ───────────────────────────────────

    #[test]
    fn url_helper_derives_dev() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "https://app-dev.oxygen-hq.com/api");
        }
        let got = subdomain_url_for("mars", "command-center");
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        assert_eq!(
            got,
            Some("https://mars--command-center.customer-apps-dev.oxygen-hq.com/".to_string())
        );
    }

    #[test]
    fn url_helper_derives_staging() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "https://app-staging.oxygen-hq.com/api");
        }
        let got = subdomain_url_for("acme", "store");
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        assert_eq!(
            got,
            Some("https://acme--store.customer-apps-staging.oxygen-hq.com/".to_string())
        );
    }

    #[test]
    fn url_helper_derives_prod() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "https://app.oxygen-hq.com/api");
        }
        let got = subdomain_url_for("acme", "store");
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        assert_eq!(
            got,
            Some("https://acme--store.customer-apps.oxygen-hq.com/".to_string())
        );
    }

    #[test]
    fn url_helper_none_for_localhost() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "http://localhost:3000/api");
        }
        let got = subdomain_url_for("mars", "store");
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        assert_eq!(got, None);
    }

    #[test]
    fn url_helper_none_for_custom_branded_host() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "https://data.acme.com/api");
        }
        let got = subdomain_url_for("mars", "store");
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        // First label is `data`, not `app*` — no convention to apply.
        assert_eq!(got, None);
    }

    #[test]
    fn url_helper_none_when_api_url_unset() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        assert_eq!(subdomain_url_for("mars", "store"), None);
    }

    // ── Router wiring: the rewrite MUST run before routing ───────────────
    //
    // Regression test for the staging/prod custom-app "blank page" bug.
    //
    // `Router::layer` runs middleware AFTER routing — it wraps each leaf
    // service (every route AND the fallback) individually, and path
    // matching happens on the ORIGINAL URI first. So a fallback-bound
    // request like `/foo.js` is already routed to the static fallback
    // before this middleware runs; the rewrite to
    // `/customer-apps/<org>/<slug>/foo.js` is then handed straight to the
    // static file server (→ admin SPA `index.html`) and never re-enters the
    // router to reach `serve_dispatch`. Wrapping the WHOLE router instead
    // makes the rewrite run before routing, so it can change which handler
    // is selected.
    #[tokio::test]
    async fn subdomain_rewrite_must_run_before_routing() {
        use axum::http::Request;
        use axum::{Router, body::Body, routing::any};
        use tower::ServiceExt;

        async fn dispatch() -> &'static str {
            "DISPATCH"
        }
        async fn spa() -> &'static str {
            "ADMIN_SPA"
        }

        let host = "poke-house--command-center.customer-apps-staging.oxygen-hq.com";
        let make_req = || {
            Request::builder()
                .uri("/foo.js")
                .header("host", host)
                .body(Body::empty())
                .unwrap()
        };
        async fn body_of(resp: Response) -> Vec<u8> {
            axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap()
                .to_vec()
        }

        // (1) BUGGY wiring — exactly what serve.rs does today: middleware
        //     applied via `Router::layer`. Reproduces the production
        //     symptom: `/foo.js` falls through to the admin SPA because the
        //     rewrite happened after routing.
        let buggy = Router::new()
            .route("/customer-apps/{*path}", any(dispatch))
            .fallback(spa)
            .layer(axum::middleware::from_fn(subdomain_rewrite_middleware));
        let got = body_of(buggy.oneshot(make_req()).await.unwrap()).await;
        assert_eq!(
            got, b"ADMIN_SPA",
            "Router::layer runs after routing, so the rewrite misses serve_dispatch"
        );

        // (2) FIXED wiring — wrap the WHOLE router so the rewrite runs
        //     before routing and `/foo.js` is re-routed to the dispatch
        //     handler.
        use tower::Layer;
        let inner = Router::new()
            .route("/customer-apps/{*path}", any(dispatch))
            .fallback(spa);
        let fixed = axum::middleware::from_fn(subdomain_rewrite_middleware).layer(inner);
        let got = body_of(fixed.oneshot(make_req()).await.unwrap()).await;
        assert_eq!(
            got, b"DISPATCH",
            "wrapping the whole router runs the rewrite before routing"
        );
    }

    // ── admin_base_url ────────────────────────────────────────────────────

    #[test]
    fn admin_base_url_prod_apex() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "https://app.oxygen-hq.com/api");
        }
        assert_eq!(
            admin_base_url(),
            Some("https://app.oxygen-hq.com".to_string())
        );
    }

    #[test]
    fn admin_base_url_env_suffix() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "https://app-staging.oxygen-hq.com/api");
        }
        assert_eq!(
            admin_base_url(),
            Some("https://app-staging.oxygen-hq.com".to_string())
        );
    }

    #[test]
    fn admin_base_url_keeps_explicit_port() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::set_var("OXY_API_URL", "http://localhost:3000/api");
        }
        assert_eq!(admin_base_url(), Some("http://localhost:3000".to_string()));
    }

    #[test]
    fn admin_base_url_none_when_api_url_unset() {
        let _g = env_lock().lock().unwrap();
        unsafe {
            std::env::remove_var("OXY_API_URL");
        }
        assert_eq!(admin_base_url(), None);
    }
}
