//! `GET /api/customer-apps/{org}/{app}/health` — external liveness for one
//! published custom app.
//!
//! ## Why this exists, and why it is not the bare `/health` you wanted
//!
//! A custom-app subdomain answers **200 with the SPA shell for every path and
//! every hostname**, including hostnames with no app behind them. Measured on
//! prod: `poke-house-staging--command-center…/`, a made-up
//! `nonexistent-canary-xyz…/`, and `app.oxygen-hq.com/` all return the same
//! 6137-byte HTML, and `/health`, `/live`, `/ready`, `/readyz` return it too.
//!
//! Two consequences shape this module:
//!
//! 1. **A monitor pointed at a bare `/health` on the subdomain would be green
//!    forever**, whatever the app's real state, because the fall-through shell
//!    is indistinguishable from a healthy answer — there is no string in it that
//!    a passing response could own. So this endpoint lives under `/api/*`, the
//!    only prefix `custom_apps_host_dispatch::subdomain_rewrite_middleware`
//!    leaves unrewritten, and returns JSON.
//! 2. **The body must be unambiguous under substring matching.** The verdict key
//!    is `oxy_app_health`, which cannot occur in the SPA shell, and its values
//!    are `pass` / `fail` — deliberately *not* `healthy` / `unhealthy`, where a
//!    monitor configured with `Contains "healthy"` silently matches the failure
//!    body. Neither value is a substring of the other, so even a sloppy
//!    contains-check is correct here.
//!
//! ## The contract a monitor should be configured against
//!
//! **`200` means pass. Every other status means fail** — including `401`, `403`,
//! `404` and `500`, which are auth/lookup outcomes rather than app verdicts.
//! Every one of those bodies also carries `"oxy_app_health":"fail"`, so a
//! body-matching monitor and a status-matching monitor agree on all paths.
//!
//! Do **not** configure a monitor to expect only `200`-or-`503`. That was the
//! first draft of this doc and it was wrong: the auth helper can answer before a
//! verdict is ever computed.
//!
//! ## Auth: a token, always
//!
//! Every request authenticates ([`authenticate_and_authorize`] — session cookie
//! or bearer token) *before* the app is looked up, so an unauthenticated caller
//! gets `401` whether or not the app exists. That ordering is the point: today
//! every subdomain returns an identical 200, so nothing distinguishes a real app
//! from a typo, and a health endpoint necessarily could. Requiring the token
//! first means this ships without adding an anonymous enumeration oracle.
//!
//! Two limits worth stating plainly rather than implying:
//!
//! - **Authenticated callers can still distinguish existence across orgs.** With
//!   any valid token, `403` (the app exists, you may not see it) and `404` (no
//!   such org/app) are different answers. That is inherited from the shared gate
//!   — `/debug` behaves the same — not introduced here, but the non-enumeration
//!   claim above is about anonymous callers only.
//! - **A non-staff token cannot see the `published` failure.**
//!   `user_can_access_app` gates the customer path on `published_at.is_some()`,
//!   so for an org-member token an *unpublished* app returns `403`, not a
//!   `published: fail` verdict. The alarm still fires — the body says `fail` —
//!   but the diagnosis reads "not permitted" when the real cause is "never
//!   promoted". The `403` detail says so, and a staff-scoped token tells the two
//!   apart. This is deliberate: re-deriving access here to dodge it would mean a
//!   second copy of the rule `Ring::AppAccess` owns.
//!
//! ## Not a Kubernetes probe
//!
//! Same rule as `healthcheck.rs`, for the same reason: `/health` there returns
//! 503 when Postgres is unreachable, and wiring that to a `livenessProbe` gave
//! oxy-prod a crash loop layered on top of a database incident (2026-08-11).
//! This endpoint reports on an *app*, not on the process serving it — a 503 here
//! means "that app is not serving", never "restart me".
//!
//! ## Fleet posture
//!
//! Postgres reads (`apps`, and `app_builds` on the S3 path) plus at most one
//! build-store `HEAD` → **FleetOk**,
//! which is the manifest default and is pinned by `custom_app_health_is_fleet_ok`
//! rather than by a manifest entry. Not incidental: a liveness endpoint routed to
//! the ide singleton would report failure every time that singleton restarted,
//! which is precisely backwards. (The build store falls back to the state dir when
//! no bucket is configured, but that is the single-node dev path; hosted fleets
//! always have a bucket.)

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use entity::apps;
use entity::prelude::AppBuilds;
use oxy::database::client::establish_connection;
use sea_orm::EntityTrait;
use uuid::Uuid;

use super::custom_apps_auth::authenticate_and_authorize;
use super::custom_apps_source::AppSource;

mod report;

pub use report::{AppRef, BuildRef, Check, HealthResponse};
use report::{ENTRYPOINT, FAIL, LADDER, PASS, skip_remaining};

/// `GET /api/customer-apps/{org_slug}/{app_slug}/health`
///
/// Pollable from any host, so one monitor can watch every app from the admin
/// hostname rather than needing a check per subdomain.
pub async fn get_health(
    Path((org_slug, app_slug)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    health_for(&headers, &org_slug, &app_slug).await
}

/// `GET /api/customer-apps/health`
///
/// The subdomain form: resolves the app from the `Host` header, so a monitor can
/// watch `https://<org>--<slug>.customer-apps.<zone>/api/customer-apps/health`
/// and read like the app's own endpoint. `/api/*` is the one prefix the
/// subdomain rewrite middleware passes through untouched, which is what makes
/// this reachable at all.
pub async fn get_health_for_host(headers: HeaderMap) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    match oxy_app_core::custom_apps_host_dispatch::parse_subdomain(host) {
        Some((org_slug, app_slug)) => health_for(&headers, &org_slug, &app_slug).await,
        // Deliberately 404 rather than 400: on the admin host this path simply
        // does not name an app, and the slug-explicit route above is the one to
        // use there.
        None => error_response(
            StatusCode::NOT_FOUND,
            "not a custom-app subdomain — use /api/customer-apps/{org}/{app}/health",
        ),
    }
}

async fn health_for(headers: &HeaderMap, org_slug: &str, app_slug: &str) -> Response {
    // Auth first, app lookup second — that ordering is what keeps this from
    // becoming an anonymous enumeration oracle. See the module docs.
    let outcome = match authenticate_and_authorize(headers, org_slug, app_slug).await {
        Ok(o) => o,
        Err(status) => return error_response(status, reason(status)),
    };
    let app_ref = AppRef {
        id: outcome.app.id,
        org_slug: org_slug.to_string(),
        slug: app_slug.to_string(),
    };
    let (build, checks) = evaluate(&outcome.app).await;
    respond(app_ref, build, checks)
}

/// Does this source serve from a build in the store?
///
/// The serve path dispatches on `AppSource` **before** it looks at any build
/// pointer, and only the S3 arm resolves one — a V0 app is proxied upstream and a
/// local-folder app is read from a directory. So "is there a published build" is
/// a question that only exists for S3.
fn serves_from_build_store(source: &AppSource) -> bool {
    // Exhaustive on purpose, not `matches!`. `custom_apps_source` advertises that
    // adding a variant is contained — "extend the enum + parse + handle" — and a
    // `matches!` would answer `false` for a new one in silence, quietly giving it
    // `published_at` semantics and skipping both store rungs with nothing failing
    // to build. `check_entrypoint` is already exhaustive; this is the other half.
    match source {
        AppSource::S3 => true,
        AppSource::V0 { .. } | AppSource::LocalFolder { .. } => false,
    }
}

/// The two columns the publication rung reads, lifted off the row so the rule can
/// be unit-tested per source kind without constructing a whole `apps::Model`.
#[derive(Debug, Clone, Copy)]
struct PublicationState {
    /// `published_at` is set — the only publication state a non-S3 app has.
    marked_published: bool,
    /// `published_build_id` is set — meaningful only for an S3 app.
    has_published_build: bool,
}

impl PublicationState {
    fn of(app: &apps::Model) -> Self {
        Self {
            marked_published: app.published_at.is_some(),
            has_published_build: app.published_build_id.is_some(),
        }
    }
}

/// Is this app published, in the sense **its own source kind** uses?
///
/// Pure, so every source's verdict is unit-testable without a database — which is
/// how the bug this replaces would have been caught. That bug: the ladder asked
/// `published_build_id.is_some()` for everyone, but `publish_one` sets that column
/// only `if let Some(ptr) = draft_ptr`, and a V0 or local-folder app has no
/// `app_builds` rows to point at. Publishing one is, in the serve path's own words,
/// "purely a sidebar visibility toggle" — so it stamps `published_at` and leaves
/// the pointer NULL, forever. Every healthy, serving V0 app therefore reported
/// `published: fail` → 503 on the first poll and every poll after, and republishing
/// could not clear it. Worst on the source kind most likely to actually disappear,
/// where a permanently-red monitor makes the real outage invisible.
fn publication_check(state: PublicationState, source: &AppSource) -> Check {
    const NAME: &str = "published";
    if !serves_from_build_store(source) {
        return if state.marked_published {
            Check::pass(NAME)
        } else {
            Check::fail(
                NAME,
                "app is not published — for an externally hosted or local-folder app that is the \
                 whole of its publication state (a visibility toggle), so there is nothing else \
                 to promote",
            )
        };
    }
    match (state.has_published_build, state.marked_published) {
        (true, _) => Check::pass(NAME),
        // Published, but nothing was ever promoted into the published channel.
        // Distinct from "not published", reachable with an ordinary org-member
        // token, and exactly the state the serve path 404s on — so it gets its
        // own sentence rather than the never-promoted one.
        (false, true) => Check::fail(
            NAME,
            "app is marked published but has no published build — `published_build_id` is unset, \
             so the serve path 404s every request. Run `oxyc publish`, or promote an existing build.",
        ),
        (false, false) => Check::fail(
            NAME,
            "app has no published build — it has never been promoted, or was unpublished",
        ),
    }
}

/// Walk [`report::LADDER`] against the **published** channel. A draft build is what
/// staff see behind a cookie; an external monitor is asking what a real visitor gets.
async fn evaluate(app: &apps::Model) -> (Option<BuildRef>, Vec<Check>) {
    let mut checks = vec![Check::pass("registered")];

    // Source first: it decides what every later rung means. A source the serve
    // path can't parse is also a hard failure with its OWN remediation — the
    // serve handler 500s on the same error, and re-publishing does not repair a
    // malformed `source_config`.
    let source = match AppSource::from_model(app) {
        Ok(s) => s,
        Err(e) => {
            checks.push(Check::fail(
                "source_config",
                format!(
                    "source_config is unreadable ({e}) — every request for this app fails at \
                     dispatch. Fix the app's source configuration; re-publishing will not repair it."
                ),
            ));
            skip_remaining(&mut checks, "source configuration is unreadable");
            return (None, checks);
        }
    };
    checks.push(Check::pass("source_config"));

    let publication = publication_check(PublicationState::of(app), &source);
    let published = publication.result == PASS;
    checks.push(publication);
    if !published {
        skip_remaining(&mut checks, "app is not serving a published build");
        return (None, checks);
    }

    if !serves_from_build_store(&source) {
        checks.push(Check::skipped(
            "build_record",
            "this source kind serves without a build record — oxy proxies or reads it directly",
        ));
        checks.push(check_entrypoint(app.id, "", &source).await);
        return (None, checks);
    }

    // `publication_check` only passes an S3 app when the pointer is set, so this
    // branch is unreachable today. It is a check rather than an `unreachable!`
    // because the invariant now lives in a different function from the read: if
    // the two ever drift, a panic here costs a monitor its answer entirely —
    // there is no `CatchPanicLayer` in this workspace, so the caller gets a
    // dropped connection instead of the `fail` body every other path guarantees.
    let Some(build_pk) = app.published_build_id else {
        checks.push(Check::fail(
            "build_record",
            "internal inconsistency: the app passed the publication check without a \
             published build pointer",
        ));
        skip_remaining(&mut checks, "could not identify the published build");
        return (None, checks);
    };
    let build = match load_build(build_pk).await {
        Ok(b) => b,
        Err(detail) => {
            checks.push(Check::fail("build_record", detail));
            skip_remaining(&mut checks, "could not read the build record");
            return (None, checks);
        }
    };
    checks.push(Check::pass("build_record"));
    let build_ref = BuildRef {
        build_id: build.build_id.clone(),
        published_at: app.published_at.map(|t| t.to_rfc3339()),
    };
    checks.push(check_entrypoint(app.id, &build.build_id, &source).await);
    (Some(build_ref), checks)
}

/// The published build row, or a caller-facing reason it could not be read.
async fn load_build(build_pk: Uuid) -> Result<entity::app_builds::Model, String> {
    // The auth helper connects first and answers 500 on a dead pool, so this
    // branch is rarely the one that reports a database outage. Kept because
    // "rarely" is not "never" — a pool that dies between the two calls lands
    // here, and a health endpoint should name what it could not check.
    let db = establish_connection()
        .await
        .map_err(|e| format!("database unreachable: {e}"))?;
    match AppBuilds::find_by_id(build_pk).one(&db).await {
        Ok(Some(b)) => Ok(b),
        // The pointer outlived the row. Rare, but exactly the state that serves
        // a 404 to every visitor while the admin list still shows the app as
        // published.
        Ok(None) => Err("published_build_id points at a build row that no longer exists".into()),
        Err(e) => Err(format!("build lookup failed: {e}")),
    }
}

/// Is the bundle's entry file actually there?
///
/// `HEAD`, not `GET`: "present and non-empty" is answered identically without
/// transferring the file, and this endpoint is designed to be polled per app.
async fn check_entrypoint(app_id: Uuid, build_id: &str, source: &AppSource) -> Check {
    const NAME: &str = "bundle_entrypoint";
    match source {
        AppSource::S3 => {
            match super::custom_apps_build_store::head_object(app_id, build_id, ENTRYPOINT).await {
                // Unknown size counts as present: a store that declines to report
                // `Content-Length` has told us the object is there, and only a
                // size *known* to be zero is a broken bundle.
                Ok(Some(size)) if !size.is_known_empty() => Check::pass(NAME),
                Ok(Some(_)) => Check::fail(
                    NAME,
                    format!("{ENTRYPOINT} is present but empty in the build store"),
                ),
                Ok(None) => Check::fail(
                    NAME,
                    format!(
                        "{ENTRYPOINT} is absent from the build store for build {build_id} — \
                         re-publish the app"
                    ),
                ),
                Err(e) => Check::fail(NAME, format!("build store unreachable: {e}")),
            }
        }
        // Probing the upstream on every poll would put a third party's latency
        // and rate limits on our critical path and make this endpoint report
        // their uptime. Routing is verified; the upstream is declared un-probed
        // rather than guessed at.
        AppSource::V0 { .. } => Check::skipped(
            NAME,
            "externally hosted (v0) app — oxy proxies it and does not host the bundle, so this \
             endpoint verifies routing only. Monitor the upstream directly as well.",
        ),
        AppSource::LocalFolder { .. } => Check::skipped(
            NAME,
            "local-folder app — served from a developer machine's disk, not the build store",
        ),
    }
}

/// 200 when every evaluated check passed, 503 otherwise.
///
/// `skipped` does not fail the verdict — it means "we could not evaluate this",
/// and the detail says why. Treating it as a failure would mark every V0 app
/// permanently down; treating it as a pass would claim we checked something we
/// did not, which is why it is neither.
fn respond(app: AppRef, build: Option<BuildRef>, checks: Vec<Check>) -> Response {
    // `skip_remaining` holds the bail-out paths to `LADDER`; nothing held the
    // happy path, so adding a name to the const would have silently omitted it
    // from every *passing* body with no test failing. One assert covers both.
    debug_assert!(
        checks.iter().map(|c| c.name).eq(LADDER),
        "every response must report LADDER in order; got {:?}",
        checks.iter().map(|c| c.name).collect::<Vec<_>>()
    );
    let failed = checks.iter().any(|c| c.result == FAIL);
    let body = HealthResponse {
        oxy_app_health: if failed { FAIL } else { PASS },
        app,
        build,
        checks,
        checked_at: chrono::Utc::now().to_rfc3339(),
    };
    let status = if failed {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (status, no_store(), axum::Json(body)).into_response()
}

/// An auth/lookup outcome, framed so a body-matching monitor sees `fail` here
/// exactly as it would on a real verdict.
fn error_response(status: StatusCode, detail: &str) -> Response {
    (
        status,
        no_store(),
        axum::Json(serde_json::json!({
            "oxy_app_health": FAIL,
            "error": detail,
        })),
    )
        .into_response()
}

/// A cached 200 outliving the failure it preceded is the one outcome a liveness
/// endpoint must never produce — and a cached 403 in front of a monitor is the
/// same bug wearing a different status, so every response carries this.
fn no_store() -> [(header::HeaderName, &'static str); 1] {
    [(header::CACHE_CONTROL, "no-store")]
}

fn reason(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication required",
        // Spelled out because the obvious reading sends an operator after the
        // wrong thing: the access gate requires publication, so a non-staff
        // token sees this for an unpublished app too.
        StatusCode::FORBIDDEN => {
            "not permitted to view this app — a non-staff token also gets this for an app that \
             is not published; retry with a staff-scoped token to tell the two apart"
        }
        StatusCode::NOT_FOUND => "no such app",
        _ => "health check could not be performed",
    }
}

#[cfg(test)]
mod tests;
