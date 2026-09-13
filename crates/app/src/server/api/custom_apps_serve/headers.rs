//! Response headers, caching policy, content-type classification, and the
//! login / legacy-uuid redirects for custom-app serving.

use std::path::Path as StdPath;

use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use entity::prelude::Apps;
use oxy::database::client::establish_connection;
use oxy_auth::authenticator::Authenticator;
use oxy_auth::built_in::BuiltInAuthenticator;
use oxy_auth::user::UserService;
use sea_orm::EntityTrait;
use uuid::Uuid;

/// A 404 on the custom-app serve route, marked explicitly uncacheable.
///
/// 404 is heuristically cacheable (RFC 9111 §15.1) and CDNs apply a default
/// negative TTL — CloudFront's is 10s. Nearly every negative answer on this
/// route is *state that flips*: "no build for this channel" becomes a 200 at
/// the first `oxyc publish`, an unknown org or app slug becomes real when it
/// is created, and a missing hashed asset comes back when a rollback
/// re-ships that chunk. Caching any of those holds the stale answer across
/// the transition, and none of them is worth caching to begin with.
///
/// This route polices its 200s carefully — `private` on HTML so a shared
/// cache can't store the tracking cookie, `immutable` gated on the resolved
/// file so the SPA fallback can't be pinned. Leaving the 404s with no policy
/// at all was the asymmetry.
pub(super) fn no_store_404() -> Response {
    (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

/// True for "user opened a page" requests: root, any trailing-slash
/// directory, or a `.html` file. Excludes asset URLs (`/_next/...`,
/// `/static/...`, `*.js`/`*.css`/`*.svg`/etc.) and API fetches so the
/// Activity tab counts page loads, not request volume.
pub(super) fn is_html_navigation(rest: &str) -> bool {
    let path = rest.trim_start_matches('/');
    if path.is_empty() || path.ends_with('/') {
        return true;
    }
    // Strip a query string before checking the extension.
    let bare = path.split('?').next().unwrap_or(path);
    bare.ends_with(".html")
}

/// True when the browser told us this request is speculative — a prefetch or a
/// prerender it started on its own, not a page the user opened.
///
/// Three spellings, because no two engines agree:
///
/// - **`Sec-Purpose: prefetch`** — the Speculation Rules spec; Chrome and Edge.
/// - **`Purpose: prefetch`** — the long-standing de-facto header, sent by older
///   Chrome and by intermediaries.
/// - **`X-moz: prefetch`** — what **Firefox** actually sends for
///   `<link rel="prefetch">`. It sends neither of the other two, so omitting
///   this made the guard a no-op on Firefox: the launcher's hover-warm recorded
///   an open and minted the tracking session at hover time, which the real
///   navigation then inherited — precisely the failure the guard exists to
///   prevent, on one of the two engines that matters.
///
/// All three are checked because the platform actively issues prefetches — the
/// HQ launcher warms an app on hover (`prefetchApp`) so the click is instant —
/// and a speculative fetch that recorded a view would make "someone opened this
/// app" mean "someone's pointer passed over its card."
///
/// Also suppresses the tracking `Set-Cookie`: a prefetched response that minted
/// a session id would start that session at hover time, and the real
/// navigation seconds later would inherit it. Cheaper and more honest to let
/// the actual page load start the session.
///
/// Not a security boundary — the header is client-supplied and a caller who
/// wants to avoid being counted can always send it. That is fine: it costs them
/// their own view row and nothing else.
pub(super) fn is_speculative_request(headers: &HeaderMap) -> bool {
    let says_prefetch = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                let v = v.to_ascii_lowercase();
                v.contains("prefetch") || v.contains("prerender")
            })
    };
    says_prefetch("sec-purpose") || says_prefetch("purpose") || says_prefetch("x-moz")
}

/// Cheap UA classifier — keeps the recorded value small + bounded so
/// the Activity tab can group rows by class without storing the full
/// UA string. Real browser navigations have UA strings starting with
/// `Mozilla/` (per RFC convention); SDK/curl-style calls don't.
pub(super) fn classify_user_agent(header: Option<&axum::http::HeaderValue>) -> String {
    let Some(v) = header.and_then(|h| h.to_str().ok()) else {
        return "unknown".to_string();
    };
    if v.starts_with("Mozilla/") {
        "browser".to_string()
    } else if v.is_empty() {
        "unknown".to_string()
    } else {
        "sdk".to_string()
    }
}

pub(super) fn redirect_to_login(headers: &HeaderMap, uri: &Uri) -> Redirect {
    // `return_to` is where the user lands after a successful login —
    // it MUST be the URL they originally hit (subdomain or subpath),
    // so the post-login bounce drops them back at the custom-app
    // entry point. `base_url(headers)` echoes the request Host, which
    // is exactly what we want for `return_to`.
    let request_base = base_url(headers);
    let return_to = format!("{request_base}{uri}");

    // The login SPA itself only lives on the admin host. If the
    // request came in on a customer-apps subdomain (no SPA there),
    // bouncing to the same subdomain's `/login` causes the dispatcher
    // to rewrite it to `/customer-apps/<org>/<slug>/login`, which is
    // another `serve_dispatch` invocation that re-runs auth, redirects
    // again, accumulating `?return_to=` each round. nginx caps that at
    // 414 Request-URI Too Large after a handful of iterations and the
    // browser surfaces "too many redirects." Use the admin host
    // explicitly when we can derive it (from `OXY_API_URL`); fall back
    // to `request_base` when we can't (local dev with localhost).
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let login_base = if oxy_app_core::custom_apps_host_dispatch::parse_subdomain(host).is_some() {
        oxy_app_core::custom_apps_host_dispatch::admin_base_url().unwrap_or(request_base)
    } else {
        request_base
    };
    let target = format!(
        "{login_base}/login?return_to={}",
        urlencoding::encode(&return_to)
    );
    Redirect::to(&target)
}

/// 301 from a legacy `/customer-apps/<uuid>/<rest>` URL to the canonical
/// `<base>/customer-apps/<org_slug>/<app_slug>/<rest>`. Looks up the row
/// before redirecting — fakes get a clean 404 so we don't leak the uuid
/// namespace to unauth'd probes via 301 vs 404 distinction.
pub(super) async fn redirect_legacy_uuid(
    uuid: Uuid,
    rest: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> Response {
    // Auth gate first — same reason as the pretty path. Probing with a uuid
    // shouldn't reveal whether it's registered.
    let identity = match BuiltInAuthenticator::new().authenticate(headers).await {
        Ok(i) => i,
        Err(_) => return redirect_to_login(headers, uri).into_response(),
    };
    if UserService::find_user_by_identity(&identity)
        .await
        .unwrap_or(None)
        .is_none()
    {
        return redirect_to_login(headers, uri).into_response();
    }

    let db = match establish_connection().await {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("DB connection failed during uuid redirect for {uuid}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let app = match Apps::find_by_id(uuid).one(&db).await {
        Ok(Some(a)) => a,
        // Flips both ways — a uuid is unknown until the app is created and
        // again once it's deleted.
        Ok(None) => return no_store_404(),
        Err(e) => {
            tracing::error!("Failed to look up custom app {uuid}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let org = match entity::prelude::Organizations::find_by_id(app.org_id)
        .one(&db)
        .await
    {
        Ok(Some(o)) => o,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let base = base_url(headers);
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let target = if rest.is_empty() {
        format!("{base}/customer-apps/{}/{}/{query}", org.slug, app.slug)
    } else {
        format!(
            "{base}/customer-apps/{}/{}/{rest}{query}",
            org.slug, app.slug
        )
    };
    // `no-store`, despite this being the one response here a client would
    // otherwise keep indefinitely — and *because* of it.
    //
    // 301 is heuristically cacheable (RFC 9111 §15.4.2) and browsers pin it
    // far harder than any negative TTL: Chrome and Firefox hold it until the
    // cache is cleared. The target embeds `org.slug` and `app.slug`, and both
    // are mutable — `invalidate_app_resolution_cache` exists at the rename and
    // delete sites for exactly that reason. Without this header, a visitor who
    // ever hit the uuid URL keeps being redirected to a slug that no longer
    // resolves, 404ing from their own cache with no server-side remedy.
    //
    // It is also auth-dependent: the gate above runs *before* the lookup so a
    // 301-vs-404 difference can't leak the uuid namespace to unauthenticated
    // probes. A shared cache storing this response would hand the
    // authenticated 301 to an anonymous prober and collapse that distinction,
    // so it must not be stored at all — `private` would leave the browser
    // pinning a stale slug, which is the other half of the problem.
    (
        StatusCode::MOVED_PERMANENTLY,
        [
            (header::LOCATION, target),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
    )
        .into_response()
}

/// Returns true if the client wants HTML — used to scope the SPA fallback
/// (`bundle_dir/index.html`) to top-level navigations only. Browsers send
/// `Accept: text/html,application/xhtml+xml,...` for navigations; `fetch()`
/// and asset XHRs send `Accept: */*` or a specific MIME type. We deliberately
/// do NOT treat `*/*` as HTML, because that's exactly the case the reviewer
/// wanted to 404 cleanly (asset for a missing file shouldn't get the SPA
/// shell back at 200). Missing Accept is rare in practice (manual `curl`
/// without flags); we treat it as HTML so the SPA still renders.
pub(crate) fn wants_html(headers: &HeaderMap) -> bool {
    let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let accept = accept.to_ascii_lowercase();
    accept.contains("text/html") || accept.contains("application/xhtml+xml")
}

/// Derive the public base URL of this oxy instance from request headers,
/// honoring `X-Forwarded-Proto` / `X-Forwarded-Host` so the redirect target
/// matches what the *browser* sees (which can differ from how oxy is
/// configured behind a reverse proxy). Falls back to `Host` and finally
/// `http://localhost:3000` (tests / direct-bind setups).
fn base_url(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_ascii_lowercase())
        .unwrap_or_else(|| "http".to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim())
        .unwrap_or("localhost:3000");
    format!("{scheme}://{host}")
}

/// The build-version half of a bundle response's cache decision — see the
/// `?v=` section of `cache_control_for`.
///
/// Both halves travel together rather than as a precomputed bool, so the one
/// function that picks the policy is also the one that decides what "matches"
/// means, and its tests can say so.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct VersionPin<'a> {
    /// The raw `v` query parameter, when the URL carried one.
    pub requested: Option<&'a str>,
    /// The `app_builds.id` (primary key) this request is actually serving.
    /// `None` for a source with no build in the store: a local folder is edited
    /// in place, so there are no fixed bytes to pin.
    pub served: Option<Uuid>,
}

impl VersionPin<'_> {
    /// `v` parses as a UUID **and** names the served build. Absent, malformed,
    /// or naming any other build is not a pin.
    fn names_served_build(self) -> bool {
        match (self.requested, self.served) {
            (Some(v), Some(served)) => Uuid::parse_str(v).is_ok_and(|v| v == served),
            _ => false,
        }
    }
}

/// The `v` parameter from a request's query string, raw; first occurrence
/// wins. Not percent-decoded — a UUID has nothing to decode, and an encoded
/// one just fails to parse and keeps the revalidating policy.
pub(super) fn requested_build_version(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| kv.strip_prefix("v="))
}

/// Pick a `Cache-Control` policy for a bundle response.
///
/// Content-hashed asset dirs are safe to cache forever and `immutable`: the
/// URL only changes when the bytes change. Next.js emits hashed files under
/// `_next/static/`; Vite, Astro, Rsbuild, and SvelteKit emit them under
/// `assets/`. HTML and unfingerprinted root files must revalidate so a new
/// deployment is picked up immediately.
///
/// ## Why HTML is `private`, not just `no-cache`
///
/// `no-cache` means "store, but revalidate before reuse" — it does **not**
/// stop a *shared* cache from storing the response. HTML responses from this
/// handler carry a per-visitor tracking `Set-Cookie`
/// (`custom_apps_tracking::session_id_for_serve`, stamped in `serve_pretty`),
/// so a shared cache that stored one would hand **every** later visitor the
/// same session id and silently collapse the Activity tab's numbers into a
/// single session.
///
/// `private` is what forbids that, and it is *a* precondition for ever
/// putting a CDN in front of this route — the "web-cache-deception" risk
/// named in `internal-docs/customer-apps-performance.md`. The HTML *body* is
/// in fact identical for every viewer of an app (`window.__OXY_APP__` holds
/// app-level identity only, never user identity); it is the cookie on the
/// response, not the bytes, that makes it unshareable.
///
/// ## What this does NOT cover: `public` on bundle bytes
///
/// Non-HTML responses still leave as `public`, while the route enforces
/// **per-app** visibility — `app.is_restricted()` means org membership alone
/// isn't enough (`custom_apps_auth`). A CDN or corporate proxy will therefore
/// store a restricted app's JS and re-serve it to anyone holding the URL,
/// with the member list never consulted. What stands between a restricted
/// bundle and a stranger today is that content-hashed filenames are
/// discoverable only from the (now `private`) HTML — obscurity, not the
/// authz gate.
///
/// That is a deliberate current position, not an oversight: bundle bytes are
/// treated as non-secret. Deriving the `public`/`private` half from the app's
/// visibility is the fix if that ever stops being true, and it costs
/// restricted apps their shared-cache hit. Read "precondition" above as
/// necessary, not sufficient.
///
/// ## Why the resolved file decides before the requested prefix
///
/// The prefix rule reads the **requested** path but the HTML test reads the
/// **resolved** one, and the SPA fallback makes those diverge: a miss on
/// `assets/index-abc123.js` resolves to `index.html`. Applying the prefix
/// rule first would return an HTML body under `public, immutable`, and since
/// Vite hashes by content, a later build shipping that same chunk would find
/// the browser (or CDN) holding a year-long HTML entry at a module-script
/// URL — a white screen with no server-side remedy. An HTML body therefore
/// never leaves here cacheable, whatever URL reached it.
///
/// ## `?v=` — a version pin, honoured only for the build actually served
///
/// A bundle-root file is unfingerprinted, so its bare URL gets five minutes
/// and no more: longer would show a stale file after a publish. The launcher's
/// icon and art are such files and it renders them on every visit, so past
/// five minutes every card image paid a round trip plus the auth gate.
/// `workspace_custom_apps::icon_art_urls` therefore appends
/// `?v=<published build pk>` — one URL per publish — and a URL that names
/// its build can be cached forever.
///
/// It is honoured **only when `v` equals the build this request is serving**
/// (`VersionPin::served`), never merely because it parses. App resolution —
/// which carries the build pointers — is cached per process for
/// `custom_apps_cache::CACHE_TTL` (60s). For up to that long after a publish,
/// a pod that has not seen it still resolves the app to the PREVIOUS build,
/// while the launcher list, read fresh on another pod, already hands the
/// browser `icon.svg?v=<new>`. That pod serves the old bytes under the new
/// URL. Were that response `immutable`, the browser would pin the old icon
/// under the new build's URL for a year, with no server-side remedy: the URL
/// does not change again until the next publish. On a mismatch the response
/// keeps the ordinary five-minute policy — the staleness a bare URL already
/// accepts — and the first render after it expires fetches the right bytes. A
/// staff draft preview (draft bytes under a published-build `v`) is the same
/// mismatch and gets the same answer.
///
/// The pin is checked after both guards above and after the hashed-prefix
/// rule, and it only ever upgrades `max-age=300`: it cannot make HTML or a
/// platform object cacheable, and a `v` naming another build cannot demote a
/// hashed asset. The service worker deliberately does not share this case —
/// see `custom_apps_asset_manifest::is_immutable_asset_path`.
pub(super) fn cache_control_for(
    request_path: &str,
    file_path: &StdPath,
    pin: VersionPin<'_>,
) -> &'static str {
    let ext = file_path.extension().and_then(|e| e.to_str());
    if matches!(ext, Some("html" | "htm")) {
        return "private, no-cache";
    }
    let trimmed = request_path.trim_start_matches('/');
    // Platform-reserved objects (`__oxy/asset-manifest.json`) are per-build state
    // that flips the moment anyone publishes, and they sit behind the same auth
    // gate as the bundle. `public, max-age=300` — what an unfingerprinted root
    // file gets — would let a shared cache hand a five-minute-old precache list
    // to a browser whose app has already moved on, and hand it to callers who
    // never passed the gate. Neither is worth trading for a cache hit on a
    // document the service worker deliberately fetches `no-store` anyway.
    if crate::server::api::custom_apps_asset_manifest::is_reserved_platform_path(trimmed) {
        return "private, no-cache";
    }
    if trimmed.starts_with("_next/static/") || trimmed.starts_with("assets/") {
        return "public, max-age=31536000, immutable";
    }
    // Equality with the SERVED build, not just a well-formed `v` — see the
    // `?v=` section above for the stale-pod race this closes.
    if pin.names_served_build() {
        return "public, max-age=31536000, immutable";
    }
    "public, max-age=300"
}

// The ETag format and `If-None-Match` comparison are shared with the admin
// SPA shell — see `server::http_cache` for why they have one home.
pub(super) use crate::server::http_cache::{if_none_match, weak_etag as etag_for};

pub(super) fn guess_content_type(path: &StdPath) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
        h
    }

    #[test]
    fn speculative_requests_are_recognised_in_every_spelling() {
        // The Speculation Rules form, which is what Chrome and Edge send.
        assert!(is_speculative_request(&headers_with(
            "sec-purpose",
            "prefetch"
        )));
        assert!(is_speculative_request(&headers_with(
            "sec-purpose",
            "prefetch;prerender"
        )));
        // The older de-facto header — older Chrome, and intermediaries.
        assert!(is_speculative_request(&headers_with("purpose", "prefetch")));
        // Firefox sends ONLY this one. Missing it made the whole guard a no-op
        // on Firefox, which is the case this assertion exists for.
        assert!(is_speculative_request(&headers_with("x-moz", "prefetch")));
        // Case-insensitive on the value, since no client normalises it.
        assert!(is_speculative_request(&headers_with("Purpose", "Prefetch")));
        assert!(is_speculative_request(&headers_with("X-moz", "Prefetch")));
    }

    /// The manifest is per-build state behind an auth gate, so it must not be
    /// stored by a shared cache — the same reason HTML is `private`, minus the
    /// cookie.
    #[test]
    fn reserved_platform_objects_are_never_shared_cacheable() {
        assert_eq!(
            cache_control_for(
                "__oxy/asset-manifest.json",
                StdPath::new("__oxy/asset-manifest.json"),
                VersionPin::default(),
            ),
            "private, no-cache"
        );
        // An app's own JSON at a non-reserved path keeps the ordinary policy.
        assert_eq!(
            cache_control_for(
                "data.json",
                StdPath::new("data.json"),
                VersionPin::default()
            ),
            "public, max-age=300"
        );
    }

    const IMMUTABLE: &str = "public, max-age=31536000, immutable";
    const REVALIDATE: &str = "public, max-age=300";
    const SERVED: Uuid = Uuid::from_u128(0x7);

    fn serving(v: Option<&str>) -> VersionPin<'_> {
        VersionPin {
            requested: v,
            served: Some(SERVED),
        }
    }

    fn root_file(pin: VersionPin<'_>) -> &'static str {
        cache_control_for("icon.svg", StdPath::new("icon.svg"), pin)
    }

    #[test]
    fn a_version_naming_the_served_build_is_immutable() {
        let v = SERVED.to_string();
        assert_eq!(root_file(serving(Some(&v))), IMMUTABLE);
        // Art commonly lives below the root; the pin is not a root-only rule.
        assert_eq!(
            cache_control_for(
                "shots/card.png",
                StdPath::new("shots/card.png"),
                serving(Some(&v))
            ),
            IMMUTABLE
        );
    }

    /// The stale-pod race: a pod still on the previous build receives the new
    /// build's URL. It must not pin the old bytes under it.
    #[test]
    fn a_version_naming_any_other_build_keeps_the_revalidating_policy() {
        let other = Uuid::from_u128(0x8).to_string();
        assert_eq!(root_file(serving(Some(&other))), REVALIDATE);
    }

    #[test]
    fn no_version_keeps_the_revalidating_policy() {
        assert_eq!(root_file(serving(None)), REVALIDATE);
        // A source with no build (a local folder) can never be pinned, even by
        // a `v` that happens to parse.
        let v = SERVED.to_string();
        assert_eq!(
            root_file(VersionPin {
                requested: Some(&v),
                served: None
            }),
            REVALIDATE
        );
    }

    #[test]
    fn a_version_that_is_not_a_uuid_keeps_the_revalidating_policy() {
        let trailing = format!("{SERVED}x");
        for v in ["", "latest", "1", "not-a-uuid", trailing.as_str()] {
            assert_eq!(root_file(serving(Some(v))), REVALIDATE, "v={v:?}");
        }
    }

    /// Both guards come first. A matching pin must not make HTML shareable (it
    /// carries the tracking cookie) — including the SPA fallback, where an
    /// asset URL resolved to `index.html` — nor a per-build platform object.
    #[test]
    fn a_matching_version_never_overrides_the_html_or_platform_guards() {
        let v = SERVED.to_string();
        assert_eq!(
            cache_control_for("index.html", StdPath::new("index.html"), serving(Some(&v))),
            "private, no-cache"
        );
        assert_eq!(
            cache_control_for("icon.svg", StdPath::new("index.html"), serving(Some(&v))),
            "private, no-cache"
        );
        assert_eq!(
            cache_control_for(
                "__oxy/asset-manifest.json",
                StdPath::new("__oxy/asset-manifest.json"),
                serving(Some(&v))
            ),
            "private, no-cache"
        );
    }

    #[test]
    fn a_mismatched_version_never_demotes_a_hashed_asset() {
        let other = Uuid::from_u128(0x8).to_string();
        assert_eq!(
            cache_control_for(
                "assets/x.js",
                StdPath::new("assets/x.js"),
                serving(Some(&other))
            ),
            IMMUTABLE
        );
    }

    #[test]
    fn reads_the_v_parameter_and_nothing_that_merely_contains_it() {
        assert_eq!(requested_build_version(None), None);
        assert_eq!(requested_build_version(Some("")), None);
        assert_eq!(requested_build_version(Some("v=abc")), Some("abc"));
        assert_eq!(requested_build_version(Some("x=1&v=abc")), Some("abc"));
        assert_eq!(requested_build_version(Some("v=a&v=b")), Some("a"));
        assert_eq!(requested_build_version(Some("vv=abc")), None);
        assert_eq!(requested_build_version(Some("x=v=abc")), None);
        assert_eq!(requested_build_version(Some("v")), None);
    }

    #[test]
    fn a_real_navigation_is_not_speculative() {
        assert!(!is_speculative_request(&HeaderMap::new()));
        // `Sec-Purpose` exists for non-speculative purposes too; only the
        // speculative tokens count.
        assert!(!is_speculative_request(&headers_with("sec-purpose", "")));
        assert!(!is_speculative_request(&headers_with(
            "purpose",
            "subresource"
        )));
    }
}
