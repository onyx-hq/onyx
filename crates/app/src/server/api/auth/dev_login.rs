//! Dev-only sign-in bypass — `GET|POST /api/auth/dev-login`.
//!
//! Local development runs the production path (cloud mode, magic-link auth),
//! so anything driving a browser — Playwright MCP, a scratch Playwright
//! script, a headless CI probe — otherwise has to complete Google OAuth or
//! fish a token out of the `MAGIC_LINK_LOCAL_TEST` email preview. Neither is
//! scriptable. This endpoint mints exactly the session the real login flows
//! mint (`finalize_login` → JWT + the `oxy_session` cookie), for an identity
//! the operator pre-declared in the environment.
//!
//! Guard rails, in order of how much they matter:
//!
//! 1. **Off unless an allow-list resolves.** `OXY_DEV_LOGIN_EMAILS` supplies
//!    one in any build; in a **debug build only**, leaving it unset falls back
//!    to `OXY_GLOBAL_ADMINS` plus the seeded [`personas`], so a dev box needs
//!    no configuration at all. Disabled, every verb 404s — a deployment that
//!    never sets the var does not advertise the route.
//!
//!    Two guards make that fallback safe, along different axes:
//!
//!    - **`debug_assertions`** covers shipped vs local. `OXY_GLOBAL_ADMINS`
//!      **is** set in production — it seeds the `app_admins` table — so an
//!      unconditional alias would turn every real deployment into an
//!      unauthenticated Global-Admin sign-in: one
//!      `POST {"email": "<any staff address>"}` and the caller holds Oxy's ops
//!      tier. Release binaries (prod, and every Docker image) see only the
//!      explicit var. A release binary run locally still works; it just names
//!      the var, exactly as before.
//!    - **Loopback** covers this machine vs the network. `serve` binds
//!      `0.0.0.0` by default and `.env.example` ships an uncommented roster, so
//!      a fallback honored for any peer would make a plain `cargo run serve` on
//!      café or office wifi vend staff sessions to every device on the network
//!      — with nothing typed and nothing in the flow announcing it. Only the
//!      explicit var, which somebody deliberately wrote, is served off-box.
//!
//!    The distinction throughout is *deliberateness*: an operator who typed a
//!    list of identities gets what they asked for; an inferred list never
//!    leaves the machine that inferred it.
//! 2. **Only pre-declared identities.** The caller may name an email, or a
//!    persona (`as=member`) that resolves to one, but the address must already
//!    be in the allow-list; an unlisted address is a 403. So the worst a caller
//!    can do is become an identity the operator — or the inferred fallback,
//!    under both guards above — chose.
//! 3. **Loud.** Enabling it prints a warning at startup and logs a `warn!` on
//!    every issued session.
//! 4. **No drive-by sign-in.** Only `POST` sets the `SameSite=Lax` session
//!    cookie; `GET` returns the token in the body and nothing else, so a page
//!    a developer happens to visit cannot navigate them into a session.
//!
//! A dev box is cloud mode with non-prod secrets, so it is indistinguishable
//! from prod by `ServeMode` (see product-context.md) — the explicit env opt-in
//! is the gate, and a mode heuristic must not be added here.
//!
//! **Why not `#[cfg(debug_assertions)]` on the route** (considered, rejected):
//! it would make the bypass physically absent from shipped binaries, but
//! running a *released* binary locally is a supported dev path — `oxy start`
//! from an install, and every Docker image, is a release build. A compile-time
//! gate would make the documented workflow 404 with no explanation on exactly
//! those setups. The runtime gate is one deliberate env var either way.

mod account;
mod peer;
mod personas;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::LazyLock;

use axum::{
    extract,
    http::{HeaderMap, StatusCode},
    response::Json,
};

pub use peer::PeerAddr;

use oxy::database::client::establish_connection;

use super::dto::{AuthResponse, DevLoginRequest};
use super::ops::{finalize_login, is_valid_email_format, login_response};
use personas::{DevLoginRefusal, Target};

/// Comma-separated allow-list of sign-in identities. Setting it is what turns
/// the endpoint on; leaving it unset is what keeps it off everywhere else.
pub(crate) const DEV_LOGIN_EMAILS_ENV: &str = "OXY_DEV_LOGIN_EMAILS";

/// The staff roster that seeds `app_admins`. Read here **only** as a debug-build
/// convenience — see [`resolve_source`] for why that qualifier is load-bearing.
pub(crate) const GLOBAL_ADMINS_ENV: &str = "OXY_GLOBAL_ADMINS";

/// Where the allow-list came from. Not cosmetic: it decides who may *use* the
/// list, because only [`DevLoginSource::Explicit`] represents someone
/// deliberately turning the bypass on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DevLoginSource {
    /// `OXY_DEV_LOGIN_EMAILS` — an explicit opt-in, honored in every build.
    Explicit,
    /// `OXY_GLOBAL_ADMINS` plus the seeded personas, debug builds only.
    GlobalAdmins,
}

impl DevLoginSource {
    /// The variable to name in logs, so a reader never goes hunting for one
    /// they did not set.
    pub(crate) fn env_var(self) -> &'static str {
        match self {
            Self::Explicit => DEV_LOGIN_EMAILS_ENV,
            Self::GlobalAdmins => GLOBAL_ADMINS_ENV,
        }
    }

    /// Whether a session may only be issued to a loopback caller.
    ///
    /// The explicit var is a deliberate act — somebody typed a list of
    /// identities — so it is honored for any peer. That is the escape hatch
    /// for containers, remote dev boxes, and CI.
    ///
    /// The roster fallback is **not** a deliberate act: nobody typed
    /// anything, and `serve` binds `0.0.0.0` by default while `.env.example`
    /// ships an uncommented `OXY_GLOBAL_ADMINS`. Honoring those for a remote
    /// peer would make a plain `cargo run serve` on café or office wifi an
    /// unauthenticated global-admin vending machine for every device on the
    /// network — zero configuration performed, and nothing in the flow saying
    /// anything was enabled. Loopback keeps the zero-config win for the
    /// developer's own browser without ever leaving the machine.
    fn requires_loopback(self) -> bool {
        !matches!(self, Self::Explicit)
    }
}

/// The allow-list plus which variable produced it, so the startup banner can
/// name the right one instead of always blaming `OXY_DEV_LOGIN_EMAILS`.
struct DevLoginConfig {
    emails: Vec<String>,
    source: Option<DevLoginSource>,
    /// Parsed `OXY_GLOBAL_ADMINS` — what `as=staff` names. Empty on a release
    /// build, which never reads the roster for anything.
    roster: Vec<String>,
}

/// Parsed once per process, which matches the "set it and restart" contract the
/// docs state. Re-reading per request would also re-emit the malformed-entry
/// warning on every `GET /auth/config` — which every client hits on boot — so a
/// single typo would become permanent log noise.
static DEV_LOGIN: LazyLock<DevLoginConfig> = LazyLock::new(|| {
    build_config(
        std::env::var(DEV_LOGIN_EMAILS_ENV).ok(),
        std::env::var(GLOBAL_ADMINS_ENV).ok(),
        cfg!(debug_assertions),
    )
});

/// The whole config from the raw env values, with the build injected for the
/// same testability reason as [`resolve_source`].
///
/// The inferred list is `OXY_GLOBAL_ADMINS ∪ personas`: personas widen a list
/// the fallback already produced, under the same two guards, and never conjure
/// one — no usable roster (unset, or every entry malformed), no fallback. A
/// typed list is exactly what was typed.
fn build_config(
    explicit: Option<String>,
    global_admins: Option<String>,
    debug_build: bool,
) -> DevLoginConfig {
    let roster = if debug_build {
        parse_dev_login_emails(global_admins.as_deref(), Some(DevLoginSource::GlobalAdmins))
    } else {
        Vec::new()
    };
    let (raw, source) = resolve_source(explicit, global_admins, debug_build);
    let emails = match source {
        Some(DevLoginSource::GlobalAdmins) => personas::inferred_allow_list(&roster),
        _ => parse_dev_login_emails(raw.as_deref(), source),
    };
    DevLoginConfig {
        source: source.filter(|_| !emails.is_empty()),
        emails,
        roster,
    }
}

/// Which variable the allow-list came from, in order:
///
/// 1. `OXY_DEV_LOGIN_EMAILS` — the explicit opt-in, honored in every build.
///    Set-but-empty counts as "set", so `OXY_DEV_LOGIN_EMAILS=` is how you turn
///    the bypass **off** on a debug build without unsetting the staff roster.
/// 2. `OXY_GLOBAL_ADMINS` — debug builds only. A dev box already lists its staff
///    there, and a second identical list is one that goes stale. The pre-rename
///    `OXY_APP_ADMINS` is **not** a third rung: it is no longer read anywhere,
///    and `custom_apps_auth` says so at startup if it is still set.
///
/// `debug_build` is a parameter rather than a `cfg!` read inline so the release
/// behavior is testable from a debug test run — the case that matters most here
/// is precisely the one the test binary can't otherwise reach.
fn resolve_source(
    explicit: Option<String>,
    global_admins: Option<String>,
    debug_build: bool,
) -> (Option<String>, Option<DevLoginSource>) {
    if let Some(raw) = explicit {
        return (Some(raw), Some(DevLoginSource::Explicit));
    }
    if !debug_build {
        return (None, None);
    }
    // Never name a source we didn't actually read a value from, or the startup
    // banner would blame a variable nobody set.
    match global_admins {
        Some(raw) => (Some(raw), Some(DevLoginSource::GlobalAdmins)),
        None => (None, None),
    }
}

/// Whether the bypass is reachable *for this caller* — what `GET /auth/config`
/// must report, rather than the process-wide [`is_dev_login_enabled`].
///
/// `pub(crate)`, like everything else that reads the allow-list: nothing
/// outside this crate has a reason to ask, and an unreachable footgun beats a
/// signposted one.
///
/// The two have to agree or the 404 above buys nothing: `/auth/config` is
/// public, unauthenticated, and hit by every client on boot, so reporting a
/// flat `true` would tell the very off-box caller the 404 is hiding from that
/// there is a bypass here worth probing — and would render a Dev sign-in button
/// that can only 404 for them.
pub(crate) fn dev_login_reachable_by(peer: Option<SocketAddr>) -> bool {
    peer::reachable(is_dev_login_enabled(), dev_login_is_loopback_only(), peer)
}

/// The configured identities, normalized and validated. Empty ⇒ disabled.
pub(crate) fn dev_login_emails() -> &'static [String] {
    &DEV_LOGIN.emails
}

/// The env var that enabled the bypass, for the startup banner. `None` when off.
pub(crate) fn dev_login_source() -> Option<&'static str> {
    DEV_LOGIN.source.map(DevLoginSource::env_var)
}

/// Whether the active allow-list is only honored for loopback callers, so the
/// startup banner can say which of the two very different things is true.
pub(crate) fn dev_login_is_loopback_only() -> bool {
    DEV_LOGIN
        .source
        .is_some_and(DevLoginSource::requires_loopback)
}

/// Whether an allow-list resolved at all — **process-wide, ignoring who is
/// asking**. For the startup banner, and as one input to
/// [`dev_login_reachable_by`].
///
/// Not for a request path: an inferred roster is honored on loopback only, so
/// answering a caller from this alone re-opens the leak
/// [`dev_login_reachable_by`] exists to close. That function is what
/// `GET /auth/config` reports.
pub(crate) fn is_dev_login_enabled() -> bool {
    !dev_login_emails().is_empty()
}

fn parse_dev_login_emails(raw: Option<&str>, source: Option<DevLoginSource>) -> Vec<String> {
    // Name the variable the entries actually came from: a typo in
    // OXY_GLOBAL_ADMINS blaming OXY_DEV_LOGIN_EMAILS sends the reader looking
    // for a var they never set.
    let label = source.map_or(DEV_LOGIN_EMAILS_ENV, DevLoginSource::env_var);
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_lowercase)
        .filter(|entry| {
            let valid = is_valid_email_format(entry);
            if !valid {
                tracing::warn!("{label}: ignoring malformed entry {entry:?}");
            }
            valid
        })
        .collect()
}

/// Pick the identity to sign in as: the caller's choice when it is on the
/// allow-list, otherwise the first configured entry when the caller named
/// none. `None` means "asked for an address we will not issue".
fn resolve_dev_login_email(allowed: &[String], requested: Option<&str>) -> Option<String> {
    match requested.map(str::trim).filter(|email| !email.is_empty()) {
        None => allowed.first().cloned(),
        Some(requested) => {
            let requested = requested.to_lowercase();
            allowed.iter().find(|email| **email == requested).cloned()
        }
    }
}

/// `404` when the bypass is off *or* when an un-typed roster fallback is reached
/// from off-box — decided before anything else, so no later refusal (a `400`
/// naming the valid personas, say) can tell an unserved caller the route exists.
///
/// The off-box case is a `404`, not a `403`, for the same reason "disabled" is:
/// from that peer's side the endpoint simply does not exist, and saying
/// "forbidden" would confirm there is a bypass here worth probing.
///
/// Caveat: `peer` is the socket's remote address, so a debug build behind a
/// reverse proxy sees the proxy and every caller looks local. Nothing in the
/// supported dev flow puts a proxy in front of `serve`, and release builds
/// never reach the fallback at all — but do not extend this check to trust a
/// forwarded-for header, which is caller-controlled.
fn gate(
    allowed: &[String],
    source: Option<DevLoginSource>,
    peer_is_loopback: bool,
) -> Result<(), StatusCode> {
    if allowed.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    if source.is_some_and(DevLoginSource::requires_loopback) && !peer_is_loopback {
        tracing::warn!(
            "dev-login: refused a non-loopback caller — the allow-list came from \
             {} rather than an explicit {DEV_LOGIN_EMAILS_ENV}, so it is honored \
             on this machine only. Set {DEV_LOGIN_EMAILS_ENV} to serve other hosts.",
            source.map_or("(none)", DevLoginSource::env_var)
        );
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(())
}

/// The [`gate`], then `403` when the caller named an address the operator did
/// not declare.
fn resolve_or_refuse(
    allowed: &[String],
    requested: Option<&str>,
    source: Option<DevLoginSource>,
    peer_is_loopback: bool,
) -> Result<String, StatusCode> {
    gate(allowed, source, peer_is_loopback)?;
    resolve_dev_login_email(allowed, requested).ok_or_else(|| {
        tracing::warn!(
            "dev-login: refused {:?} — not in {}",
            requested.unwrap_or_default(),
            source.map_or(DEV_LOGIN_EMAILS_ENV, DevLoginSource::env_var)
        );
        StatusCode::FORBIDDEN
    })
}

/// Everything decided before any database work, in the order that keeps the
/// route invisible to callers it does not serve: `404` (the gate), then `400`
/// (both `email` and `as`, or an unknown persona), then `409` (`as=staff` with
/// no roster), then `403` (not on the allow-list).
fn resolve_request<'a>(
    config: &DevLoginConfig,
    req: &'a DevLoginRequest,
    peer_is_loopback: bool,
) -> Result<(Target<'a>, String), DevLoginRefusal> {
    gate(&config.emails, config.source, peer_is_loopback)?;
    let target = personas::parse_target(req.email.as_deref(), req.persona.as_deref())?;
    let requested = target.requested_email(&config.roster)?;
    let email = resolve_or_refuse(
        &config.emails,
        requested.as_deref(),
        config.source,
        peer_is_loopback,
    )?;
    Ok((target, email))
}

/// How the minted session is handed back.
///
/// `POST` gets the `oxy_session` cookie, like every real login flow. `GET`
/// deliberately does not: a top-level navigation IS a GET and the cookie is
/// `SameSite=Lax`, so any page a developer visits while a local server runs
/// could otherwise plant a session in their browser with a bare
/// `location = 'http://localhost:3000/api/auth/dev-login'`. GET exists for
/// scripts that want the token out of the body, and a script has no use for
/// the cookie — so withholding it costs nothing and closes login-CSRF.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SessionDelivery {
    CookieAndBody,
    BodyOnly,
}

/// `POST /auth/dev-login` — body `{"email": "..."}`, `{"as": "member"}`, or `{}`
/// for the first configured identity. Used by the web-app's `/dev-login` page;
/// sets the session cookie.
pub async fn dev_login(
    peer: PeerAddr,
    headers: HeaderMap,
    extract::Json(req): extract::Json<DevLoginRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), DevLoginRefusal> {
    issue_dev_session(&headers, &req, SessionDelivery::CookieAndBody, peer).await
}

/// `GET /auth/dev-login?email=...` (or `?as=<persona>`) — the token in the body,
/// for tools that would rather `curl` than run JavaScript. No `Set-Cookie`; see
/// [`SessionDelivery`].
pub async fn dev_login_get(
    peer: PeerAddr,
    headers: HeaderMap,
    extract::Query(req): extract::Query<DevLoginRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), DevLoginRefusal> {
    issue_dev_session(&headers, &req, SessionDelivery::BodyOnly, peer).await
}

async fn issue_dev_session(
    headers: &HeaderMap,
    req: &DevLoginRequest,
    delivery: SessionDelivery,
    peer: PeerAddr,
) -> Result<(HeaderMap, Json<AuthResponse>), DevLoginRefusal> {
    let (target, email) = resolve_request(&DEV_LOGIN, req, peer.is_loopback())?;

    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("dev-login: failed to establish database connection: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user = account::find_or_provision(&email, target.persona(), &connection).await?;

    tracing::warn!(
        "dev-login: issued a session for {email} without authentication ({} is set \
         — never set it on a shared deployment)",
        DEV_LOGIN
            .source
            .map_or(DEV_LOGIN_EMAILS_ENV, DevLoginSource::env_var)
    );

    let (token, user_info, orgs) = finalize_login(user, &connection).await?;
    Ok(match delivery {
        SessionDelivery::CookieAndBody => login_response(headers, token, user_info, orgs),
        SessionDelivery::BodyOnly => (
            HeaderMap::new(),
            Json(AuthResponse {
                token,
                user: user_info,
                orgs,
            }),
        ),
    })
}
