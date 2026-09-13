//! Bundle-source resolution and file serving for custom apps.
//!
//! The per-app source decision (v0 / local / s3) is recorded at register
//! time; this module turns a resolved source into a served response —
//! reading objects from S3 through the in-memory bundle cache, or from a
//! local bundle dir with path-traversal + symlink-escape defenses.

use std::path::{Component, Path as StdPath, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sea_orm::EntityTrait;
use tokio::fs;
use uuid::Uuid;

use crate::server::api::custom_apps_asset_manifest::{self as asset_manifest, AssetManifest};
use crate::server::api::custom_apps_bundle_cache;
use crate::server::api::custom_apps_cache::{
    CACHE_CHANNEL_LOCAL, cached_build, cached_canonical_dir, invalidate_cached_canonical_dir,
    set_cached_build, set_cached_canonical_dir,
};
use crate::server::api::custom_apps_html_cache::{self as html_cache, RenderedHtml};
use crate::server::api::custom_apps_precompress as precompress;

use super::headers::*;
use super::rewrite::*;

/// Browser-side runtime identity for a served custom app. Injected into
/// every HTML response under `<script>window.__OXY_APP__ = {…}</script>`
/// so the bundle's SDK can call `/api/<projectId>/…` without having those
/// ids baked in at build time. See sdk/typescript/src/config.ts.
#[derive(serde::Serialize, Debug, Clone)]
pub(crate) struct AppRuntimeConfig {
    #[serde(rename = "appId")]
    pub app_id: Uuid,
    pub slug: String,
    #[serde(rename = "orgId")]
    pub org_id: Uuid,
    #[serde(rename = "orgSlug")]
    pub org_slug: String,
    #[serde(rename = "projectId")]
    pub project_id: Uuid,
    pub branch: String,
    /// Empty string = same-origin. v1 always emits empty; reserved for a
    /// future where custom apps live on a different host than the API.
    #[serde(rename = "apiBaseUrl")]
    pub api_base_url: String,
    /// The URL prefix every asset of this app resolves under, with a trailing
    /// slash — `"/customer-apps/<org>/<app>/"`.
    ///
    /// The **same value on both surfaces**, which is not obvious: a
    /// subdomain-served app's browser URLs also carry the subpath, because the
    /// bundle bakes it and `already_canonicalized` in
    /// `custom_apps_host_dispatch` deliberately passes it through rather than
    /// double-prefixing. One base means one service-worker scope and one set of
    /// preload hints for both.
    #[serde(rename = "basePath")]
    pub base_path: String,
    /// Whether the client runtime should register the platform service worker.
    /// `false` opts an app out via its manifest.
    #[serde(rename = "serviceWorker")]
    pub service_worker: bool,
    /// Whether the client runtime should auto-instrument. `false` leaves only
    /// the server-side view rows, which no app can opt out of.
    ///
    /// Since 2026-09 this covers the AUTHOR'S PRODUCT ANALYTICS only —
    /// pageviews, web vitals, engagement. The two platform health signals
    /// (`oxy-app-ready`, `oxy-error`) ignore it. See `runtime.js`.
    pub analytics: bool,
    /// The build that served this document, so a client error can name the code
    /// it was thrown by. Resolved at inject time rather than when the beacon
    /// lands: by then the app may have been re-published, and attributing a
    /// stack to the wrong build makes it unresolvable against a source map.
    ///
    /// `from_app` seeds this with the PUBLISHED build because the channel is
    /// not known that early; `serve_pretty` overwrites it with the build it
    /// actually serves once the channel is picked. Empty for a proxied source,
    /// which has no build in the store to name.
    #[serde(rename = "buildId")]
    pub build_id: String,
}

impl AppRuntimeConfig {
    pub(super) fn from_app(app: &entity::apps::Model, org_slug: &str) -> Self {
        Self {
            app_id: app.id,
            slug: app.slug.clone(),
            org_id: app.org_id,
            org_slug: org_slug.to_string(),
            project_id: app.project_id,
            branch: app.branch.clone(),
            api_base_url: String::new(),
            base_path: format!("/customer-apps/{org_slug}/{}/", app.slug),
            // Both default on. An app opts out through its manifest
            // (`performance.serviceWorker`, `analytics`), and the override is
            // applied in `render_html` — not here, because the build (and so
            // its manifest) is not known when this config is built.
            service_worker: true,
            analytics: true,
            build_id: app
                .published_build_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        }
    }
}

/// Bundle subtrees the serve plane must never hand back as files.
///
/// `functions/` holds the app's compiled Oxy Functions. They ship *inside* the
/// same bundle as the frontend (`oxyc publish` writes them to
/// `<bundle>/functions/<name>.js`), but they are server-side handlers: the
/// isolate loads them straight out of the build store via
/// `custom_apps_functions`, never over HTTP. Nothing legitimate fetches them
/// through this route.
///
/// Without this list the static-asset route served them to anyone who cleared
/// the app's auth gate — handler logic, embedded query SQL, and (before the
/// `--sources-content=false` change in `publish.rs`) the author's original
/// TypeScript, carried in the inline sourcemap.
const SERVER_ONLY_PREFIXES: &[&str] = &["functions"];

/// Is this bundle-relative request path inside a server-only subtree?
///
/// Compares the first real path segment, skipping empty and `.` components so
/// `//functions/x` and `./functions/x` can't slip past, and case-insensitively
/// so a case-insensitive filesystem (a macOS dev box) doesn't answer
/// `Functions/x` when Linux wouldn't. Traversal (`..`) is already rejected
/// upstream by `resolve_safe` / `is_safe_rel`.
///
/// **A child segment is required.** The artifact is always
/// `functions/<name>.js` (see `custom_apps_functions`), never bare
/// `functions`, so matching the lone segment protected nothing and cost real
/// pages: an app routing `/functions` client-side, or a static export whose
/// own `functions.html` / `functions/index.html` the `.html`-suffix candidate
/// exists to serve, would silently get the site root at `200`. That hit apps
/// with no Oxy Functions at all, and `LocalFolder` sources that cannot
/// contain an artifact by construction.
///
/// Prefix-scoped on purpose: `assets/functions.js` and `my-functions/x` are
/// ordinary frontend files and stay servable.
pub(super) fn is_server_only_path(rest: &str) -> bool {
    let mut segments = rest.split('/').filter(|seg| !seg.is_empty() && *seg != ".");
    let Some(first) = segments.next() else {
        return false;
    };
    let reserved = SERVER_ONLY_PREFIXES
        .iter()
        .any(|p| first.eq_ignore_ascii_case(p));
    reserved && segments.next().is_some()
}

/// Does this bundle-relative path name a source map?
///
/// Suffix match, case-insensitive: a case-insensitive filesystem would
/// otherwise serve `INDEX.JS.MAP` on macOS and 404 it on Linux, and a gate whose
/// answer depends on the host's filesystem is not a gate.
pub(super) fn is_source_map(rest: &str) -> bool {
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    path.to_ascii_lowercase().ends_with(".map")
}

/// What the serve plane should do with a request, once the server-only check
/// has had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServeGuard {
    /// Ordinary request — resolve it normally.
    Allow,
    /// Blocked, but the caller accepts HTML: hand back the SPA shell so a
    /// client-side route of the same name still renders.
    SpaShell,
    /// Blocked with no HTML fallback available — 404, identical to what a
    /// non-existent path returns, so the route doesn't confirm which handlers
    /// an app has.
    NotFound,
}

/// The single decision both serve branches take. Shared deliberately: the S3
/// and local-dir wirings resolve files differently enough that a test of one
/// would not catch a regression in the other, so the part worth getting right
/// lives in one tested function instead of twice in prose.
pub(super) fn serve_guard(rest: &str, accepts_html: bool) -> ServeGuard {
    // A source map is not a public asset. `guess_content_type` maps `.map` to
    // `application/json`, so before this check every `.map` in a published
    // bundle was downloadable by anyone who could open the app — handing the
    // app's original source, comments and unused branches included, to every
    // viewer. Maps stay in the build store and are applied server-side when an
    // app-admin reads a client-error stack (`custom_apps_sourcemap`).
    //
    // Always `NotFound`, never the SPA shell: the caller is devtools or a fetch
    // expecting JSON, and answering with HTML would turn a clean 404 into a
    // parse error somebody has to debug. The trade is real — an author loses
    // in-browser stack resolution — and it is made deliberately, because the
    // maps were reaching an audience wider than the author assumed.
    if is_source_map(rest) {
        return ServeGuard::NotFound;
    }
    if !is_server_only_path(rest) {
        return ServeGuard::Allow;
    }
    if accepts_html {
        ServeGuard::SpaShell
    } else {
        ServeGuard::NotFound
    }
}

/// The object key the S3 branch should fetch for a raw request path, or
/// `None` to 404.
///
/// Extracted so the branch that serves production is testable at all — the
/// rest of `serve_from_s3_build` needs a database and a build store, which is
/// why this decision previously had no coverage and the local-dir test could
/// not stand in for it.
///
/// **Order matters: guard the RAW path, then rewrite.** Guarding the rewritten
/// value turned `/functions/` into `functions/index.html`, which *does* have a
/// child segment and so was blocked — handing the site root to a static
/// export's own `/functions/` page, while the local-dir branch (which guards
/// `rest`) served it correctly. Same predicate, same input, both branches.
///
/// Guarding first is not a hole: the rewrite only ever appends `index.html` to
/// a directory-style path, and an artifact is `<name>.js` with `name` matching
/// `^[a-z][a-z0-9-]{0,63}$` — enforced at publish by
/// `custom_apps_publish::record_functions` (`is_valid_function_name`), so the
/// rewrite can never synthesise a key that reaches one.
fn s3_object_key(rest: &str, accepts_html: bool) -> Option<String> {
    match serve_guard(rest, accepts_html) {
        ServeGuard::NotFound => None,
        ServeGuard::SpaShell => Some("index.html".to_string()),
        ServeGuard::Allow => {
            // Empty or directory-style paths serve that directory's entry.
            let t = rest.trim_start_matches('/');
            Some(if t.is_empty() || t.ends_with('/') {
                format!("{t}index.html")
            } else {
                t.to_string()
            })
        }
    }
}

/// Channel resolution for S3-source serve:
/// - staff with the preview-draft cookie set → draft
/// - app has been published → published (default for both staff and customer)
/// - otherwise → draft (only staff reaches here; the auth gate blocked
///   customers on unpublished apps)
pub(super) fn resolve_channel(
    staff_wants_draft: bool,
    is_published: bool,
) -> crate::server::api::custom_apps_sync::Channel {
    use crate::server::api::custom_apps_sync::Channel;
    if staff_wants_draft {
        return Channel::Draft;
    }
    if is_published {
        Channel::Published
    } else {
        Channel::Draft
    }
}

/// Serve a custom-app file directly from S3 (the new publish pipeline).
/// Resolves the build's files from its `app_builds` row and reads objects
/// through the in-memory bundle cache — no local state dir, so any node can
/// serve any build. HTML gets the same base-path rewrite + `window.__OXY_APP__`
/// injection as the legacy disk path; the cache key includes the build id, so
/// a promote/rollback serves fresh bytes with no explicit invalidation.
///
/// `requested_version` is the request's raw `?v=` (see `cache_control_for`).
/// It is paired here with the build row this request actually loaded — not
/// with whatever the caller believes is live — because that pairing is the
/// whole of what makes an `immutable` answer safe.
pub(crate) async fn serve_from_s3_build(
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    build_pk: Uuid,
    rest: &str,
    requested_version: Option<&str>,
    runtime: &AppRuntimeConfig,
    headers: &HeaderMap,
) -> Response {
    let build = match load_build(db, app_id, build_pk).await {
        Ok(b) => b,
        Err(response) => return response,
    };
    let pin = VersionPin {
        requested: requested_version,
        served: Some(build.id),
    };

    let Some(requested) = s3_object_key(rest, wants_html(headers)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // A `.br` sibling is an internal representation, not an addressable asset.
    // Without this it resolves as an ordinary object and hands back raw brotli
    // as `application/octet-stream` with no `Content-Encoding` — a body no
    // client can read, under a URL nothing links. 404 is the honest answer:
    // the resource here is the identity object, reached without the suffix.
    // Build-store path only; a LocalFolder app's own `dist/` may legitimately
    // ship `.br` files the dev put there, and those are not ours to hide.
    if precompress::is_precompressed_path(&requested) {
        return no_store_404();
    }

    // Pre-compressed fast path, BEFORE the identity fetch. A `.br` sibling
    // exists only when its identity object does (both come off the same file
    // list at publish), and everything downstream — mime, cache policy —
    // derives from the request path rather than the fetched bytes. So on a
    // hit the identity object is never needed: one store round-trip instead
    // of two, and one LRU entry instead of two.
    if let Some(response) =
        try_precompressed(app_id, &build.build_id, &requested, pin, headers).await
    {
        return response;
    }

    // Try the requested object; on a miss, a navigation falls back to the
    // SPA shell (`index.html`).
    let (rel_used, bytes) =
        match custom_apps_bundle_cache::get_or_fetch(app_id, &build.build_id, &requested).await {
            Ok(Some(b)) => (requested.clone(), b),
            // Only a navigation gets the SPA shell. An asset XHR for a
            // missing file must get a real 404 rather than an opaque 200
            // carrying HTML — the `wants_html` gate `serve_from_dir` applies
            // via `allow_spa_fallback`, which this path had been missing.
            //
            // The *gate* now matches the disk path; the *ladder* does not.
            // `serve_file` also tries `<path>.html` and `<path>/index.html`
            // before the root shell, so a multi-page static export (Next.js
            // `trailingSlash: false`, Astro) resolves `/about` to
            // `about.html` from a local folder but to the shell here. Left
            // as-is deliberately: this is the hot navigation path and the
            // extra rungs would add two store round-trips per cold client
            // route on the SPA case this PR is tuning for.
            Ok(None) if !wants_html(headers) => return no_store_404(),
            Ok(None) => {
                match custom_apps_bundle_cache::get_or_fetch(app_id, &build.build_id, "index.html")
                    .await
                {
                    Ok(Some(b)) => ("index.html".to_string(), b),
                    Ok(None) => return no_store_404(),
                    Err(e) => {
                        tracing::error!("app {app_id}: S3 read index.html failed: {e}");
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                }
            }
            Err(e) => {
                tracing::error!("app {app_id}: S3 read {requested} failed: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };

    let resolved_path = StdPath::new(&rel_used);
    let mime = guess_content_type(resolved_path);
    let cache = cache_control_for(&requested, resolved_path, pin);

    // HTML is transformed on the way out (base-path rewrite + identity
    // injection), so it is never pre-compressed — the stored bytes and the
    // sent bytes differ by construction. The `CompressionLayer` still
    // compresses it on the fly.
    if mime.starts_with("text/html") {
        return html_response(
            HtmlBuild {
                app_id,
                build_id: &build.build_id,
                object_key: &rel_used,
            },
            &bytes,
            mime,
            cache,
            resolved_path,
            runtime,
            headers,
        )
        .await;
    }
    asset_response(bytes, mime, cache, None)
}

/// Load the `app_builds` row for a channel pointer, through the TTL cache.
///
/// Keyed by PK: the fields the serve path reads (`build_id`) are fixed when
/// the publish pipeline writes the row, and a promote/rollback repoints
/// `apps` at a DIFFERENT pk rather than rewriting this one — so a cache hit
/// can never serve a superseded build.
///
/// `Err` carries the response to return as-is.
async fn load_build(
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    build_pk: Uuid,
) -> Result<entity::app_builds::Model, Response> {
    if let Some(b) = cached_build(build_pk) {
        return Ok(b);
    }
    match entity::app_builds::Entity::find_by_id(build_pk)
        .one(db)
        .await
    {
        Ok(Some(b)) => {
            set_cached_build(build_pk, b.clone());
            Ok(b)
        }
        Ok(None) => {
            tracing::error!("app {app_id}: build pointer {build_pk} has no app_builds row");
            Err(no_store_404())
        }
        Err(e) => {
            tracing::error!("app {app_id}: failed to load build {build_pk}: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// Identity of the build an HTML document came out of — the three components
/// that, with the org and app slugs already on `AppRuntimeConfig`, make the
/// rendered document's cache key complete.
///
/// A struct rather than three more positional parameters because
/// `html_response` already took six, and `(Uuid, &str, &str)` at a call site
/// is exactly the shape you transpose by accident.
pub(super) struct HtmlBuild<'a> {
    pub app_id: Uuid,
    pub build_id: &'a str,
    /// The object key that actually resolved — `index.html` for an SPA
    /// fallback, not the URL that was asked for. Two URLs resolving to the same
    /// document must share one cache entry.
    pub object_key: &'a str,
}

/// Build the response for an HTML entry document.
///
/// The transform — base-path rewrite, `window.__OXY_APP__` + client-runtime
/// injection, weak ETag over the FINAL bytes (the transform is what makes it
/// weak) — is **memoized per build** in `custom_apps_html_cache`. Nothing in it
/// depends on the viewer, so redoing it per navigation was pure repetition
/// directly in front of the app's first paint. See that module for why the key
/// is also the whole invalidation story.
///
/// Three headers are added on top of the body:
///
/// - **`Link: rel=preload/modulepreload`** for the build's entry assets, from
///   the asset manifest. This is the first-load win: the entry chunks start
///   downloading while the HTML is still streaming, rather than one round trip
///   later when the parser reaches the `<script>` tag. Emitted on the 304 as
///   well — a revalidated shell needs its assets just as much as a fresh one,
///   and a 304 body cannot carry the `<link>` tags.
/// - **`x-oxy-build`** — the live build id. The service worker reads it off
///   every navigation and re-precaches in place when it moves, so a publish is
///   picked up without waiting for the worker itself to be replaced.
/// - **`Cache-Tag`** — `app-<id>` and `build-<id>`. Inert at the origin, and
///   emitted for the CDN step that has not been taken yet: a tag-capable edge
///   (Cloudflare, Fastly's `Surrogate-Key` equivalent) can then purge exactly
///   this app, or exactly one build, on publish. CloudFront — the candidate in
///   `customer-apps-performance.md` — has no tag purge and would invalidate by
///   path instead, so this is insurance rather than a plan. It costs one header
///   now and is awkward to retrofit once an edge is already holding untagged
///   objects.
async fn html_response(
    build: HtmlBuild<'_>,
    bytes: &[u8],
    mime: &str,
    cache: &str,
    resolved_path: &StdPath,
    runtime: &AppRuntimeConfig,
    headers: &HeaderMap,
) -> Response {
    let rendered = match html_cache::get(
        build.app_id,
        build.build_id,
        build.object_key,
        &runtime.org_slug,
        &runtime.slug,
    ) {
        Some(hit) => hit,
        None => {
            let rendered = render_html(bytes, resolved_path, runtime, build.app_id, {
                load_asset_manifest(build.app_id, build.build_id).await
            });
            html_cache::put(
                build.app_id,
                build.build_id,
                build.object_key,
                &runtime.org_slug,
                &runtime.slug,
                rendered.clone(),
            );
            rendered
        }
    };
    finish_html(rendered, mime, cache, build.build_id, build.app_id, headers)
}

/// The pure half: bytes in, rendered document out. Split from the caching and
/// header work so it can be exercised without a build store — the transform is
/// the part with the interesting failure modes.
pub(super) fn render_html(
    bytes: &[u8],
    resolved_path: &StdPath,
    runtime: &AppRuntimeConfig,
    app_id: Uuid,
    manifest: Option<AssetManifest>,
) -> RenderedHtml {
    // The build's own opt-outs ride in its asset manifest, so they are applied
    // to a copy of the runtime config here rather than being resolved when the
    // config is first built — which happens before we know which build is
    // serving. A build with no manifest keeps the defaults (both on).
    let mut runtime = runtime.clone();
    if let Some(m) = &manifest {
        runtime.service_worker = m.client.service_worker;
        runtime.analytics = m.client.analytics;
    }
    let rewritten = rewrite_bundle_base_path(bytes, &runtime.base_path, app_id);
    let body = Bytes::from(inject_app_config(&rewritten, &runtime, resolved_path));
    let etag = etag_for(&body);
    let link = manifest.and_then(|m| asset_manifest::preload_link_header(&m, &runtime.base_path));
    RenderedHtml { body, etag, link }
}

/// Turn a rendered document into a 200 or a 304, with the shared header set on
/// both.
fn finish_html(
    rendered: RenderedHtml,
    mime: &str,
    cache: &str,
    build_id: &str,
    app_id: Uuid,
    headers: &HeaderMap,
) -> Response {
    let not_modified = if_none_match(headers, &rendered.etag);
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        (
            [(header::CONTENT_TYPE, mime.to_string())],
            Body::from(rendered.body),
        )
            .into_response()
    };
    let h = response.headers_mut();
    if let Ok(v) = header::HeaderValue::from_str(cache) {
        h.insert(header::CACHE_CONTROL, v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&rendered.etag) {
        h.insert(header::ETAG, v);
    }
    if let Some(link) = rendered.link.as_deref()
        && let Ok(v) = header::HeaderValue::from_str(link)
    {
        h.insert(header::LINK, v);
    }
    if let Ok(v) = header::HeaderValue::from_str(build_id) {
        h.insert("x-oxy-build", v);
    }
    if let Ok(v) = header::HeaderValue::from_str(&format!("app-{app_id},build-{build_id}")) {
        h.insert("cache-tag", v);
    }
    response
}

/// Read a build's asset manifest, or `None` when it has none.
///
/// Goes through the bundle cache, so a build published before manifests existed
/// costs exactly one doomed probe per process — the absence is remembered — and
/// every caller degrades to "no preload hints, no precache list" rather than
/// failing. That is the whole compatibility story for older builds.
async fn load_asset_manifest(app_id: Uuid, build_id: &str) -> Option<AssetManifest> {
    let bytes = custom_apps_bundle_cache::get_or_fetch(
        app_id,
        build_id,
        asset_manifest::ASSET_MANIFEST_PATH,
    )
    .await
    .inspect_err(|e| tracing::warn!("app {app_id}: asset manifest read failed: {e}"))
    .ok()
    .flatten()?;
    serde_json::from_slice::<AssetManifest>(&bytes)
        .inspect_err(|e| tracing::warn!("app {app_id}: asset manifest is unreadable: {e}"))
        .ok()
        .filter(|m| m.schema_version == asset_manifest::SCHEMA_VERSION)
}

/// Serve the `.br` sibling written at publish time, if the client accepts
/// brotli and the sibling exists. `None` means "fall through to the identity
/// object" — the caller has not fetched it yet, which is the point.
///
/// The probe is filtered by extension, so a request that could never have a
/// sibling (an SPA route with no extension, an image, a font) costs nothing.
/// For the rest, a build published before pre-compression existed misses
/// once per object per process — the absence is remembered by the bundle
/// cache — and the response falls through to the `CompressionLayer` exactly
/// as it did before.
async fn try_precompressed(
    app_id: Uuid,
    build_id: &str,
    requested: &str,
    pin: VersionPin<'_>,
    headers: &HeaderMap,
) -> Option<Response> {
    if !precompress::accepts_brotli(headers)
        || !precompress::is_precompressible_extension(requested)
    {
        return None;
    }
    let br_path = format!("{requested}{}", precompress::PRECOMPRESSED_SUFFIX);
    match custom_apps_bundle_cache::get_or_fetch(app_id, build_id, &br_path).await {
        Ok(Some(br_bytes)) => {
            // A `.br` hit means the identity object exists, so no SPA
            // fallback is possible here: the resolved path IS the request.
            let path = StdPath::new(requested);
            Some(asset_response(
                br_bytes,
                guess_content_type(path),
                cache_control_for(requested, path, pin),
                Some("br"),
            ))
        }
        // No sibling — the caller fetches the identity object.
        Ok(None) => None,
        // A store failure on the OPTIONAL variant must never fail the
        // request; fall through and let the identity fetch decide.
        Err(e) => {
            tracing::warn!("app {app_id}: pre-compressed probe {br_path} failed: {e}");
            None
        }
    }
}

/// Build the response for a static (non-HTML) asset, pre-compressed or not.
///
/// `Vary: accept-encoding` goes on **both** forms: without it a shared cache
/// could store the brotli body and hand it to a client that never asked for
/// it. Load-bearing the moment anything caches upstream of us.
fn asset_response(
    bytes: Bytes,
    mime: &str,
    cache: &str,
    encoding: Option<&'static str>,
) -> Response {
    let mut response = (
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
            (header::VARY, "accept-encoding".to_string()),
        ],
        Body::from(bytes),
    )
        .into_response();
    if let Some(encoding) = encoding {
        response.headers_mut().insert(
            header::CONTENT_ENCODING,
            header::HeaderValue::from_static(encoding),
        );
    }
    response
}

pub(super) async fn serve_from_local(
    id: Uuid,
    configured_path: &StdPath,
    rest: &str,
    headers: &HeaderMap,
    runtime: &AppRuntimeConfig,
) -> Response {
    // The configured path IS the bundle dir — the dev points us
    // directly at the directory that holds `index.html` + assets.
    // We used to append `out/` on the assumption it was always a
    // Next.js static export; that broke Vite (`dist/`), Astro
    // (`dist/`), Rsbuild (`dist/`), and anything with a custom out
    // dir. Today the convention is explicit on the dev's side: point
    // at the right folder, however your bundler names it.
    serve_from_dir(
        id,
        CACHE_CHANNEL_LOCAL,
        configured_path,
        rest,
        headers,
        runtime,
    )
    .await
}

/// Shared serving path for both local-folder and s3 sources. Resolves the
/// per-uuid canonical bundle dir (cached), then defers to `serve_file` for
/// the path-traversal-safe + symlink-defended file read.
///
/// `channel_key` keys the cache so draft and published bundle dirs don't
/// cross-contaminate (a staff preview-draft request must never be cached
/// and served back to a customer asking for published, and vice versa).
/// Pass `CACHE_CHANNEL_LOCAL` for local-folder sources; otherwise pass
/// the active S3 channel's `.as_str()` value.
async fn serve_from_dir(
    id: Uuid,
    channel_key: &'static str,
    bundle_dir: &StdPath,
    rest: &str,
    headers: &HeaderMap,
    runtime: &AppRuntimeConfig,
) -> Response {
    let canonical_dir = if let Some(p) = cached_canonical_dir(id, channel_key) {
        p
    } else {
        match bundle_dir.canonicalize() {
            Ok(p) if p.is_dir() => {
                set_cached_canonical_dir(id, channel_key, p.clone());
                p
            }
            _ => {
                // Echo the resolved path back so the operator can see at a
                // glance whether the wrong directory is configured — common
                // cause for LocalFolder apps after the implicit `out/`
                // suffix was dropped, and for S3 apps that haven't been
                // synced yet.
                tracing::warn!(
                    "Bundle dir missing or unresolvable for custom app {id} (expected {bundle_dir:?})"
                );
                return (
                    StatusCode::NOT_FOUND,
                    // Flips to 200 the moment the bundle syncs — see
                    // `no_store_404`, which this mirrors with a body.
                    [(header::CACHE_CONTROL, "no-store")],
                    format!(
                        "Bundle not deployed for custom app {id}.\n\
                         Server looked for files at: {}\n\n\
                         Common causes:\n\
                         - LocalFolder source: source_config.path doesn't point at the directory containing index.html.\n\
                         - S3 source: bundle hasn't been synced yet, or publish ran against an empty draft prefix.",
                        bundle_dir.display()
                    ),
                )
                    .into_response();
            }
        }
    };
    let allow_spa_fallback = wants_html(headers);
    let response = serve_file(&canonical_dir, rest, allow_spa_fallback, runtime).await;
    // If the symlink-escape guard fired, the canonical dir we cached points
    // at something suspicious — invalidate now so a follow-up request after
    // the operator fixes the symlink doesn't have to wait CACHE_TTL.
    if response.status() == StatusCode::FORBIDDEN {
        invalidate_cached_canonical_dir(id, channel_key);
    }
    response
}

/// Serve a file from `bundle_dir`. `bundle_dir` MUST be a canonicalized,
/// absolute path — the symlink-escape check at the bottom compares against
/// it as a fixed prefix.
async fn serve_file(
    bundle_dir: &StdPath,
    rest: &str,
    allow_spa_fallback: bool,
    runtime: &AppRuntimeConfig,
) -> Response {
    let Some(candidate) = resolve_safe(bundle_dir, rest) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // Candidate order: literal path → `.html` suffix (Next.js
    // trailingSlash=false) → index.html in the requested directory. The
    // bundle-root index.html SPA fallback is appended only when the request
    // accepts HTML — that way an asset XHR for a missing file still gets a
    // proper 404 instead of an opaque 200 with the SPA shell, which makes
    // operator debugging much harder.
    // Server-only subtrees (`functions/<name>.js`) contribute NO real-file
    // candidate — same `serve_guard` the S3 branch takes. `allow_spa_fallback`
    // is `wants_html`, so a blocked navigation still reaches the shell below
    // and a blocked asset fetch falls through to the 404.
    let mut candidates = match serve_guard(rest, allow_spa_fallback) {
        ServeGuard::Allow => vec![
            candidate.clone(),
            candidate.with_extension("html"),
            candidate.join("index.html"),
        ],
        ServeGuard::SpaShell | ServeGuard::NotFound => Vec::new(),
    };
    // The SPA fallback is unconditional otherwise, so it would hand the shell to
    // a `.map` request with `Accept: text/html` even though `serve_guard` said
    // NotFound — the local-folder path re-opening the hole the S3 path closes.
    // `serve_guard` alone cannot prevent this: it decides candidates, and this
    // push happens after.
    if allow_spa_fallback && !is_source_map(rest) {
        candidates.push(bundle_dir.join("index.html"));
    }
    let resolved = first_existing(&candidates).await;

    let Some(path) = resolved else {
        return no_store_404();
    };

    // Symlink-escape defense: bundles come from CI output, not a hand-curated
    // directory, so a symlink inside `<bundle_dir>` that points outside it
    // (`out/secrets -> /etc/passwd`) would otherwise be served. Canonicalize
    // and verify the real file lives inside the canonical bundle root.
    let canon = match fs::canonicalize(&path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("Failed to canonicalize {path:?}: {e}");
            return no_store_404();
        }
    };
    if !canon.starts_with(bundle_dir) {
        tracing::warn!(
            "Refusing to serve {canon:?}: escapes bundle root {bundle_dir:?} via symlink"
        );
        return StatusCode::FORBIDDEN.into_response();
    }

    match fs::read(&canon).await {
        Ok(bytes) => {
            let mime = guess_content_type(&canon);
            // No version pin: a local folder is edited in place, so no build's
            // bytes are fixed and a `?v=` must never make them immutable.
            let cache = cache_control_for(rest, &canon, VersionPin::default());
            // HTML responses get two passes:
            //   1. base-path rewrite — fixes the build-time-vs-serve-time
            //      mismatch that bites when an engineer renames a slug
            //      without rebuilding (the bundle's `<script src=...>`
            //      hardcodes the absolute base it was built with).
            //   2. runtime identity injection — `window.__OXY_APP__` into
            //      `<head>` so the SDK can read identity at runtime.
            //
            // Non-HTML responses pass through unchanged. The rewrite is
            // safe to skip for them because Vite/Next-style bundles only
            // emit absolute base-path strings in the HTML entry; JS/CSS
            // chunks reference each other relatively at runtime.
            let body_bytes = if mime.starts_with("text/html") {
                // Same transform the build-store branch runs, minus the
                // memoization: a local-folder bundle is a directory the dev is
                // actively editing, so there is no build id to key a rendered
                // copy on and caching one would serve their last save forever.
                // No asset manifest either — it is written at publish, and this
                // source never publishes.
                render_html(&bytes, &canon, runtime, runtime.app_id, None)
                    .body
                    .to_vec()
            } else {
                bytes
            };
            (
                [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)],
                Body::from(body_bytes),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to read bundle file {canon:?}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Join `bundle_dir` with `rest`, rejecting paths that try to escape via
/// `..` or absolute components. Without this guard a request for
/// `../../etc/passwd` would happily traverse out of the bundle root.
fn resolve_safe(bundle_dir: &StdPath, rest: &str) -> Option<PathBuf> {
    let rest = rest.trim_start_matches('/');
    let rel = StdPath::new(rest);
    for c in rel.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => return None,
        }
    }
    Some(bundle_dir.join(rel))
}

async fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    for p in paths {
        if let Ok(meta) = fs::metadata(p).await
            && meta.is_file()
        {
            return Some(p.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source maps are never served. They were, silently, because
    /// `guess_content_type` answers `application/json` for `.map` — so an
    /// app's original source was downloadable by every viewer.
    #[test]
    fn source_maps_are_never_served() {
        assert!(is_source_map("assets/index-a3f2.js.map"));
        assert!(is_source_map("assets/index.css.map"));
        assert!(
            is_source_map("ASSETS/INDEX.JS.MAP"),
            "a case-insensitive filesystem must not decide this"
        );
        assert!(
            is_source_map("assets/i.js.map?v=2"),
            "a query must not defeat it"
        );

        // Ordinary assets whose names merely contain the letters.
        assert!(!is_source_map("assets/roadmap.js"));
        assert!(!is_source_map("assets/index.js"));

        // 404, never the SPA shell: the caller wants JSON, and HTML would turn
        // a clean miss into a parse error someone has to debug.
        assert_eq!(
            serve_guard("assets/index.js.map", true),
            ServeGuard::NotFound
        );
        assert_eq!(
            serve_guard("assets/index.js.map", false),
            ServeGuard::NotFound
        );
        // And an ordinary asset is unaffected.
        assert_eq!(serve_guard("assets/index.js", false), ServeGuard::Allow);
        // `serve_file`'s SPA fallback is gated on the same predicate — the
        // guard decides candidates, the fallback pushes one after it, so
        // without that second check a local-folder app would answer a `.map`
        // navigation with the shell.
        assert!(is_source_map("assets/index.js.map"));
    }

    #[test]
    fn server_only_path_matches_the_functions_subtree() {
        assert!(is_server_only_path("functions/top-stores.js"));
        // Leading / doubled / `.` segments must not slip past the check.
        assert!(is_server_only_path("/functions/top-stores.js"));
        assert!(is_server_only_path("//functions/top-stores.js"));
        assert!(is_server_only_path("./functions/top-stores.js"));
        // A case-insensitive filesystem (macOS dev box) would resolve this to
        // the real directory, so the guard can't be case-sensitive either.
        assert!(is_server_only_path("Functions/top-stores.js"));
    }

    #[test]
    fn server_only_path_leaves_ordinary_frontend_files_alone() {
        assert!(!is_server_only_path("index.html"));
        assert!(!is_server_only_path("assets/main.js"));
        // Prefix-scoped, not substring: these are frontend files whose names
        // merely start with (or contain) the word.
        assert!(!is_server_only_path("assets/functions.js"));
        assert!(!is_server_only_path("my-functions/x.js"));
        assert!(!is_server_only_path("static/functions/x.js"));
        assert!(!is_server_only_path(""));
    }

    /// The artifact is always `functions/<name>.js`, never the bare segment,
    /// so matching `functions` alone protected nothing and broke real pages:
    /// an app routing `/functions` client-side, or a static export serving its
    /// own `functions.html` / `functions/index.html`, silently got the site
    /// root at 200 — including apps with no Oxy Functions at all.
    #[test]
    fn server_only_path_requires_a_child_segment() {
        assert!(!is_server_only_path("functions"));
        assert!(!is_server_only_path("functions/"));
        assert!(!is_server_only_path("/functions"));
        assert!(!is_server_only_path("functions/."));
        // …while anything actually inside it stays blocked.
        assert!(is_server_only_path("functions/top-stores.js"));
        assert!(is_server_only_path("functions/nested/deep.js"));
    }

    /// The S3 branch's whole path decision, which otherwise needs a database
    /// and a build store to exercise.
    ///
    /// The `functions/` case is the one that regressed: guarding the value
    /// AFTER the directory-index rewrite saw `functions/index.html`, blocked
    /// it, and served the site root — on the branch that serves production,
    /// for a shape the local-dir branch handled correctly.
    #[test]
    fn s3_object_key_guards_the_raw_path_then_rewrites() {
        // A static export's own `/functions/` page resolves to its real file.
        assert_eq!(
            s3_object_key("functions/", true).as_deref(),
            Some("functions/index.html")
        );
        assert_eq!(
            s3_object_key("functions/", false).as_deref(),
            Some("functions/index.html")
        );
        // The artifact underneath is still blocked either way.
        assert_eq!(
            s3_object_key("functions/top-stores.js", true).as_deref(),
            Some("index.html"),
            "navigation falls back to the shell"
        );
        assert_eq!(s3_object_key("functions/top-stores.js", false), None);
        // An explicit request for a file inside the reserved dir is blocked
        // even when the rewrite would have produced the same key — matching
        // the local-dir branch exactly.
        assert_eq!(s3_object_key("functions/index.html", false), None);
    }

    #[test]
    fn s3_object_key_keeps_ordinary_directory_and_file_paths() {
        assert_eq!(s3_object_key("", true).as_deref(), Some("index.html"));
        assert_eq!(s3_object_key("/", true).as_deref(), Some("index.html"));
        assert_eq!(
            s3_object_key("assets/main.js", false).as_deref(),
            Some("assets/main.js")
        );
        assert_eq!(
            s3_object_key("docs/", true).as_deref(),
            Some("docs/index.html")
        );
        assert_eq!(
            s3_object_key("/assets/main.js", false).as_deref(),
            Some("assets/main.js"),
            "leading slash is stripped"
        );
    }

    /// Both serve branches route through this, so it's the one place the
    /// blocked-request policy is stated.
    #[test]
    fn serve_guard_falls_back_to_the_shell_only_for_html_requests() {
        assert_eq!(
            serve_guard("functions/top-stores.js", true),
            ServeGuard::SpaShell
        );
        assert_eq!(
            serve_guard("functions/top-stores.js", false),
            ServeGuard::NotFound
        );
        // A bare `/functions` page — and its directory-style form, which the
        // S3 branch rewrites to `functions/index.html` — are ordinary.
        assert_eq!(serve_guard("functions", true), ServeGuard::Allow);
        assert_eq!(serve_guard("functions", false), ServeGuard::Allow);
        assert_eq!(serve_guard("functions/", true), ServeGuard::Allow);
        assert_eq!(serve_guard("functions/", false), ServeGuard::Allow);
        assert_eq!(serve_guard("index.html", false), ServeGuard::Allow);
    }

    fn test_runtime() -> AppRuntimeConfig {
        AppRuntimeConfig {
            app_id: Uuid::nil(),
            slug: "hello-oxy".to_string(),
            org_id: Uuid::nil(),
            org_slug: "acme".to_string(),
            project_id: Uuid::nil(),
            branch: "main".to_string(),
            api_base_url: String::new(),
            base_path: String::from("/customer-apps/acme/hello-oxy/"),
            service_worker: true,
            analytics: true,
            build_id: String::new(),
        }
    }

    /// A published bundle carries its compiled Oxy Functions next to the
    /// frontend. The asset route used to hand them out to anyone past the
    /// app's auth gate — handler logic, inlined SQL, and the author's original
    /// TypeScript via the sourcemap. Whatever the Accept header says, the
    /// bytes must never come back.
    #[tokio::test]
    async fn serve_file_never_returns_a_compiled_function() {
        // `TempDir` rather than a hand-rolled path: it cleans up on unwind, so
        // a failing assert below doesn't leak a bundle into the temp dir.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("functions")).expect("mkdir");
        std::fs::write(dir.join("index.html"), b"<html><head></head></html>").expect("index");
        std::fs::write(
            dir.join("functions/top-stores.js"),
            b"const MARGIN_RULE='cost*1.42';export default async()=>MARGIN_RULE;",
        )
        .expect("fn");
        // A static export's own page at the same name (`trailingSlash: false`)
        // — must keep working, sharing the directory with the artifact above.
        // The `trailingSlash: true` shape gets its own fixture below, because
        // the `.html` candidate is tried first and would mask it here.
        std::fs::write(dir.join("functions.html"), b"<html>functions page</html>").expect("page");
        let root = dir.canonicalize().expect("canonicalize");
        let runtime = test_runtime();

        // Asset-style fetch (no SPA fallback): a plain 404, same as any path
        // that doesn't exist — the route doesn't confirm which handlers exist.
        let res = serve_file(&root, "functions/top-stores.js", false, &runtime).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Navigation (SPA fallback allowed): the shell, never the handler. An
        // app may legitimately route `/functions` client-side.
        let res = serve_file(&root, "functions/top-stores.js", true, &runtime).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("body");
        let body = String::from_utf8_lossy(&body);
        assert!(
            !body.contains("MARGIN_RULE"),
            "SPA fallback leaked the function body: {body}"
        );
        assert!(
            body.contains("<html>"),
            "expected the SPA shell, got: {body}"
        );

        // The frontend next to it is unaffected.
        let res = serve_file(&root, "index.html", false, &runtime).await;
        assert_eq!(res.status(), StatusCode::OK);

        // The app's OWN `/functions` page still resolves to its real file via
        // the `.html`-suffix candidate — the guard reserves the subtree, not
        // the name. Previously this returned the site root at 200.
        let res = serve_file(&root, "functions", true, &runtime).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(
            String::from_utf8_lossy(&body).contains("functions page"),
            "the app's own /functions page must still be served"
        );
    }

    fn manifest_with_entries() -> AssetManifest {
        AssetManifest {
            schema_version: asset_manifest::SCHEMA_VERSION,
            build_id: "build-7".into(),
            entries: vec![
                asset_manifest::Entry {
                    path: "assets/app.css".into(),
                    kind: asset_manifest::EntryKind::Style,
                },
                asset_manifest::Entry {
                    path: "assets/app.js".into(),
                    kind: asset_manifest::EntryKind::Module,
                },
            ],
            assets: vec!["assets/app.css".into(), "assets/app.js".into()],
            client: asset_manifest::ClientPrefs::default(),
        }
    }

    fn html_headers(rendered: &RenderedHtml, request: &HeaderMap) -> (StatusCode, HeaderMap) {
        let response = finish_html(
            rendered.clone(),
            "text/html; charset=utf-8",
            "private, no-cache",
            "build-7",
            Uuid::nil(),
            request,
        );
        (response.status(), response.headers().clone())
    }

    /// The three headers the first-load story rests on. `Link` is what starts
    /// the entry chunks a round trip early; `x-oxy-build` is how a running
    /// service worker notices a publish without being replaced; `Cache-Tag` is
    /// what would let a CDN purge one app rather than a zone.
    #[test]
    fn html_response_carries_preload_build_and_purge_headers() {
        let runtime = test_runtime();
        let rendered = render_html(
            b"<html><head></head><body></body></html>",
            std::path::Path::new("index.html"),
            &runtime,
            runtime.app_id,
            Some(manifest_with_entries()),
        );
        let (status, headers) = html_headers(&rendered, &HeaderMap::new());

        assert_eq!(status, StatusCode::OK);
        let link = headers
            .get(header::LINK)
            .and_then(|v| v.to_str().ok())
            .expect("Link header");
        assert!(
            link.contains("</customer-apps/acme/hello-oxy/assets/app.css>; rel=preload; as=style"),
            "{link}"
        );
        assert!(
            link.contains("</customer-apps/acme/hello-oxy/assets/app.js>; rel=modulepreload"),
            "{link}"
        );
        assert_eq!(
            headers.get("x-oxy-build").and_then(|v| v.to_str().ok()),
            Some("build-7")
        );
        assert_eq!(
            headers.get("cache-tag").and_then(|v| v.to_str().ok()),
            Some(format!("app-{},build-build-7", Uuid::nil()).as_str())
        );
    }

    /// A build with no manifest — anything published before manifests existed,
    /// and every local-folder source — must serve exactly as it did, minus the
    /// hints. This is the whole backwards-compatibility story.
    #[test]
    fn html_response_omits_the_link_header_without_a_manifest() {
        let runtime = test_runtime();
        let rendered = render_html(
            b"<html><head></head></html>",
            std::path::Path::new("index.html"),
            &runtime,
            runtime.app_id,
            None,
        );
        let (status, headers) = html_headers(&rendered, &HeaderMap::new());
        assert_eq!(status, StatusCode::OK);
        assert!(headers.get(header::LINK).is_none());
        // The rest of the header set is unconditional.
        assert!(headers.get(header::ETAG).is_some());
        assert!(headers.get("x-oxy-build").is_some());
    }

    /// A 304 has no body, so it cannot carry the document's own `<link>` tags —
    /// which is exactly when the preload header matters most. It must survive
    /// the short-circuit alongside the ETag and the cache policy.
    #[test]
    fn a_revalidated_shell_keeps_its_preload_hints() {
        let runtime = test_runtime();
        let rendered = render_html(
            b"<html><head></head></html>",
            std::path::Path::new("index.html"),
            &runtime,
            runtime.app_id,
            Some(manifest_with_entries()),
        );
        let mut request = HeaderMap::new();
        request.insert(
            header::IF_NONE_MATCH,
            header::HeaderValue::from_str(&rendered.etag).expect("etag is a valid header"),
        );

        let (status, headers) = html_headers(&rendered, &request);
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(headers.get(header::LINK).is_some());
        assert_eq!(
            headers.get(header::ETAG).and_then(|v| v.to_str().ok()),
            Some(rendered.etag.as_str())
        );
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("private, no-cache")
        );
    }

    /// The build's manifest is what decides whether the client runtime
    /// registers a worker and instruments the page — the config struct is built
    /// before we know which build is serving, so the override has to happen at
    /// render time or the opt-out silently does nothing.
    #[test]
    fn render_html_applies_the_builds_client_opt_outs() {
        let runtime = test_runtime();
        let mut manifest = manifest_with_entries();
        manifest.client = asset_manifest::ClientPrefs {
            service_worker: false,
            analytics: false,
        };
        let rendered = render_html(
            b"<html><head></head></html>",
            std::path::Path::new("index.html"),
            &runtime,
            runtime.app_id,
            Some(manifest),
        );
        let body = String::from_utf8_lossy(&rendered.body);
        assert!(body.contains("\"serviceWorker\":false"), "{body}");
        assert!(body.contains("\"analytics\":false"), "{body}");

        // …and the default is both on, so an app that says nothing is
        // instrumented. That is the point of the feature.
        let on = render_html(
            b"<html><head></head></html>",
            std::path::Path::new("index.html"),
            &runtime,
            runtime.app_id,
            Some(manifest_with_entries()),
        );
        let body = String::from_utf8_lossy(&on.body);
        assert!(body.contains("\"serviceWorker\":true"));
        assert!(body.contains("\"analytics\":true"));
    }

    /// The `trailingSlash: true` shape, in its own fixture: a bundler emits
    /// `functions.html` or `functions/index.html`, not both, and the candidate
    /// order (`.html` suffix before `<dir>/index.html`) means a fixture
    /// carrying both only ever exercises the first.
    ///
    /// This is the shape that regressed on the S3 branch, where the guard ran
    /// on the rewritten `functions/index.html` instead of the raw
    /// `functions/`. `s3_object_key_guards_the_raw_path_then_rewrites` pins
    /// the S3 half; this pins the local half.
    #[tokio::test]
    async fn serve_file_serves_a_directory_style_functions_page() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("functions")).expect("mkdir");
        std::fs::write(dir.join("index.html"), b"<html>site root</html>").expect("index");
        std::fs::write(
            dir.join("functions/index.html"),
            b"<html>functions dir page</html>",
        )
        .expect("dir page");
        std::fs::write(dir.join("functions/top-stores.js"), b"const SECRET=1;").expect("fn");
        let root = dir.canonicalize().expect("canonicalize");
        let runtime = test_runtime();

        let res = serve_file(&root, "functions/", true, &runtime).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(
            String::from_utf8_lossy(&body).contains("functions dir page"),
            "the app's own /functions/ directory page must still be served"
        );

        // The artifact sharing that directory is still unreachable.
        let res = serve_file(&root, "functions/top-stores.js", false, &runtime).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
