//! Subpath bundle serving for custom apps.
//!
//! Serves custom-app frontends at `GET /customer-apps/{uuid}` and
//! `GET /customer-apps/{uuid}/{*rest}`, gating each request with cookie or
//! bearer auth and an org-membership check.
//!
//! Bundle source per app — see `custom_apps_source::AppSource`:
//! - `LocalFolder { path }` — the dev's `path` IS the bundle dir
//!   (the directory containing `index.html`), so this handler reads
//!   `<path>/<rest_path>` straight off disk. Whatever your bundler
//!   names the output folder (`out/`, `dist/`, `build/`, …), point at
//!   it directly.
//! - `S3` — bundle was synced to
//!   `$OXY_STATE_DIR/customer-apps/<uuid>/out/` by `POST /sync`. The
//!   `out/` segment is owned by the publish pipeline here, not the
//!   dev — CI uploads to `s3://<bucket>/apps/<uuid>/out/`.
//!
//! Authentication failures redirect to `{base}/login?return_to=<url>` rather
//! than 401 so unauthenticated visitors land in the magic-link flow
//! automatically.
//!
//! ## Authentication ordering
//!
//! Auth is checked **before** any DB lookup of the requested app. This
//! prevents an unauthenticated probe from learning whether a UUID is a real
//! registered app (real → 302 redirect; fake → 404 leaks the existence
//! signal). Order is: authenticate → load user → load app → check membership.
//!
//! ## Per-request DB load
//!
//! A Next.js bundle triggers 30–100 asset requests per page load, each
//! routed through this handler, so anything uncached here is multiplied by
//! ~100 on every click. **The warm steady state is now zero DB round-trips
//! per asset.** Each step of the resolution chain is cached in process with
//! a 60-second TTL:
//!
//! | Step | Cache |
//! | ---- | ----- |
//! | user, keyed by `custom_apps_auth::user_cache_key` (id; address only for a provider identity) | `custom_apps_cache::cached_user` |
//! | access check `(user_id, app_id)` | `custom_apps_auth`, dropped by `invalidate_access_cache` |
//! | platform standing | `oxy_server_authz::globals` |
//! | org-by-slug + app-by-`(org_id, slug)` | `custom_apps_cache::cached_app_resolution` |
//! | `app_builds` row (S3 source) | `custom_apps_cache::cached_build`, see `sources.rs` |
//!
//! Asset *bytes* come from the LRU in `custom_apps_bundle_cache`, which also
//! remembers absences, so neither the SPA fallback nor the pre-compressed
//! `.br` probe re-hits the store.
//!
//! **Before adding a step to this chain, cache it.** Uncached, it costs ~100
//! queries per page load on its own — that is exactly how the previous three
//! lookups came to cost 300.

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use entity::prelude::Apps;
use oxy::database::client::establish_connection;
use oxy_auth::authenticator::Authenticator;
use oxy_auth::built_in::BuiltInAuthenticator;
use oxy_auth::user::UserService;
use oxy_shared::fleet_role::{RouteRole, RouteRoleDecl};
use sea_orm::ColumnTrait;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
use sea_orm::QueryFilter;
use tracing::Instrument;
use uuid::Uuid;

use super::custom_apps_auth::user_can_access_app;
use super::custom_apps_cache::{
    ResolvedApp, cached_app_resolution, cached_user, set_cached_app_resolution, set_cached_user,
};
use super::custom_apps_functions::seam::FunctionQueryExecutor;

mod headers;
// The admin SPA's static handler answers the same navigation-vs-subresource
// question for its own fallback, and must answer it identically.
pub(crate) use headers::wants_html;
pub(crate) mod rewrite;
mod sources;

use headers::*;
use sources::*;

pub(crate) use rewrite::first_custom_apps_prefix;
pub(crate) use sources::serve_from_s3_build;

/// Single entry point for `GET /customer-apps/{*path}`. Decides between
/// the legacy uuid form (redirects to the canonical pretty URL) and the
/// new pretty form (`<org_slug>/<app_slug>/<rest>`).
///
/// Auth lives inside [`serve_resolved`] so the legacy-uuid redirect path
/// can still bounce anonymous visitors through `/login?return_to=...`
/// before they ever learn whether a given uuid is a real app.
/// One route, two pods. `serve_dispatch` answers everything under
/// `/customer-apps/{*path}`: bundle bytes from S3, which any replica can serve,
/// and `POST .../fn/<name>`, which EXECUTES an Oxy Function against the working
/// copy. A mount cannot state one role for both, so the module that owns the
/// split states it — the same shape `agentic_http::router_roles()` uses.
pub fn serve_dispatch_roles() -> &'static [RouteRoleDecl] {
    &[
        RouteRoleDecl {
            method: "*",
            path: "/customer-apps/{org}/{app}/fn/{name}",
            role: RouteRole::IdeOnly,
        },
        RouteRoleDecl {
            method: "*",
            path: "/customer-apps/{*path}",
            role: RouteRole::FleetOk,
        },
    ]
}

pub async fn serve_dispatch(Path(path): Path<String>, request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let uri = parts.uri;
    let method = parts.method;

    // Strip the leading slash that axum hands us for a `{*path}` capture
    // when the URL is `/customer-apps/`. Split lazily into the first two
    // segments + the remainder; an empty path means /customer-apps/ which
    // has nothing to dispatch on.
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut path_parts = trimmed.splitn(2, '/');
    let first = path_parts.next().unwrap_or("");
    let rest_after_first = path_parts.next().unwrap_or("");

    // Legacy uuid form: `/customer-apps/<uuid>/<rest>` → 301 to canonical.
    if let Ok(uuid) = first.parse::<Uuid>() {
        return redirect_legacy_uuid(uuid, rest_after_first, &headers, &uri).await;
    }

    // Pretty form: `/customer-apps/<org_slug>/<app_slug>/<rest>`. Need to
    // split `rest_after_first` once more to get the app slug.
    let mut rest_parts = rest_after_first.splitn(2, '/');
    let app_slug = match rest_parts.next() {
        Some(s) if !s.is_empty() => s,
        _ => {
            // We have an org but no app component — 404.
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let rest = rest_parts.next().unwrap_or("").to_string();

    // Oxy Functions route invocation: `POST .../fn/<name>`. Dispatched
    // before `serve_pretty`'s static-bundle logic — see
    // internal-docs/customer-apps-functions.md §11.10.
    if let Some(function_name) = rest.strip_prefix("fn/") {
        if function_name.is_empty() || function_name.contains('/') {
            return StatusCode::NOT_FOUND.into_response();
        }
        let body_bytes =
            match axum::body::to_bytes(body, super::custom_apps_proxy::REQUEST_BODY_LIMIT).await {
                Ok(b) => b,
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            };
        // `?refresh` bypasses the opt-in function result cache (same convention
        // as the /query endpoint).
        let refresh = uri
            .query()
            .map(|q| {
                q.split('&')
                    .any(|kv| kv == "refresh" || kv.starts_with("refresh="))
            })
            .unwrap_or(false);
        // The function query executor is injected at the serve router (an
        // Extension layer) so this runtime never imports `projects::query`.
        // Absent only if the router wiring regressed — fail closed.
        let query_exec = match parts
            .extensions
            .get::<std::sync::Arc<dyn FunctionQueryExecutor>>()
            .cloned()
        {
            Some(exec) => exec,
            None => {
                tracing::error!(
                    "custom-app function invoked but no FunctionQueryExecutor is wired into the serve router"
                );
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        // Layer-1 preagg cache, injected alongside the query executor at the
        // serve router. Absent (default = no cache) in any composition that
        // does not run a rebuild worker, in which case `ctx.semantic` compiles
        // straight to warehouse SQL — the same posture the CLI takes.
        let preagg = parts
            .extensions
            .get::<crate::server::api::middlewares::workspace_context::PreaggCacheCtx>()
            .cloned()
            .unwrap_or_default();
        return super::custom_apps_functions::handle_function_request(
            first,
            app_slug,
            function_name,
            method,
            headers,
            body_bytes,
            refresh,
            query_exec,
            preagg,
        )
        .await;
    }

    serve_pretty(first, app_slug, rest, method, headers, uri, body).await
}

/// The custom-app serve path, phase by phase.
///
/// The HTTP layer already produces one `GET /customer-apps/{*path}` span, and
/// in prod it reports a p95 of 262ms
/// (`internal-docs/2026-09-09-hyperdx-observability-findings.md` §8) with
/// **nothing underneath it** — so the one question that span raises is the one
/// it cannot answer. The phases below are each a child span because each is a
/// different failure and a different fix: an auth round trip, a user lookup, a
/// two-query app resolution, an access check, and the byte fetch itself. The
/// user lookup and the app resolution only produce a span on a cache miss, so
/// their presence in a trace is itself the diagnosis. The status code is not
/// repeated here: the parent HTTP span carries it on every path, early exits
/// included, which a field recorded at the bottom of this function would not.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    target = "custom_apps_serve",
    name = "custom_app_serve",
    skip_all,
    fields(
        org_slug = %org_slug,
        app_slug = %app_slug,
        route = %rest,
        app_id = tracing::field::Empty,
        source = tracing::field::Empty,
    )
)]
pub(crate) async fn serve_pretty(
    org_slug: &str,
    app_slug: &str,
    rest: String,
    method: axum::http::Method,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Body,
) -> Response {
    // Wall clock for the wide event emitted at the bottom. Started before the
    // auth round-trip on purpose: a login check that has gone slow IS the app
    // being slow from the viewer's seat, and a timer that excluded it would
    // report health the viewer does not have.
    let started = std::time::Instant::now();
    // 1. Authenticate FIRST so an unauthenticated probe can't distinguish a
    //    real registered (org, app) pair (302 redirect) from a fake one
    //    (404). No DB work happens before we know the caller has a valid
    //    session.
    let identity = match BuiltInAuthenticator::new()
        .authenticate(&headers)
        .instrument(tracing::info_span!(
            target: "custom_apps_serve",
            "custom_app_authenticate"
        ))
        .await
    {
        Ok(i) => i,
        Err(e) => {
            // Surface auth failures so an operator can tell apart "no cookie"
            // (browser never logged in) vs "stale JWT" vs "wrong domain
            // scope". Without this the only symptom is a silent redirect to
            // login and there's no way to know why from outside the server.
            let cookie_present = headers
                .get(axum::http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("oxy_session="))
                .unwrap_or(false);
            let auth_header_present = headers.get(axum::http::header::AUTHORIZATION).is_some();
            // `info`, not `warn`: an unauthenticated hit is the client doing
            // something ordinary (a service-worker update check, a
            // `version.json` poll, an expired session), and at `warn` it was a
            // third of prod's warnings. The fields keep the diagnosis.
            tracing::info!(
                target: "custom_apps_serve",
                org_slug = %org_slug,
                app_slug = %app_slug,
                cookie_present,
                auth_header_present,
                error = %e,
                "auth failed, redirecting to login"
            );
            return redirect_to_login(&headers, &uri).into_response();
        }
    };

    // User lookup is cached — without this, every asset request triggers a
    // fresh `users` table query. Keyed by who the credential NAMES, not by its
    // email claim: a frontline worker's claim is empty, and an email key put
    // every worker in one slot. See `custom_apps_auth::user_cache_key`.
    let cache_key = super::custom_apps_auth::user_cache_key(&identity);
    let user = if let Some(u) = cached_user(&cache_key) {
        u
    } else {
        match UserService::find_user_by_identity(&identity)
            .instrument(tracing::info_span!(
                target: "custom_apps_serve",
                "custom_app_user_lookup"
            ))
            .await
        {
            Ok(Some(u)) => {
                set_cached_user(cache_key, u.clone());
                u
            }
            Ok(None) => return redirect_to_login(&headers, &uri).into_response(),
            Err(e) => {
                tracing::error!("Failed to load user for custom app {org_slug}/{app_slug}: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    // 2. Now that the caller is known, look up the org + app and check
    // membership. Joining by (org_slug, app_slug) is the only DB hit other
    // than the membership cache miss path; both indexed.
    let db = match establish_connection().await {
        Ok(db) => db,
        Err(e) => {
            tracing::error!("DB connection failed serving custom app {org_slug}/{app_slug}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Turning the URL's two slugs into rows costs two indexed queries, and
    // every asset in the page repeats them for the same pair. Cached together
    // for 60s and dropped wholesale by `invalidate_app_resolution_cache` on
    // any app mutation, so a publish or a visibility change still lands
    // immediately. A miss is not cached — a newly-created app is reachable at
    // once.
    let resolved = match cached_app_resolution(org_slug, app_slug) {
        Some(r) => r,
        None => {
            let org = match entity::prelude::Organizations::find()
                .filter(entity::organizations::Column::Slug.eq(org_slug))
                .one(&db)
                .instrument(tracing::info_span!(
                    target: "custom_apps_serve",
                    "custom_app_resolve_org"
                ))
                .await
            {
                Ok(Some(o)) => o,
                Ok(None) => return no_store_404(),
                Err(e) => {
                    tracing::error!("Failed to look up org {org_slug}: {e}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            let app = match Apps::find()
                .filter(entity::apps::Column::OrgId.eq(org.id))
                .filter(entity::apps::Column::Slug.eq(app_slug))
                .one(&db)
                .instrument(tracing::info_span!(
                    target: "custom_apps_serve",
                    "custom_app_resolve_app"
                ))
                .await
            {
                Ok(Some(a)) => a,
                Ok(None) => return no_store_404(),
                Err(e) => {
                    tracing::error!("Failed to look up custom app {org_slug}/{app_slug}: {e}");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };

            let resolved = ResolvedApp { org, app };
            set_cached_app_resolution(org_slug, app_slug, resolved.clone());
            resolved
        }
    };
    let ResolvedApp { org, app } = resolved;
    let id = app.id;

    // Combined access check (org member | workspace grant | global app
    // admin). Cached per (user_id, app_id) for 60s — see
    // `custom_apps_auth::user_can_access_app`. Critical for the Next.js
    // asset storm (30-100 requests per page load).
    let allowed = match user_can_access_app(&db, user.id, user.email.as_deref().unwrap_or(""), &app)
        .instrument(tracing::info_span!(
            target: "custom_apps_serve",
            "custom_app_authorize"
        ))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Access check failed for custom app {id}: {e}");
            record_early_exit(
                &app,
                user.id,
                &headers,
                &rest,
                is_html_navigation(&rest),
                StatusCode::INTERNAL_SERVER_ERROR,
                started,
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !allowed {
        // A 403 is not an app fault and does not dent the SLI (see
        // `is_app_fault`), but it is still traffic this app served and an
        // operator asking "who is being turned away" has nowhere else to look.
        record_early_exit(
            &app,
            user.id,
            &headers,
            &rest,
            is_html_navigation(&rest),
            StatusCode::FORBIDDEN,
            started,
        );
        return (
            StatusCode::FORBIDDEN,
            "You don't have access to this custom app.",
        )
            .into_response();
    }

    // 3. Dispatch through the source facade. The per-app source decision
    //    (v0 / local / s3) is recorded at register time; this handler only
    //    knows about three rendering modes and matches on them.
    let source = match super::custom_apps_source::AppSource::from_model(&app) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Bad source config for custom app {id}: {e}");
            // A 500 the PLATFORM produced. Returning here skipped the recorder
            // at the bottom, so the SLI built to catch platform failure was
            // blind to exactly that.
            record_early_exit(
                &app,
                user.id,
                &headers,
                &rest,
                is_html_navigation(&rest),
                StatusCode::INTERNAL_SERVER_ERROR,
                started,
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let runtime = AppRuntimeConfig::from_app(&app, &org.slug);

    // 3a. The platform-reserved `__oxy/` namespace, answered by us rather than
    //     by the bundle. Deliberately placed HERE — after authentication and
    //     the access check, before the source dispatch — so it inherits the
    //     app's exact gate: the same viewers who can load the bundle can
    //     register its worker and post its telemetry, and nobody else can. It
    //     also means these paths work identically on the subpath and subdomain
    //     surfaces with no CORS involved, because they *are* the app's origin.
    //
    //     Not everything under the prefix is answered here — the asset manifest
    //     is a real object inside the build — but everything under it is
    //     *classified* here, so a name nobody claims 404s rather than reaching
    //     the bundle. That is what makes the namespace reserved rather than
    //     merely conventional.
    if let Some(name) = reserved_platform_path(&rest) {
        match classify_reserved(name, &method) {
            Reserved::ServiceWorker => return service_worker_for(&runtime, &headers),
            Reserved::WebManifest => {
                return web_manifest_for(&db, &runtime, &app, &headers).await;
            }
            Reserved::MonogramIcon => return monogram_icon_for(&app),
            Reserved::Beacon => {
                return ingest_beacon(
                    body,
                    &headers,
                    &db,
                    app.id,
                    app.org_id,
                    user.id,
                    user.email.as_deref().unwrap_or(""),
                )
                .await;
            }
            // A real object inside the build, written at publish — fall through
            // to the ordinary dispatch so it rides the same LRU,
            // pre-compression, and absence caching as every other asset.
            Reserved::BuildObject => {}
            Reserved::Unknown => return super::custom_apps_client::reserved_not_found(),
        }
    }

    // Detect the "user-navigation" requests (root path, trailing-slash
    // directory, or `.html`) so we record at most one view event per
    // user-visible page load — not once per asset / API fetch which
    // would 100× the volume with zero extra signal.
    //
    // Computed BEFORE the dispatch so the per-source-type response
    // build doesn't need to know about tracking. Tracking + cookie
    // injection happen in the post-dispatch wrapper at the bottom.
    let is_html_request = is_html_navigation(&rest);
    let source_label = if on_custom_app_subdomain(&headers) {
        "subdomain"
    } else {
        "subpath"
    };

    use super::custom_apps_source::AppSource;
    // The app id and the backend that answered belong on the parent span:
    // "which app, served from where" is how an operator slices a latency or
    // error question before looking at any single request.
    {
        let span = tracing::Span::current();
        span.record("app_id", tracing::field::display(id));
        span.record(
            "source",
            match &source {
                AppSource::V0 { .. } => "v0_proxy",
                AppSource::LocalFolder { .. } => "local_folder",
                AppSource::S3 => "s3",
            },
        );
    }
    let dispatch_span = tracing::info_span!(
        target: "custom_apps_serve",
        "custom_app_dispatch"
    );
    let response = async {
        match source {
            AppSource::V0 { url } => {
                super::custom_apps_proxy::proxy(
                    &url,
                    &rest,
                    method,
                    &uri,
                    &headers,
                    body,
                    super::custom_apps_proxy::ProxyIdentity {
                        app: &app,
                        org: &org,
                        user_id: user.id,
                        user_email: user.email.as_deref().unwrap_or(""),
                    },
                )
                .await
            }
            AppSource::LocalFolder { path } => {
                // LocalFolder has no draft/published split — one directory
                // serves everyone. Publishing for these sources is purely a
                // sidebar visibility toggle.
                serve_from_local(id, &path, &rest, &headers, &runtime).await
            }
            AppSource::S3 => {
                // The customer URL accepts no view modifier. Draft mode
                // lives on a staff-only HttpOnly cookie set via
                // `POST /api/customer-apps/preview-draft`. Customer's
                // browser never carries this cookie; even if a customer
                // forged it, `is_app_admin_email` denies them draft
                // access below.
                let cookie_wants_draft = super::custom_apps_preview::wants_draft_preview(&headers);
                // Fail-closed inside the one reader: a lookup error reports no standing.
                // Admin OR owner — both operator tiers reach every custom-app surface.
                // Scoped to THIS app's org: a grant bounded elsewhere must not unlock the
                // draft channel here. `is_staff()` would, and is now true for every role.
                let is_staff = oxy_server_authz::globals::platform_reaches(
                    &db,
                    user.email.as_deref().unwrap_or(""),
                    oxy_authz::Cap::DevelopApps,
                    app.org_id,
                )
                .await;
                let channel =
                    resolve_channel(cookie_wants_draft && is_staff, app.published_at.is_some());
                // New publish pipeline: when the channel has a build pointer,
                // serve straight from S3 (no local state dir). Legacy `s3`
                // rows leave both pointers NULL and fall through to the
                // state-dir path until they're re-published.
                use super::custom_apps_sync::Channel;
                let build_pk = match channel {
                    Channel::Draft => app.draft_build_id,
                    Channel::Published => app.published_build_id,
                };
                match build_pk {
                    Some(build_pk) => {
                        // The runtime config was built ~100 lines above, before the
                        // channel was known, so its `buildId` defaulted to the
                        // PUBLISHED build. A staff draft preview of a published app
                        // would then serve draft bytes and inject the published id:
                        // an error thrown by draft code gets attributed to the wrong
                        // build, `resolve_stack` fetches that build's maps, and the
                        // lookups return plausible-but-wrong files and lines —
                        // reported as `stack_resolved: true`, which is the one case
                        // that flag exists to prevent. Draft preview is exactly when
                        // someone is reading a stack.
                        let mut runtime = runtime;
                        runtime.build_id = build_pk.to_string();
                        serve_from_s3_build(&db, id, build_pk, &rest, &runtime, &headers).await
                    }
                    None => {
                        // Post-retirement: the legacy state-dir serve is gone.
                        // An s3-source app with no build pointer hasn't been
                        // published through the new pipeline yet (`oxyc publish`).
                        tracing::warn!(
                            "app {id}: no build for {channel:?} channel — not yet published via `oxyc publish`"
                        );
                        no_store_404()
                    }
                }
            }
        }
    }
    .instrument(dispatch_span)
    .await;

    // Post-dispatch: for browser-navigation requests only (root /
    // trailing-slash / `.html`), stamp the session cookie on the
    // response and spawn the view-event recording. Asset / API
    // fetches are storm-volume — exclude them so the Activity tab
    // counts user-visible page loads, not request volume.
    //
    // The invariant: a non-2xx is not a view, whoever produced it. The
    // dispatch spans three backends (build store, local dir, upstream
    // proxy) and each has its own ladder of 404s, 500s and 403s; the gate
    // deliberately does not care which one answered.
    //
    // `304` is the one exception — the browser re-rendered the page from
    // its cache, which is a view. `html_response`'s `if_none_match` and an
    // upstream conditional response both produce one.
    //
    // Two consequences worth knowing, because they cut opposite ways:
    //
    //   - **Gained.** An app that is registered but has nothing to serve —
    //     no `oxyc publish` yet, a `LocalFolder` path pointing nowhere, an
    //     S3 source not yet synced — stops recording a view for every hit
    //     on its 404. That is a state customers sit in, so it was real
    //     inflation.
    //   - **Lost.** A proxied app's *own* rendered error page — a Next.js
    //     `404.tsx`, a maintenance `503` — no longer counts, though a
    //     human did look at it. `AppSource::V0` passes the upstream status
    //     through verbatim, so this route cannot tell "the app rendered
    //     its own 404" from "the app is broken." Undercounting those beats
    //     counting every hit on an app that renders nothing.
    //   - **Lost.** A proxied app's redirect off its front door. Note this
    //     is a loss, not a de-duplication: `is_html_navigation` is true
    //     only for root, trailing-slash and `.html`, so a `middleware.ts`
    //     or i18n bounce from `/` to `/en` loses the `/` view and never
    //     gains one for `/en` — the entry point every visitor arrives
    //     through drops to zero. It de-duplicates only when the target is
    //     itself tracked (`/foo/` → `/bar/`), which is the narrower half
    //     of the redirect space. Accepted because a 3xx isn't a page load;
    //     if the count matters, the fix is to track a 3xx whose `Location`
    //     resolves inside the same app rather than to widen this gate.
    //
    // This gates the `Set-Cookie` too, so a visitor whose first hit is a
    // non-2xx starts a fresh session on their next navigation. Harmless —
    // the tracking cookie is analytics-only and separate from
    // `oxy_session`.
    let rendered = {
        let s = response.status();
        s.is_success() || s == StatusCode::NOT_MODIFIED
    };

    // One wide event per served request — every request, not just the HTML
    // ones the view recorder below counts. This is the denominator the
    // availability SLI is computed from, and an SLI that only saw successful
    // page loads would be a tautology. Assets are recorded and then excluded
    // from the ratio at query time (see `availability_sql`), so an asset
    // failure is diagnosable without being able to drown out a shell failure.
    //
    // Speculative prefetches are excluded for the same reason they are not
    // views: the HQ launcher warms an app on card hover, and counting those
    // would make hover traffic indistinguishable from real load.
    if !is_speculative_request(&headers) {
        super::custom_apps_telemetry::record_serve(super::custom_apps_telemetry::ServeEvent {
            org_id: app.org_id,
            app_id: app.id,
            build_id: None,
            request_id: crate::server::api::middlewares::request_id::request_id_from_headers(
                &headers,
            ),
            session_id: None,
            user_id: user.id,
            is_html: is_html_request,
            route: &rest,
            status: response.status().as_u16(),
            duration_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
        });
    }
    // A browser-initiated speculation is not a view. The HQ launcher warms an
    // app on card hover so the click is instant (`prefetchApp`), and without
    // this every hover would record an open — and mint a session id at hover
    // time that the real navigation would then inherit. See
    // `is_speculative_request`.
    if is_html_request && rendered && !is_speculative_request(&headers) {
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        let secure = uri.scheme_str() == Some("https")
            || headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.eq_ignore_ascii_case("https"))
                .unwrap_or(false);
        let (session_id, set_cookie) =
            super::custom_apps_tracking::session_id_for_serve(&headers, secure);
        let referrer = headers
            .get(axum::http::header::REFERER)
            .and_then(|v| v.to_str().ok());
        let sanitized_referrer = super::custom_apps_tracking::sanitize_referrer(referrer, host);
        let user_agent_class = classify_user_agent(headers.get(axum::http::header::USER_AGENT));
        // The recorder needs the app row to resolve the viewer's role snapshot.
        // Hand it the one already resolved (and process-cached) above rather than
        // making it refetch by id — the read is free here and isn't there.
        let recorded_app = app.clone();
        let user_id = user.id;
        let user_email = user.email.clone();
        let source_label = source_label.to_string();
        // Fire-and-forget; a slow DB insert must not stall the HTML
        // response. Losing a row on crash is the documented acceptable
        // failure mode for tracking-grade data.
        tokio::spawn(async move {
            super::custom_apps_tracking::record_view(
                recorded_app,
                user_id,
                user_email,
                session_id,
                sanitized_referrer,
                user_agent_class,
                source_label,
            )
            .await;
        });

        let mut resp = response;
        if let Ok(hv) = axum::http::HeaderValue::from_str(&set_cookie) {
            resp.headers_mut().append(header::SET_COOKIE, hv);
        }
        return resp;
    }

    response
}

/// Emit a serve event for a request that fails BEFORE reaching the normal
/// recording point at the bottom of `serve_pretty`.
///
/// The two 500s the platform itself produces — an access-check DB failure and a
/// bad source config — returned early, so the SLI that exists to catch platform
/// failure never saw the platform's own failures. Split out rather than inlined
/// three times so the `kind`/`outcome` classification stays in one place.
fn record_early_exit(
    app: &entity::apps::Model,
    user_id: Uuid,
    headers: &HeaderMap,
    rest: &str,
    is_html: bool,
    status: StatusCode,
    started: std::time::Instant,
) {
    super::custom_apps_telemetry::record_serve(super::custom_apps_telemetry::ServeEvent {
        org_id: app.org_id,
        app_id: app.id,
        build_id: None,
        request_id: oxy_shared::utils::request_id::from_headers(headers),
        session_id: None,
        user_id,
        is_html,
        route: rest,
        status: status.as_u16(),
        duration_ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
    });
}

/// The tail of a request path inside the platform-reserved namespace, or `None`
/// when the request is for the app's own files.
///
/// Case-insensitive on the prefix for the same reason `is_server_only_path` is:
/// a case-insensitive filesystem would otherwise resolve `__OXY/sw.js` to a real
/// object on one platform and not another.
fn reserved_platform_path(rest: &str) -> Option<&str> {
    let trimmed = rest.trim_start_matches('/');
    let prefix = super::custom_apps_asset_manifest::RESERVED_PLATFORM_PREFIX;
    if trimmed.len() < prefix.len() {
        return None;
    }
    let (head, tail) = trimmed.split_at(prefix.len());
    head.eq_ignore_ascii_case(prefix).then_some(tail)
}

/// What a request inside the platform-reserved `__oxy/` namespace is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reserved {
    /// `GET __oxy/sw.js` — the platform service worker.
    ServiceWorker,
    /// `POST __oxy/beacon` — the client runtime's telemetry batch.
    Beacon,
    /// `GET __oxy/manifest.webmanifest` — the synthesised web app manifest.
    WebManifest,
    /// `GET __oxy/icon.svg` — the platform monogram, the icon every app has
    /// whether or not its author declared one.
    MonogramIcon,
    /// A platform object that lives *inside the build* rather than being
    /// synthesised here — today only the asset manifest. Falls through to the
    /// ordinary source dispatch.
    BuildObject,
    /// Anything else. 404, never a fall-through to the bundle: reserving the
    /// prefix is what stops an app serving its own file at a platform URL, and
    /// a miss that fell through would give one back.
    Unknown,
}

/// Classify one reserved-namespace request. Pure, so the routing table is
/// testable without a database, a build store, or a request body — which is the
/// whole reason it is separated from the dispatch that acts on it.
///
/// Method is part of the decision rather than checked afterwards: `POST
/// __oxy/sw.js` is not "the worker, wrong method", it is a request for something
/// that does not exist, and answering it the same way as any other unknown
/// reserved path keeps the surface from advertising which verbs it takes.
fn classify_reserved(name: &str, method: &axum::http::Method) -> Reserved {
    let prefix = super::custom_apps_asset_manifest::RESERVED_PLATFORM_PREFIX;
    let sw = super::custom_apps_client::SERVICE_WORKER_PATH.trim_start_matches(prefix);
    let beacon = super::custom_apps_client::BEACON_PATH.trim_start_matches(prefix);
    let manifest =
        super::custom_apps_asset_manifest::ASSET_MANIFEST_PATH.trim_start_matches(prefix);
    let webmanifest = super::custom_apps_client::WEB_MANIFEST_PATH.trim_start_matches(prefix);
    let monogram = super::custom_apps_client::MONOGRAM_ICON_PATH.trim_start_matches(prefix);

    match (name, method) {
        (n, &axum::http::Method::GET) if n == sw => Reserved::ServiceWorker,
        (n, &axum::http::Method::POST) if n == beacon => Reserved::Beacon,
        (n, &axum::http::Method::GET) if n == webmanifest => Reserved::WebManifest,
        (n, &axum::http::Method::GET) if n == monogram => Reserved::MonogramIcon,
        (n, &axum::http::Method::GET) if n == manifest => Reserved::BuildObject,
        _ => Reserved::Unknown,
    }
}

/// Serve the platform worker, scoped as wide as this surface needs and no wider.
///
/// On a **custom-app subdomain** the whole origin is one app and the page sits
/// at `/`, while the bundle's assets keep the `/customer-apps/<org>/<app>/`
/// prefix (the host dispatcher passes those through rather than double-prefixing
/// them). A worker scoped to that prefix would never control the page, so the
/// origin has to permit `/`.
///
/// On the **admin host** it must not: `/` there is the Oxy console, and a custom
/// app's worker claiming it would intercept the whole SPA. `runtime.js` computes
/// the same answer client-side from the document's own path; this header is the
/// half that holds if it ever gets it wrong.
fn service_worker_for(runtime: &AppRuntimeConfig, headers: &HeaderMap) -> Response {
    let allowed = if on_custom_app_subdomain(headers) {
        "/"
    } else {
        &runtime.base_path
    };
    super::custom_apps_client::service_worker_response(allowed)
}

/// The `icons[]` for one app: its own mark when it has a usable one, and the
/// platform monogram always.
///
/// Extracted so `web_manifest_for` stays inside the ~60-line guidance, and
/// because "which icons does this app advertise" is a question worth being able
/// to answer in one place.
async fn manifest_icons_for(
    db: &DatabaseConnection,
    app: &entity::apps::Model,
    base_path: &str,
) -> Vec<super::custom_apps_client::ManifestIcon> {
    let mut icons: Vec<super::custom_apps_client::ManifestIcon> = Vec::new();

    // `resolve_channel` rather than a hardcoded `Published`: an app that has
    // never published resolves `NotFound` on the published channel and loses its
    // mark entirely, which is the case that actually bites.
    //
    // The staff-draft-preview half is deliberately NOT honoured here. It would
    // cost a `platform_reaches` round trip on this route, and the response is
    // `max-age=300` — so a staff member previewing a draft icon change would see
    // a stale manifest for up to five minutes regardless. The authz call buys
    // nothing the cache does not immediately undo.
    let channel = resolve_channel(false, app.published_at.is_some());

    // A manifest that fails to resolve costs the app its branding on the home
    // screen, never its installability — but it is logged, because "why is my
    // app's icon not on the home screen" is otherwise unanswerable from logs.
    match super::custom_apps_manifest::resolve_manifest(db, app, channel).await {
        Ok(m) => {
            if let Some(icon) = m.icon.as_deref()
                && super::workspace_custom_apps::safe_relative_art_path(icon)
                && let Some(mime) = super::custom_apps_client::icon_mime(icon)
            {
                icons.push(super::custom_apps_client::ManifestIcon {
                    src: format!("{base_path}{icon}"),
                    mime,
                    // Only claim a size for a scalable format. We never decode an
                    // author's raster, so any pixel count here would be invented
                    // — and a browser that trusts an invented size and finds
                    // something smaller drops the icon entirely.
                    //
                    // The consequence, worth naming: Blink scores candidates by
                    // declared `sizes`, so an entry with none rarely wins against
                    // the monogram below. Author artwork is effectively
                    // decorative until publish records real dimensions.
                    sizes: (mime == "image/svg+xml").then_some("any"),
                    // Not `maskable`: Android crops ~20% off each edge, and this
                    // is artwork whose safe zone we have never seen.
                    purpose: "any",
                });
            }
        }
        Err(e) => {
            tracing::warn!(
                app_id = %app.id,
                error = %e,
                "could not resolve app manifest for web-manifest icons — falling back to the monogram"
            );
        }
    }

    icons.push(super::custom_apps_client::ManifestIcon {
        src: format!(
            "{base_path}{}",
            super::custom_apps_client::MONOGRAM_ICON_PATH
        ),
        mime: "image/svg+xml",
        // A MEASURED claim, unlike the author's raster above: `monogram_svg`
        // emits `viewBox="0 0 512 512" width="512" height="512"`, so this is our
        // own artwork's real size. It matters because a bare `any` on an SVG is
        // exactly what Chrome's installability check has historically been
        // unreliable about — and an unusable fallback icon makes the whole
        // fallback a silent no-op for the apps it exists for.
        sizes: Some("512x512 any"),
        // The platform drew this one, inside the maskable safe zone.
        purpose: "any maskable",
    });

    icons
}

/// Serve the synthesised web app manifest — what makes an app installable.
///
/// **The scope must match the service worker's**, and computing it the same way
/// from the same input is how that is guaranteed rather than remembered. On a
/// custom-app subdomain the page sits at `/` while the bundle's assets keep the
/// `/customer-apps/<org>/<app>/` prefix; on the admin host both are under the
/// base path. A manifest whose `scope` does not cover the page is not an error
/// any browser reports — it just silently declines to install, which is the
/// worst failure available to a feature whose entire purpose is installation.
///
/// Icons resolve against the BASE path in both cases, because that is where the
/// bundle's files actually live on either surface.
///
/// The app's own icon is optional and author-declared, so it cannot be the only
/// entry: a browser will not offer installation without an icon it can fetch,
/// and most apps ship none (which is why the launcher has a monogram fallback
/// at all). The platform monogram is therefore always present as the last
/// entry, and the author's mark — resolved through the same manifest and the
/// same sanitiser every other surface uses, per `oxy-app-visual-identity` —
/// goes first when there is one.
///
/// Reading the manifest costs a query only when the app has no
/// `manifest_override`, and this route is hit on install and update checks
/// rather than on navigation, so it is not on the path `oxy-customer-apps-perf`
/// is about.
async fn web_manifest_for(
    db: &DatabaseConnection,
    runtime: &AppRuntimeConfig,
    app: &entity::apps::Model,
    headers: &HeaderMap,
) -> Response {
    let page_scope = if on_custom_app_subdomain(headers) {
        "/"
    } else {
        &runtime.base_path
    };

    let icons = manifest_icons_for(db, app, &runtime.base_path).await;

    let body = super::custom_apps_client::web_manifest_json(
        &app.name,
        &short_name_for(&app.name),
        app.id,
        page_scope,
        &icons,
    );

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/manifest+json",
            ),
            // Short, not immutable: the manifest is derived from the app's name
            // and the surface it was requested on, and a rename that took a
            // year to reach installed devices would be worse than a re-fetch.
            //
            // `private`, not `public`, for the reason `cache_control_for`
            // already gives every other object in this namespace: it sits
            // behind the app's auth gate and names the app, so a shared cache
            // holding it could hand it to a caller who never passed that gate.
            (axum::http::header::CACHE_CONTROL, "private, max-age=300"),
        ],
        body,
    )
        .into_response()
}

/// A home-screen label, which has room for far less than a name.
///
/// Truncated on a word boundary rather than swapped for the slug: the slug is
/// not reliably shorter (`"Store Operations"` and `"store-operations"` are the
/// same length), so falling back to it traded the capitals for nothing.
fn short_name_for(name: &str) -> String {
    const MAX: usize = 12;
    if name.chars().count() <= MAX {
        return name.to_string();
    }
    let mut out = String::new();
    for word in name.split_whitespace() {
        let sep = usize::from(!out.is_empty());
        if out.chars().count() + sep + word.chars().count() > MAX {
            break;
        }
        if sep == 1 {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        // One long word, so there is no boundary to cut on.
        out = name.chars().take(MAX).collect();
    }
    out
}

/// Serve the platform monogram — the icon every app is guaranteed to have.
///
/// Under the reserved namespace and behind the app's own gate, like everything
/// else here, so it needs no separate authorization story.
fn monogram_icon_for(app: &entity::apps::Model) -> Response {
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            // Same reasoning as the manifest it is named from: derived from the
            // app's name, behind the app's gate, so short and private.
            (axum::http::header::CACHE_CONTROL, "private, max-age=300"),
        ],
        super::custom_apps_client::monogram_svg(&app.name),
    )
        .into_response()
}

/// Read a telemetry batch and hand it to the beacon.
///
/// The caller has already authenticated the request and confirmed this viewer
/// may open the app — the beacon deliberately makes no second authorization
/// decision, because it is dispatched from inside the gate the bundle's own
/// bytes pass through.
async fn ingest_beacon(
    body: axum::body::Body,
    headers: &HeaderMap,
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    org_id: Uuid,
    user_id: Uuid,
    user_email: &str,
) -> Response {
    let bytes = match axum::body::to_bytes(body, super::custom_apps_beacon::MAX_BODY_BYTES).await {
        Ok(b) => b,
        // `to_bytes` rejects an over-limit body before the size check inside
        // `admit` ever sees it; that one is the second line, not the first.
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    super::custom_apps_beacon::handle(
        db.clone(),
        app_id,
        org_id,
        user_id,
        user_email.to_string(),
        headers,
        bytes,
    )
    .await
}

/// Did this request arrive on a `<org>--<app>.customer-apps…` host?
///
/// One reader for a fact two places need — the recorded view's `source` label
/// and the service worker's permitted scope. They must not be able to disagree:
/// the label is cosmetic, but the scope decides whether a custom app's worker
/// may claim the admin origin.
fn on_custom_app_subdomain(headers: &HeaderMap) -> bool {
    oxy_app_core::custom_apps_host_dispatch::parse_subdomain(
        headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )
    .is_some()
}

#[cfg(test)]
mod reserved_tests {
    use super::*;

    /// The monogram must be classified, not fall through to the bundle — the
    /// whole point of the reserved namespace.
    #[test]
    fn monogram_is_a_classified_reserved_path() {
        assert!(matches!(
            classify_reserved("icon.svg", &axum::http::Method::GET),
            Reserved::MonogramIcon
        ));
        assert!(matches!(
            classify_reserved("icon.svg", &axum::http::Method::POST),
            Reserved::Unknown
        ));
    }

    /// The fallback exists to fit a home-screen label. Falling back to the slug
    /// did not: `"Store Operations"` and `"store-operations"` are both 16.
    #[test]
    fn short_name_actually_shortens() {
        assert_eq!(short_name_for("Store Ops"), "Store Ops");
        assert_eq!(short_name_for("Store Operations"), "Store");
        assert_eq!(short_name_for("Poke House Store Ops"), "Poke House");
        // One long word has no boundary to cut on, so cut anyway rather than
        // returning something that overflows.
        assert_eq!(short_name_for("Supercalifragilistic").chars().count(), 12);
        for name in [
            "Store Operations",
            "Poke House Store Ops",
            "Supercalifragilistic",
        ] {
            assert!(
                short_name_for(name).chars().count() <= 12,
                "{name} produced a label that is still too long"
            );
        }
    }

    #[test]
    fn recognises_the_reserved_namespace_and_nothing_adjacent() {
        assert_eq!(reserved_platform_path("__oxy/sw.js"), Some("sw.js"));
        assert_eq!(reserved_platform_path("/__oxy/beacon"), Some("beacon"));
        // Case-insensitive prefix, case-preserving tail.
        assert_eq!(reserved_platform_path("__OXY/sw.js"), Some("sw.js"));
        assert_eq!(reserved_platform_path("__oxy/"), Some(""));

        // An app's own files must never be captured by the prefix.
        assert_eq!(reserved_platform_path("assets/__oxy/x.js"), None);
        assert_eq!(reserved_platform_path("__oxygen/x.js"), None);
        assert_eq!(reserved_platform_path("index.html"), None);
        assert_eq!(reserved_platform_path(""), None);
        // Shorter than the prefix — the split must not panic.
        assert_eq!(reserved_platform_path("__ox"), None);
    }

    use axum::http::Method;

    #[test]
    fn classifies_the_platform_endpoints_by_name_and_method() {
        assert_eq!(
            classify_reserved("sw.js", &Method::GET),
            Reserved::ServiceWorker
        );
        assert_eq!(classify_reserved("beacon", &Method::POST), Reserved::Beacon);
        assert_eq!(
            classify_reserved("asset-manifest.json", &Method::GET),
            Reserved::BuildObject
        );
    }

    /// The manifest is written into the build prefix at publish, so it must
    /// reach the ordinary dispatch rather than being answered here. Getting this
    /// wrong 404s the document the service worker's whole precache depends on —
    /// and does it silently, because a worker with no manifest simply installs
    /// and caches nothing.
    #[test]
    fn the_asset_manifest_falls_through_to_the_build() {
        assert_eq!(
            classify_reserved("asset-manifest.json", &Method::GET),
            Reserved::BuildObject,
            "the manifest is an object in the build, not a synthesised response"
        );
    }

    /// A wrong method is an unknown resource, not a 405: answering differently
    /// would advertise which verbs each platform path takes.
    #[test]
    fn a_wrong_method_or_unknown_name_is_simply_not_found() {
        assert_eq!(classify_reserved("sw.js", &Method::POST), Reserved::Unknown);
        assert_eq!(classify_reserved("beacon", &Method::GET), Reserved::Unknown);
        assert_eq!(classify_reserved("", &Method::GET), Reserved::Unknown);
        assert_eq!(
            classify_reserved("not-a-thing", &Method::GET),
            Reserved::Unknown
        );
        // An app's own file under the prefix cannot be reached either — publish
        // strips them, and this is the serve-side half of the same rule.
        assert_eq!(
            classify_reserved("secrets.json", &Method::GET),
            Reserved::Unknown
        );
    }
}

/// Test-only view of the origin's cache policy, so
/// `custom_apps_asset_manifest` can assert that the set of paths its service
/// worker serves cache-first is exactly the set this route calls `immutable`.
/// Two lists, one rule — and a divergence pins a stale chunk with no
/// server-side remedy, which is worth a test that reaches across modules.
#[cfg(test)]
pub(crate) fn cache_control_for_test_only(
    request_path: &str,
    file_path: &std::path::Path,
) -> &'static str {
    headers::cache_control_for(request_path, file_path)
}
