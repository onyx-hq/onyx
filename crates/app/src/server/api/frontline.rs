//! Frontline sign-in — the HTTP surface a kiosk talks to.
//!
//! `oxy_auth::frontline` decides whether a PIN is right. This decides what that
//! is worth: a session, scoped and short, that lets a worker be somebody for
//! the length of a shift.
//!
//! # Why these routes are public, and what stands in for auth
//!
//! A worker has nothing to authenticate WITH until they have signed in, so both
//! routes below sit in the public router. That makes rate limiting load-bearing
//! rather than defensive, and it is layered:
//!
//! * the credential itself throttles and locks out per `(org, identifier)`
//!   ([`oxy_auth::frontline::verify_pin`]);
//! * this module throttles per **org** as well, because the credential-level
//!   lockout is per worker and a caller walking a roster of 40 names gets 40
//!   separate budgets;
//! * every failure returns one response, so neither layer leaks which of the
//!   two refused.
//!
//! # Fleet role
//!
//! Both routes are `route_fleet`, and must be. They read and write Postgres and
//! touch no working copy, no `.git` and no state dir — and more to the point,
//! **signing in has to survive the ide restarting**. Pinning login to the
//! singleton would mean a deploy locks every store out of its own checklists.

use crate::server::api::middlewares::role_guards::OrgAdmin;
use crate::server::api::operating_graph::assignments;
use crate::server::api::operating_graph::dto::AssignmentSpec;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, http::header};
use entity::{org_frontline_members, org_role_members, organizations, user_credentials, users};
use oxy::database::client::establish_connection;
use oxy_app_core::audit;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_auth::frontline::{self, KIND_PIN, PinPolicy, PinVerdict};
use oxy_shared::errors::OxyError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info, instrument, warn};
use uuid::Uuid;

/// How long a shift session lasts.
///
/// Twelve hours, not the week a magic-link session gets: this credential was
/// proved by four digits typed on a shared tablet in a room full of people, and
/// the tablet does not leave the store. A closing shift is the long case.
///
/// It is a CEILING, not the working rule. What actually ends an unattended
/// shift is the kiosk's idle timeout
/// (`frontline_devices::DEFAULT_IDLE_TIMEOUT_SECONDS`), which is why that
/// module reads this constant rather than keeping its own copy of twelve.
pub(crate) const SHIFT_HOURS: i64 = 12;

/// Per-org attempt ceiling within [`ORG_WINDOW`].
///
/// The credential's own lockout is per worker, so a caller trying `0000` once
/// against each of 40 names never trips it — 40 identifiers, one attempt each.
/// This is the ceiling that notices the *pattern* rather than the account.
const ORG_ATTEMPT_CEILING: usize = 30;
const ORG_WINDOW: Duration = Duration::from_secs(60);

/// Failed attempts per org, newest last. In-process on purpose: this is a
/// coarse brake in front of the real per-credential throttle, not the throttle
/// itself, so a per-replica window is the right cost. Making it shared state
/// would put a Postgres write on every wrong keypress to slow down an attacker
/// the credential layer already locks out.
static ORG_ATTEMPTS: LazyLock<Mutex<HashMap<Uuid, Vec<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn org_is_rate_limited(org_id: Uuid) -> bool {
    // A poisoned lock means a previous holder panicked. RECOVER rather than
    // fail closed: poisoning is permanent, so `Err(_) => return true` refused
    // every sign-in for every org for the life of the process — a self-inflicted
    // outage far larger than the guessing it was meant to stop. The guarded data
    // is a map of timestamps with no invariant a panic could leave broken, so
    // there is nothing to protect by refusing to read it.
    let mut map = match ORG_ATTEMPTS.lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    let hits = map.entry(org_id).or_default();
    hits.retain(|t| now.duration_since(*t) < ORG_WINDOW);
    hits.len() >= ORG_ATTEMPT_CEILING
}

fn record_org_attempt(org_id: Uuid) {
    let mut map = match ORG_ATTEMPTS.lock() {
        Ok(m) => m,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.entry(org_id).or_default().push(Instant::now());
}

/// The throttle key for a request that named an org that does not exist.
///
/// There is no org id to key on, so key on the absence of one: a caller walking
/// slugs is one caller, and this bucket has no legitimate traffic to starve —
/// the worst it costs a real user is a `429` on a typo'd slug.
const NO_SUCH_ORG: Uuid = Uuid::nil();

#[derive(Debug, Deserialize)]
pub struct RosterQuery {
    /// Org slug — the kiosk knows which store it is bolted to.
    pub org: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RosterEntry {
    /// The stable login name the kiosk sends back with the PIN. Not the
    /// display name: renaming a worker must not change how they sign in.
    pub identifier: String,
    pub name: String,
}

/// The name picker.
///
/// # Whose names it shows
///
/// **The tablet's store, not the tenant.** A kiosk bound to a location lists
/// only workers who hold an assignment AT that location
/// (`org_role_members.location_id`, the operating graph's roster — see
/// `operating_graph::assignments`). A kiosk with no location keeps the
/// org-wide list, because there is no store to narrow to.
///
/// Two exclusions are deliberate rather than oversights:
///
/// * **An org-wide position does not appear.** `org_role_members` stores
///   `location_id IS NULL` for a franchisor-scope role, and an Account Manager
///   who works across every store signs in on the web — putting them on every
///   tablet in the chain is the org-wide picker again under another name.
/// * **There is no per-assignment status column.** Standing is the worker's
///   (`org_frontline_members.status`), already filtered below, so "active
///   assignment" means the worker is active and the assignment row exists.
///   Un-rostering somebody from a store is a delete, and it takes them off
///   that store's picker.
///
/// Until 2026-09-11 this listed every PIN holder in the org, so a Santa Rosa
/// manager appeared on the Clovis tablet — a name picker of 127 strangers,
/// and a wider guessing surface for anyone standing at the counter.
///
/// **The picker narrows; sign-in does not.** `login` below checks the org, the
/// kiosk's binding to it and the PIN — never `org_role_members`. A worker who
/// knows their own identifier can still sign in on another store's tablet.
/// This is disclosure hygiene, not a location check; the reach a signed-in
/// worker then holds is the operating graph's business, not this route's.
///
/// **Deleting a location re-widens its tablets.** `org_kiosk_devices.location_id`
/// is `ON DELETE SET NULL`, and `NULL` here means org-wide, so a kiosk whose
/// place was deleted falls back to the whole tenant's list. Re-bind or revoke
/// such a kiosk rather than leaving it.
///
/// # What it still does not carry
///
/// Deliberately **no** credential material and **no** lockout state. It is
/// rendered on a screen anyone in the building can see, so it must not help
/// an attacker choose a target — "this one is locked out" would confirm both
/// that the worker exists and that somebody has been guessing at them.
///
/// It does leak the roster of one store to anyone holding that store's kiosk
/// cookie. That is a deliberate trade and the reason the PIN is not the only
/// control: the tablet is on the wall, the names are on the schedule beside
/// it, and a name picker nobody can load is a kiosk nobody can use.
#[instrument(skip_all, fields(org = %q.org))]
pub async fn roster(headers: HeaderMap, Query(q): Query<RosterQuery>) -> axum::response::Response {
    // The body depends on the kiosk cookie; see `frontline_devices::no_store`.
    let mut resp = roster_body(&headers, q).await;
    super::frontline_devices::no_store(&mut resp);
    resp
}

async fn roster_body(headers: &HeaderMap, q: RosterQuery) -> axum::response::Response {
    let Ok(db) = establish_connection().await else {
        // `{"staff": []}`, not `{}` — a kiosk reads `body.staff` and would get
        // `undefined` from the bare object, which is a render crash rather than
        // an empty picker.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "staff": [] })),
        )
            .into_response();
    };

    // Only an enrolled kiosk of this org sees its roster. The doc on `login`
    // records that this read disclosed org existence by content; behind the
    // device it discloses nothing to anyone who is not already standing at
    // the counter, and the same empty answer covers "no kiosk", "wrong org"
    // and "no such org".
    let device = super::frontline_devices::bound_device(&db, headers).await;
    let bound_to = device.as_ref().map(|d| d.org_id);
    let Ok(Some(org)) = organizations::Entity::find()
        .filter(organizations::Column::Slug.eq(&q.org))
        .one(&db)
        .await
    else {
        // An unknown org answers as an EMPTY roster, not a 404. A 404 here is
        // an org-slug oracle, and slugs are guessable.
        return Json(serde_json::json!({ "staff": [] })).into_response();
    };

    if bound_to != Some(org.id) {
        return Json(serde_json::json!({ "staff": [] })).into_response();
    }

    let rows = user_credentials::Entity::find()
        .filter(user_credentials::Column::Kind.eq(KIND_PIN))
        .filter(user_credentials::Column::OrgId.eq(Some(org.id)))
        .order_by_asc(user_credentials::Column::Identifier)
        // A roster is a screen, not a dataset. The cap is what stops a large
        // tenant turning the picker into a slow query on every kiosk load.
        .limit(200)
        .all(&db)
        .await
        .unwrap_or_default();

    // Names come from `users`, and only for workers whose standing is active —
    // a suspended worker must not appear on the picker at all.
    //
    // Two batched queries, not two per credential. The `.limit(200)` above caps
    // the ROW count, not the QUERY count, and this route is public, unthrottled
    // (`org_is_rate_limited` guards `login` only) and answers for any guessable
    // slug — so the per-row shape made a trivial loop a 400x amplifier against
    // the shared pool.
    let ids: Vec<Uuid> = rows.iter().map(|c| c.user_id).collect();
    let active: std::collections::HashSet<Uuid> = org_frontline_members::Entity::find()
        .filter(org_frontline_members::Column::OrgId.eq(org.id))
        .filter(org_frontline_members::Column::UserId.is_in(ids.clone()))
        .filter(org_frontline_members::Column::Status.eq(org_frontline_members::STATUS_ACTIVE))
        .all(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.user_id)
        .collect();
    let names: std::collections::HashMap<Uuid, String> = users::Entity::find()
        .filter(users::Column::Id.is_in(ids))
        .all(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|u| (u.id, u.name))
        .collect();

    // Built from `rows` so the `identifier` sort the query asked for survives —
    // a picker whose order changes between loads is a picker people mis-tap.
    let candidates: Vec<(Uuid, RosterEntry)> = rows
        .into_iter()
        .filter(|c| active.contains(&c.user_id))
        .filter_map(|c| {
            names.get(&c.user_id).map(|name| {
                (
                    c.user_id,
                    RosterEntry {
                        identifier: c.identifier,
                        name: name.clone(),
                    },
                )
            })
        })
        .collect();

    // Where each of them is rostered. One more bounded read, and only when the
    // tablet has a place to narrow to — an org whose kiosks carry no location
    // pays nothing for a rule that cannot apply to it.
    //
    // `unwrap_or_default` on the read fails CLOSED, which is the direction this
    // one has to fail in: no assignment rows means an empty picker, not the
    // whole tenant's crew appearing on a store's tablet because a query blipped.
    // Closed but not silent — the warn is what separates "nobody is rostered
    // here" from "the query failed" for whoever is staring at an empty picker.
    let location = device.and_then(|d| d.location_id);
    let assignments: Vec<(Uuid, Option<Uuid>)> = match location {
        None => Vec::new(),
        Some(at) => org_role_members::Entity::find()
            .filter(org_role_members::Column::OrgId.eq(org.id))
            .filter(org_role_members::Column::LocationId.eq(at))
            .filter(
                org_role_members::Column::UserId
                    .is_in(candidates.iter().map(|(id, _)| *id).collect::<Vec<_>>()),
            )
            .all(&db)
            .await
            .inspect_err(|e| {
                warn!(error = %e, org_id = %org.id, location = %at,
                    "roster assignment read failed; the picker will be empty")
            })
            .unwrap_or_default()
            .into_iter()
            .map(|a| (a.user_id, a.location_id))
            .collect(),
    };

    let candidates_in = candidates.len();
    let staff = narrow_to_location(candidates, location, &assignments);
    info!(org_id = %org.id, location = ?location, candidates = candidates_in, staff = staff.len(),
        "frontline roster narrowed");
    Json(serde_json::json!({ "staff": staff })).into_response()
}

/// The rule the picker exists to enforce: a kiosk at a place shows the people
/// assigned to that place.
///
/// Pure, and given the assignment rows as `(user_id, location_id)` pairs, so
/// the rule can be tested without a database. It is the whole point of the
/// read and is one deleted line away from being org-wide again.
///
/// `None` for `location` is a kiosk the admin enrolled without a place —
/// today's org-wide behaviour, kept rather than treated as "nowhere".
/// `Some(_)` matches on the location EXACTLY: an assignment carrying
/// `location_id IS NULL` is an org-wide position and does not put its holder
/// on a store's tablet.
fn narrow_to_location(
    candidates: Vec<(Uuid, RosterEntry)>,
    location: Option<Uuid>,
    assignments: &[(Uuid, Option<Uuid>)],
) -> Vec<RosterEntry> {
    let Some(location) = location else {
        return candidates.into_iter().map(|(_, entry)| entry).collect();
    };
    let here: std::collections::HashSet<Uuid> = assignments
        .iter()
        .filter(|(_, at)| *at == Some(location))
        .map(|(user, _)| *user)
        .collect();
    candidates
        .into_iter()
        .filter(|(user, _)| here.contains(user))
        .map(|(_, entry)| entry)
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub org: String,
    pub identifier: String,
    pub pin: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub name: String,
    /// Seconds until the session expires — so a kiosk can show "signs out at
    /// 23:00" rather than discovering it mid-submission.
    pub expires_in: i64,
}

/// The `Set-Cookie` for a shift session.
///
/// Lifted out of `login` so the handler stays near the ~30-line guidance in
/// `crates/app/CLAUDE.md`, and because both decisions below are ones a reader
/// needs to see together rather than buried in a response builder.
fn shift_session_headers(token: &str, req_headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    // The same session cookie the web app uses, so an installed PWA carries it
    // on navigation without the page having to hold the token itself — and built
    // from the same two decisions the magic-link path makes, rather than by a
    // second rule that happens to agree in production.
    //
    // `Secure` from the REQUEST, not from the serve mode. A dev box is cloud
    // mode with non-prod secrets served over plain `http://localhost`, so
    // `!process_is_local()` set `Secure`, the browser discarded the cookie, and
    // kiosk sign-in returned 200 without sticking while magic-link login on the
    // same box worked. `is_request_secure` also honours `X-Forwarded-Proto`,
    // which matters behind an ingress terminating TLS with neither env var set.
    //
    // Max-Age from SHIFT_HOURS, not the 30-day default. The shift TTL was only
    // enforced by the JWT `exp`, so the browser kept a dead cookie for the rest
    // of that window — the morning after a shift the kiosk looked signed in and
    // 401'd on every call instead of showing the name picker.
    let secure = super::auth::is_request_secure(req_headers);
    if let Ok(v) = header::HeaderValue::from_str(&super::auth::build_session_cookie_with_max_age(
        token,
        secure,
        SHIFT_HOURS * 3600,
    )) {
        out.insert(header::SET_COOKIE, v);
    }
    out
}

/// Exchange a PIN for a shift session.
///
/// The session is an ordinary Oxy JWT with a 12-hour expiry. It is narrow not
/// because the token says so but because the AUTHZ MODEL makes it narrow: a
/// frontline worker holds `org_frontline_members` standing, which
/// `oxy_authz::Ring::AppAccess` reads only when ANDed with an explicit
/// `app_members` grant. They reach the apps they were enrolled to use and
/// nothing else — no org read, no workspace, no settings.
///
/// That is worth stating because the alternative was tempting: a bespoke
/// token type, with a bespoke validation path, and a second place for an
/// authorization bug to live.
#[instrument(skip_all, fields(org = %body.org, identifier = %body.identifier))]
pub async fn login(req_headers: HeaderMap, body: Json<LoginRequest>) -> impl IntoResponse {
    let Json(body) = body;

    let Ok(db) = establish_connection().await else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE);
    };

    let Ok(Some(org)) = organizations::Entity::find()
        .filter(organizations::Column::Slug.eq(&body.org))
        .one(&db)
        .await
    else {
        // THE BRAKE COMES FIRST. Closing the timing channel means paying the
        // Argon2 cost on this branch too — `Argon2::default()` is RFC 9106's
        // second profile, m = 19456 KiB and t = 2 — and this is the one path
        // through `login` that `org_is_rate_limited` below cannot cover, because
        // it has no org id to key on. Burning unmetered here would trade a
        // timing oracle for something strictly worse: ~19 MiB and two Argon2
        // passes per request, unauthenticated, at whatever concurrency the
        // caller picks. So the unknown-slug path gets its own bucket, and ends
        // up with exactly the same cost profile as the known-slug path.
        if org_is_rate_limited(NO_SUCH_ORG) {
            return refuse(StatusCode::TOO_MANY_REQUESTS);
        }
        record_org_attempt(NO_SUCH_ORG);

        // Same refusal as a wrong PIN, and the same COST. Matching bodies is
        // only half of it: a known slug goes on to `verify_pin`, which always
        // pays the verify, so returning after one indexed SELECT left the two
        // branches an order of magnitude apart in latency.
        //
        // Load-bearing, not belt-and-braces. `roster` used to disclose org
        // existence by content (`{"staff": []}` for an unknown slug, a list for
        // a known one), which made this branch redundant; now that `roster`
        // answers `{"staff": []}` to anyone without this org's kiosk cookie,
        // this is the only place an unauthenticated caller could have told a
        // real slug from an invented one — and it does not.
        oxy_auth::frontline::burn_verify_time(&body.pin);
        return refuse(StatusCode::UNAUTHORIZED);
    };

    if org_is_rate_limited(org.id) {
        warn!(org_id = %org.id, "frontline login rate-limited for this org");
        // 429 rather than 401: this one IS worth telling the caller apart,
        // because a kiosk should back off rather than retry, and the fact
        // leaked ("somebody is guessing at this org") is not the roster.
        return refuse(StatusCode::TOO_MANY_REQUESTS);
    }

    // The device before the PIN. A PIN is only ever verified for a request
    // that proved it comes from one of THIS org's enrolled kiosks
    // (`frontline_devices`); typed anywhere else it is refused exactly as a
    // wrong PIN is — same status, same body, same cost, and it counts against
    // the org's attempt budget — so a client without a kiosk cookie learns
    // nothing about whether the identifier exists. This is the binding the
    // design record required before any of this faced a user.
    let device = match super::frontline_devices::bound_device(&db, &req_headers).await {
        Some(d) if d.org_id == org.id => d,
        _ => {
            record_org_attempt(org.id);
            oxy_auth::frontline::burn_verify_time(&body.pin);
            info!(org_id = %org.id, "frontline login refused: no kiosk bound to this org");
            return refuse(StatusCode::UNAUTHORIZED);
        }
    };

    let verdict = match frontline::verify_pin(
        &db,
        org.id,
        &body.identifier,
        &body.pin,
        PinPolicy::default(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "frontline verify failed");
            return refuse(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    let PinVerdict::Ok { user_id } = verdict else {
        record_org_attempt(org.id);
        // One response for every failure — wrong PIN, locked out, no such
        // worker, malformed. `PinVerdict::public_message` exists for exactly
        // this and the difference stays in the log.
        info!(verdict = ?verdict, "frontline login refused");
        return refuse(StatusCode::UNAUTHORIZED);
    };

    let Ok(Some(user)) = users::Entity::find_by_id(user_id).one(&db).await else {
        return refuse(StatusCode::UNAUTHORIZED);
    };
    let name = user.name.clone();

    let token =
        match super::auth::create_auth_token_with_ttl(user, chrono::Duration::hours(SHIFT_HOURS))
            .await
        {
            Ok(t) => t,
            Err(status) => return refuse(status),
        };

    info!(%user_id, org_id = %org.id, device = %device.id, "frontline session opened");
    super::frontline_devices::touch(&db, device.id).await;

    let out_headers = shift_session_headers(&token, &req_headers);

    (
        out_headers,
        Json(LoginResponse {
            token,
            name,
            expires_in: SHIFT_HOURS * 3600,
        }),
    )
        .into_response()
}

/// Every refusal, in one shape.
fn refuse(status: StatusCode) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({ "error": "that PIN did not match" })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cookie has to die with the token it carries.
    ///
    /// It did not: `build_session_cookie` hardcoded seven days while the shift
    /// JWT expires in twelve hours, so the browser kept presenting a dead
    /// credential for another six days. That does not read as "signed out" — the
    /// kiosk looks signed in and 401s on every call.
    #[test]
    fn the_session_cookie_expires_with_the_shift() {
        let cookie =
            super::super::auth::build_session_cookie_with_max_age("tok", true, SHIFT_HOURS * 3600);
        assert!(
            cookie.contains(&format!("Max-Age={}", SHIFT_HOURS * 3600)),
            "cookie outlives the token: {cookie}"
        );
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
    }

    /// `Secure` comes from the request, not the serve mode. A dev box is cloud
    /// mode over plain http, so a mode-derived flag set `Secure` there and the
    /// browser silently dropped the cookie.
    #[test]
    fn an_insecure_request_gets_a_cookie_the_browser_will_keep() {
        let cookie = super::super::auth::build_session_cookie_with_max_age("tok", false, 3600);
        assert!(
            !cookie.contains("Secure"),
            "a plain-http kiosk would discard this: {cookie}"
        );
    }

    /// One entry, named after the person, so the assertions below read as
    /// "who is on this tablet" rather than as index arithmetic.
    fn worker(name: &str) -> (Uuid, RosterEntry) {
        (
            Uuid::new_v4(),
            RosterEntry {
                identifier: name.to_lowercase(),
                name: name.to_string(),
            },
        )
    }

    fn names(entries: &[RosterEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// The bug this closes: a kiosk bolted to Clovis listed every PIN holder
    /// in the org, so a Santa Rosa manager appeared on the Clovis tablet.
    #[test]
    fn a_kiosk_at_a_store_shows_only_that_stores_crew() {
        let clovis = Uuid::new_v4();
        let santa_rosa = Uuid::new_v4();
        let (maria, maria_entry) = worker("Maria");
        let (dev, dev_entry) = worker("Devon");
        let candidates = vec![(maria, maria_entry), (dev, dev_entry)];
        let assignments = vec![(maria, Some(clovis)), (dev, Some(santa_rosa))];

        assert_eq!(
            names(&narrow_to_location(
                candidates.clone(),
                Some(clovis),
                &assignments
            )),
            ["Maria"],
            "the tablet's own store is the roster"
        );
        // Same rows, other tablet — proving the filter reads the location it
        // was given rather than just dropping everyone but the first row.
        assert_eq!(
            names(&narrow_to_location(
                candidates,
                Some(santa_rosa),
                &assignments
            )),
            ["Devon"]
        );
    }

    /// An org-wide position is not a position at every store. An Account
    /// Manager signs in on the web; putting them on every tablet in the chain
    /// is the org-wide picker again under another name.
    #[test]
    fn an_org_wide_position_is_not_on_a_stores_tablet() {
        let clovis = Uuid::new_v4();
        let (maria, maria_entry) = worker("Maria");
        let (amy, amy_entry) = worker("Amy");
        let staff = narrow_to_location(
            vec![(maria, maria_entry), (amy, amy_entry)],
            Some(clovis),
            &[(maria, Some(clovis)), (amy, None)],
        );
        assert_eq!(names(&staff), ["Maria"]);
    }

    /// A worker with no assignment at all is nobody's crew — including on a
    /// tablet at a store they have never been rostered to.
    #[test]
    fn an_unrostered_worker_is_on_no_stores_tablet() {
        let clovis = Uuid::new_v4();
        let (ghost, ghost_entry) = worker("Ghost");
        assert!(
            narrow_to_location(vec![(ghost, ghost_entry)], Some(clovis), &[]).is_empty(),
            "an empty assignment table must not fall through to org-wide"
        );
    }

    /// A kiosk enrolled without a place keeps today's behaviour: there is no
    /// store to narrow to, and a picker nobody can load is a kiosk nobody can
    /// use.
    #[test]
    fn a_kiosk_with_no_place_still_shows_the_org() {
        let (maria, maria_entry) = worker("Maria");
        let (dev, dev_entry) = worker("Devon");
        let staff = narrow_to_location(
            vec![(maria, maria_entry), (dev, dev_entry)],
            None,
            // Assignments exist and must be ignored, not consulted.
            &[(maria, Some(Uuid::new_v4()))],
        );
        assert_eq!(names(&staff), ["Maria", "Devon"]);
    }

    /// The identifier order the query asked for is what people learn to tap.
    #[test]
    fn narrowing_keeps_the_order_the_query_chose() {
        let clovis = Uuid::new_v4();
        let (a, a_entry) = worker("Ana");
        let (b, b_entry) = worker("Bo");
        let (c, c_entry) = worker("Cy");
        let staff = narrow_to_location(
            vec![(a, a_entry), (b, b_entry), (c, c_entry)],
            Some(clovis),
            &[(c, Some(clovis)), (a, Some(clovis))],
        );
        assert_eq!(
            names(&staff),
            ["Ana", "Cy"],
            "the assignment rows' order must not reorder the picker"
        );
    }

    #[test]
    fn the_org_brake_opens_and_closes() {
        let org = Uuid::new_v4();
        assert!(!org_is_rate_limited(org), "a fresh org is not limited");
        for _ in 0..ORG_ATTEMPT_CEILING {
            record_org_attempt(org);
        }
        assert!(
            org_is_rate_limited(org),
            "the ceiling must actually stop a caller walking the roster — the \
             per-credential lockout cannot, because 40 names is 40 budgets"
        );
        // A different org is unaffected: the brake is per tenant, so one store
        // under attack cannot lock out another.
        assert!(!org_is_rate_limited(Uuid::new_v4()));
    }

    #[tokio::test]
    async fn every_refusal_carries_the_same_body() {
        // The status differs (429 tells a kiosk to back off) but the body must
        // not, or the response distinguishes "no such worker" from "wrong PIN".
        //
        // READ the body. The first version of this test `Debug`-formatted an
        // unread `axum::body::Body`, which prints the same opaque placeholder
        // for every response — so it compared three identical strings and could
        // not fail. It would have passed with three different bodies, which is
        // the only thing it was written to catch.
        let mut bodies = Vec::new();
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let bytes = axum::body::to_bytes(refuse(status).into_body(), 64 * 1024)
                .await
                .expect("refusal bodies are small and always present");
            bodies.push(String::from_utf8(bytes.to_vec()).expect("utf-8"));
        }
        assert!(
            !bodies[0].is_empty(),
            "a refusal with an empty body would make this vacuous again"
        );
        assert!(
            bodies.windows(2).all(|w| w[0] == w[1]),
            "refusals differ and leak which layer refused: {bodies:?}"
        );
    }

    /// The guard on the test above: prove the comparison can actually fail.
    ///
    /// Without this, a future edit that reverts `refuse` to something opaque
    /// makes the assertion vacuous again with nothing to notice.
    #[tokio::test]
    async fn the_refusal_body_comparison_can_fail() {
        let a = axum::body::to_bytes(refuse(StatusCode::UNAUTHORIZED).into_body(), 64 * 1024)
            .await
            .unwrap();
        let b = axum::body::to_bytes(
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"x": 1})))
                .into_response()
                .into_body(),
            64 * 1024,
        )
        .await
        .unwrap();
        assert_ne!(a, b, "two genuinely different bodies compared equal");
    }
}

// ── Enrolment ───────────────────────────────────────────────────────────────

/// No `Debug`. This struct holds a raw PIN, and a derived `Debug` is one
/// `tracing` field or one `unwrap` panic away from putting it in a log line.
#[derive(Deserialize)]
pub struct EnrolRequest {
    /// Shown on the kiosk's name picker. Not unique — two Marias are two rows.
    pub name: String,
    /// What the worker picks themselves out by. Unique per org.
    pub identifier: String,
    /// 4–8 digits. Never stored, never logged, never returned.
    pub pin: String,
    /// The apps this worker will use, by id — granted `member` in the same
    /// call. Optional: a manager can also grant later from an app's access
    /// settings, which accept an active worker. Every id must be this org's,
    /// or the whole request is refused before the worker exists.
    #[serde(default)]
    pub apps: Vec<Uuid>,
    /// Where they work, and as what — positions at places, written after the
    /// worker exists and checked before. Optional: a manager can also roster
    /// them later from Settings.
    #[serde(default)]
    pub assignments: Vec<AssignmentSpec>,
}

/// Enrol a frontline worker — the door `enroll_worker` never had.
///
/// # Why this is the missing piece
///
/// Everything else in this file already shipped: the PIN credential, the
/// standing row, the login exchange, the roster read. `oxy_auth::frontline::
/// enroll_worker` has existed the whole time with **zero non-test callers**, so
/// `GET /api/frontline/roster` has been answering `200 {"staff": []}` on every
/// deployment — a read path with no write path, which looks exactly like a
/// tenant that has not enrolled anybody.
///
/// # Who may call it
///
/// `OrgAdmin`, which is the guard the rest of the org's member management uses.
/// Deliberately NOT a new authorization concept: enrolling a worker is adding a
/// person to an org, and inventing a second rule for it is how two answers to
/// one question start disagreeing. A store manager who is not an org admin
/// cannot enrol yet — that is a real gap, and it is one for the roles model to
/// close rather than for this route to route around.
///
/// # What it deliberately does not do
///
/// No email, no invitation, no `org_members` row. That is the whole design:
/// `enroll_worker` writes `users.email = NULL`, which keeps this person out of
/// every email-keyed path — OAuth collapse, Slack matching, invitations,
/// platform grants — by construction rather than by a check somebody has to
/// remember. The worker exists, can sign in, and holds nothing else.
#[instrument(skip_all, fields(org = %org_id))]
pub async fn enrol(
    OrgAdmin(ctx): OrgAdmin,
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path(org_id): Path<Uuid>,
    Json(req): Json<EnrolRequest>,
) -> impl IntoResponse {
    let Ok(db) = establish_connection().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "database unavailable" })),
        )
            .into_response();
    };

    // The apps first, before a worker exists to be left half set up. Deciding
    // an app's audience is `AppAccessManage` — the ring the access settings
    // enforce — and `OrgAdmin` is not that ring, so a request that grants is
    // held to it here; then an id that is not this org's refuses the whole
    // request.
    let apps = super::frontline_grants::normalize_app_ids(req.apps);
    if !apps.is_empty()
        && !super::frontline_grants::may_grant_apps(
            &db,
            actor.id,
            actor.email.as_deref().unwrap_or(""),
            org_id,
        )
        .await
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "granting apps at enrolment needs the standing to manage app access"
            })),
        )
            .into_response();
    }
    if let Err(e) = super::frontline_grants::validate_apps_in_org(&db, org_id, &apps).await {
        return match e {
            super::frontline_grants::GrantError::NotThisOrg(_) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            super::frontline_grants::GrantError::Db(err) => {
                error!(%org_id, "frontline enrolment: app lookup failed: {err}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "enrolment failed" })),
                )
                    .into_response()
            }
        };
    }

    // The positions next, still before the worker exists: a position that is
    // not this org's, or a store for an org-wide position, refuses the whole
    // request rather than leaving a person half rostered.
    for spec in &req.assignments {
        if let Err(e) = assignments::validate_targets(&db, org_id, spec).await {
            return (
                e.status(),
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    match frontline::enroll_worker(
        &db,
        org_id,
        &req.name,
        &req.identifier,
        &req.pin,
        PinPolicy::default(),
    )
    .await
    {
        Ok(user_id) => {
            info!(%org_id, %user_id, apps = apps.len(), "frontline worker enrolled");
            // The grants, now that there is somebody to grant to. Validated
            // above, so the only way this fails is the database — and then the
            // worker exists without their apps, which the response must say
            // rather than report a clean 201.
            let granted = match super::frontline_grants::grant_apps_to_worker(
                &db,
                org_id,
                user_id,
                &apps,
                Some(actor.id),
            )
            .await
            {
                Ok(granted) => granted,
                Err(e) => {
                    error!(%org_id, %user_id, "worker enrolled, but the app grants failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "the worker was enrolled but could not be granted their apps \
                                      — grant them from each app's access settings",
                            "user_id": user_id,
                        })),
                    )
                        .into_response();
                }
            };
            // Gaining access is as auditable as losing it: the same entry the
            // access settings file, one per app, so this door is not the one
            // way to reach an app that leaves no trail.
            for app in &granted {
                super::org_teams::audit::record(
                    &db,
                    &ctx,
                    &actor,
                    super::org_teams::audit::APP_ACCESS_CHANGED,
                    (
                        "app",
                        app.id,
                        format!(
                            "{} ({}) ← enrolled {}",
                            app.name,
                            app.slug,
                            req.identifier.trim()
                        ),
                    ),
                )
                .await;
            }
            // Then the roster. Validated above, so again only the database
            // can fail this — and then the worker exists with their apps but
            // not their positions, which the response says.
            let rostered = match assignments::roster_at_enrolment(
                &db,
                org_id,
                user_id,
                &req.assignments,
                &actor,
            )
            .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    error!(%org_id, %user_id, "worker enrolled, but rostering failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": "the worker was enrolled but could not be rostered \
                                      — add their positions from Settings → Crew",
                            "user_id": user_id,
                        })),
                    )
                        .into_response();
                }
            };
            // The PIN is not echoed. An admin who did not keep it re-enrols or
            // resets; a response that repeats it would put it in every proxy
            // log between here and the browser.
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "user_id": user_id,
                    "identifier": req.identifier.trim(),
                    "name": req.name.trim(),
                    "apps": apps,
                    "assignments": rostered,
                })),
            )
                .into_response()
        }
        // Match the VARIANT, not every error.
        //
        // `enroll_worker` validates the PIN policy and the required fields, and
        // those messages are exactly what an admin needs — so a validation
        // failure is a 400 carrying its own sentence.
        //
        // Everything else is ours. Mapping them all to 400 told an admin that a
        // pool exhaustion was their bad request, and put raw database text
        // ("enrol begin: …") in the response body on the way. A 500 with a
        // generic body is the honest answer; the real error goes to the log,
        // where it belongs.
        Err(OxyError::ValidationError(msg)) => {
            warn!(%org_id, "frontline enrolment refused: {msg}");
            // 409 for a taken identifier, 400 for a malformed request.
            //
            // Re-enrolling somebody who already exists, or reusing a badge
            // number, is the most likely way this call fails and is entirely
            // the admin's to fix — it is a conflict with existing state, not a
            // bad request. Matched on `frontline::IDENTIFIER_TAKEN` rather than
            // on a literal, so the two sides cannot drift apart silently.
            let status = if msg == frontline::IDENTIFIER_TAKEN {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(serde_json::json!({ "error": msg }))).into_response()
        }
        Err(e) => {
            error!(%org_id, error = %e, "frontline enrolment failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "could not enrol the worker" })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct StandingRequest {
    /// `false` suspends, `true` reinstates.
    pub active: bool,
}

/// Suspend a frontline worker, or reinstate one.
///
/// The other half of enrolment, and it was missing: a worker could be enrolled
/// and never un-enrolled. The `status` column has modelled `suspended` since the
/// schema landed and nothing wrote it, so the door opened one way — which I
/// found by enrolling a test worker into a demo org and having no way to remove
/// them.
///
/// `PATCH`, not `DELETE`, because nothing is deleted. A worker who leaves keeps
/// their row so the work they did stays attributed; suspension is what takes
/// away the ability to sign in. `verify_pin` and the roster read the same
/// column, so one write closes the door on both the login and the name picker.
///
/// Idempotent. Suspending an already-suspended worker answers 200 with
/// `changed: false` rather than 409 — the caller asked for a state and that is
/// the state, and making a retry look like a conflict is how a client learns to
/// ignore the status code.
#[instrument(skip_all, fields(org = %org_id, worker = %user_id))]
pub async fn set_standing(
    OrgAdmin(ctx): OrgAdmin,
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<StandingRequest>,
) -> impl IntoResponse {
    let Ok(db) = establish_connection().await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "database unavailable" })),
        )
            .into_response();
    };

    match frontline::set_worker_standing(&db, org_id, user_id, req.active).await {
        Ok(changed) => {
            // "Who cut this worker off, and when."
            //
            // The neighbouring route in this same router writes
            // `org.member.removed` under a comment reading "Losing access is as
            // auditable as gaining it" — and suspension IS losing access. This
            // route shipped with a `tracing::info!` carrying no actor at all, so
            // once logs rolled the answer was unrecoverable.
            //
            // Only on a real change: an idempotent no-op is not an event, and an
            // audit log that records every retry is one nobody reads.
            //
            // Best-effort, like its neighbour: failing to write the trail must
            // not fail a revocation that has already happened. A worker whose
            // access was removed but whose removal went unlogged is bad; leaving
            // their access in place because the logging failed is worse.
            if changed {
                // `user_can_access_app` caches its verdict per (user, app) for
                // the app shell and every function invoke; a suspension that
                // left that entry warm would keep the kiosk working until it
                // expired. Same call every grant-changing route makes.
                crate::server::api::custom_apps_auth::invalidate_access_cache();
                let action = if req.active {
                    "frontline.worker.reinstated"
                } else {
                    "frontline.worker.suspended"
                };
                audit::record_best_effort(
                    &db,
                    audit::AuditEntry::new(actor.label().to_string(), action)
                        .actor(actor.id, audit::ActorType::User)
                        .org(ctx.org.id)
                        .target("frontline_worker", user_id.to_string(), String::new())
                        .change(
                            serde_json::json!({ "active": !req.active }),
                            serde_json::json!({ "active": req.active }),
                        ),
                )
                .await;
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "user_id": user_id,
                    "active": req.active,
                    // What the statement DID, not what was asked for. The two differ
                    // exactly when the worker was already in that state, and a
                    // caller reconciling a roster needs to tell those apart.
                    "changed": changed,
                })),
            )
                .into_response()
        }
        // Same split as enrolment: the admin's mistake carries its sentence,
        // everything else is ours and says nothing about the database.
        Err(OxyError::ValidationError(msg)) => {
            warn!(%org_id, "frontline standing refused: {msg}");
            // 404 for THIS refusal, matched by name — 400 for any other.
            //
            // Blanket-mapping the variant was right while the writer had exactly
            // one validation, and would have quietly reported the next one — a
            // bad status, a self-suspend guard — as "worker not found". Same
            // reasoning as `enrol` matching `IDENTIFIER_TAKEN` rather than a
            // literal: the two sides agree on a name so they cannot drift.
            let status = if msg == frontline::WORKER_NOT_FOUND {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, Json(serde_json::json!({ "error": msg }))).into_response()
        }
        Err(e) => {
            error!(%org_id, error = %e, "frontline standing failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "could not change the worker's standing" })),
            )
                .into_response()
        }
    }
}
