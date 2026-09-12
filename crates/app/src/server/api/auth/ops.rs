use axum::{
    http::{HeaderMap, StatusCode},
    response::Json,
};
use chrono::{Duration, Utc};
use entity::{prelude::Users, users, users::UserStatus};
use governor::{
    DefaultKeyedRateLimiter, Quota, RateLimiter,
    clock::{Clock, DefaultClock},
};
use handlebars::Handlebars;
use jsonwebtoken::{DecodingKey, Validation, decode};
use once_cell::sync::Lazy;
use oxy::config::auth::MagicLinkAuth;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use std::num::NonZeroU32;
use url::Url;
use uuid::Uuid;

use oxy::database::errors::is_unique_violation;
use oxy::{
    config::constants::AUTHENTICATION_SECRET_KEY,
    database::{client::establish_connection, filters::UserQueryFilterExt},
};
use oxy_auth::constants::SESSION_COOKIE_NAME;
use oxy_shared::errors::OxyError;

use super::dto::*;
use super::handlers::create_auth_token;

// ─── Magic Link Rate Limiter ────────────────────────────────────────────────
//
// Token-bucket rate limiter (governor). State is in-process only and resets
// on restart — intentional, no external dependency required.
// Checked before the allowlist so timing cannot reveal allowlist membership.
//
// Limit: 5 requests per email per hour.

static MAGIC_LINK_RATE_LIMITER: Lazy<DefaultKeyedRateLimiter<String>> =
    Lazy::new(|| RateLimiter::keyed(Quota::per_hour(NonZeroU32::new(5).expect("5 > 0"))));

/// Returns `None` if the request is allowed, or `Some(seconds)` with the wait
/// time until the next request is permitted.
pub(super) fn check_magic_link_rate_limit(email: &str) -> Option<u64> {
    match MAGIC_LINK_RATE_LIMITER.check_key(&email.to_lowercase()) {
        Ok(()) => None,
        Err(not_until) => {
            let wait = not_until.wait_time_from(DefaultClock::default().now());
            Some(wait.as_secs().max(1))
        }
    }
}

/// The session window: how long a minted JWT stays valid, and how long the
/// cookie carrying it lives.
///
/// ONE constant, read by both `create_auth_token` and [`build_session_cookie`],
/// because the two drifting apart is a real bug rather than an untidiness: a
/// cookie outliving its JWT leaves the browser holding a dead credential and
/// 401ing on every call instead of re-prompting, which is exactly what
/// `frontline.rs` hit when the shift TTL was enforced by the JWT `exp` alone.
pub(super) const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Build the `Set-Cookie` header value for the session cookie that wraps
/// the existing JWT. Carrying the JWT in a `.oxygen-hq.com`-scoped cookie lets
/// the external auth proxy (`/api/auth/check`) authenticate browser
/// traffic on `<app>.oxygen-hq.com` subdomains using the same credential the
/// web-app already uses on the `Authorization` header.
///
/// `Domain` is configurable via the `OXY_SESSION_COOKIE_DOMAIN` env var
/// (set to `.oxygen-hq.com` in prod). When unset, the cookie is host-only —
/// fine for local dev where there are no subdomains to gate.
pub fn build_session_cookie(jwt: &str, secure: bool) -> String {
    build_session_cookie_with_max_age(jwt, secure, SESSION_TTL_SECS)
}

/// As [`build_session_cookie`], with the browser's copy expiring alongside the
/// token rather than on the default schedule.
///
/// The two lifetimes have to agree. A cookie that outlives its JWT leaves the
/// client presenting a credential the server will refuse — which does not read
/// as "signed out", it reads as every request failing while the UI still thinks
/// there is a session. That is the whole difference between a kiosk showing the
/// name picker the next morning and one showing 401s.
pub fn build_session_cookie_with_max_age(jwt: &str, secure: bool, max_age_secs: i64) -> String {
    let mut parts = vec![
        format!("{SESSION_COOKIE_NAME}={jwt}"),
        "Path=/".to_string(),
        format!("Max-Age={max_age_secs}"),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];
    if secure {
        parts.push("Secure".to_string());
    }
    if let Ok(domain) = std::env::var("OXY_SESSION_COOKIE_DOMAIN") {
        let domain = domain.trim();
        if !domain.is_empty() {
            parts.push(format!("Domain={domain}"));
        }
    }
    parts.join("; ")
}

/// Build the `Set-Cookie` header that clears the session cookie. Sent on
/// logout. Must mirror the same `Domain` and `Path` as `build_session_cookie`
/// so the browser actually overwrites the existing cookie.
pub(crate) fn clear_session_cookie() -> String {
    let mut parts = vec![
        format!("{SESSION_COOKIE_NAME}="),
        "Path=/".to_string(),
        "Max-Age=0".to_string(),
        "HttpOnly".to_string(),
        "SameSite=Lax".to_string(),
    ];
    if let Ok(domain) = std::env::var("OXY_SESSION_COOKIE_DOMAIN") {
        let domain = domain.trim();
        if !domain.is_empty() {
            parts.push(format!("Domain={domain}"));
        }
    }
    parts.join("; ")
}

/// Should the session cookie carry the `Secure` attribute?
///
/// Three signals are consulted, in priority order:
///
/// 1. `OXY_SESSION_COOKIE_FORCE_SECURE=1` env var — explicit override.
/// 2. `OXY_SESSION_COOKIE_DOMAIN` set to a non-localhost value — if you're
///    scoping to a real domain (e.g. `.oxygen-hq.com`) the cookie must be Secure;
///    browsers ignore domain-scoped cookies on plain HTTP anyway.
/// 3. The `X-Forwarded-Proto` request header (set by the ingress) — `https`
///    means the original client request was HTTPS.
///
/// In local dev none of these are set so we default to `false`, allowing the
/// cookie to work on `http://localhost`.
///
/// `pub` so every path that mints a session answers this the same way. It is not
/// derivable from the serve mode: a dev box is cloud mode with non-prod secrets
/// and is served over plain `http://localhost`, so `!process_is_local()` sets
/// `Secure` there and the browser silently discards the cookie — a sign-in that
/// returns 200 and does not stick.
pub fn is_request_secure(headers: &HeaderMap) -> bool {
    if std::env::var("OXY_SESSION_COOKIE_FORCE_SECURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }
    // Auto-enable Secure when a real domain scope is configured.
    if let Ok(domain) = std::env::var("OXY_SESSION_COOKIE_DOMAIN") {
        let domain = domain.trim().to_ascii_lowercase();
        if !domain.is_empty() && !domain.contains("localhost") && !domain.contains("127.0.0.1") {
            return true;
        }
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// Parse a `return_to` URL into its lowercase scheme and host. Rejects any
/// scheme other than http/https (mailto:, javascript:, file:, …) and
/// anything malformed. http:// is allowed at parse time so local dev
/// (`http://localhost:3000/customer-apps/...`) works; production gating still
/// happens via [`host_in_session_zone`] which only matches the configured
/// cookie zone.
fn parse_return_to_host(url: &str) -> Option<(String, String)> {
    let parsed = Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let host = parsed.host_str().map(|h| h.to_ascii_lowercase())?;
    Some((scheme, host))
}

/// Static check: is `host` inside the configured session-cookie zone? The
/// session-cookie domain doubles as the trust boundary — anything that
/// shares the cookie also shares the trust.
fn host_in_session_zone(host: &str) -> bool {
    let Ok(zone) = std::env::var("OXY_SESSION_COOKIE_DOMAIN") else {
        return false;
    };
    let zone = zone.trim().trim_start_matches('.').to_ascii_lowercase();
    if zone.is_empty() {
        return false;
    }
    host == zone || host.ends_with(&format!(".{zone}"))
}

/// Check used by the magic-link request flow and by the standalone
/// validator endpoint. Allows any URL on a subdomain of the configured
/// session-cookie zone. A localhost escape hatch exists for local dev but
/// is *explicitly opt-in* via `OXY_AUTH_ALLOW_LOCALHOST_RETURN=1` — it
/// must NOT silently activate just because `OXY_SESSION_COOKIE_DOMAIN`
/// happens to be unset on a misconfigured production deploy, since that
/// would turn `?return_to=http://localhost/...` into an open redirect.
pub(crate) fn validate_return_to_url(url: &str) -> bool {
    let Some((scheme, host)) = parse_return_to_host(url) else {
        return false;
    };
    if host_in_session_zone(&host) {
        return true;
    }
    if (host == "localhost" || host == "127.0.0.1")
        && (scheme == "http" || scheme == "https")
        && allow_localhost_return()
    {
        return true;
    }
    false
}

fn allow_localhost_return() -> bool {
    std::env::var("OXY_AUTH_ALLOW_LOCALHOST_RETURN")
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Build the `(HeaderMap, Json<AuthResponse>)` tuple returned by every
/// successful login finalize. Centralized so all four providers
/// (Google/GitHub/Okta/magic-link) emit the cookie identically.
pub(super) fn login_response(
    request_headers: &HeaderMap,
    token: String,
    user: UserInfo,
    orgs: Vec<OrgInfo>,
) -> (HeaderMap, Json<AuthResponse>) {
    let mut response_headers = HeaderMap::new();
    let cookie = build_session_cookie(&token, is_request_secure(request_headers));
    if let Ok(value) = cookie.parse() {
        response_headers.insert(axum::http::header::SET_COOKIE, value);
    } else {
        tracing::error!("Failed to build session cookie header value");
    }
    (response_headers, Json(AuthResponse { token, user, orgs }))
}

// ─── OAuth state (CSRF defense) ────────────────────────────────────────────
//
// The frontend fetches a signed state token via `POST /auth/oauth/state`,
// echoes it through the OAuth provider redirect, and sends it back with the
// code. We verify the HMAC + short expiry before exchanging the code, which
// prevents an attacker from splicing a captured code into another user's
// session.

/// Time-to-live for an OAuth state token. Long enough to complete the round
/// trip through an interactive provider, short enough to limit replay.
pub(super) const OAUTH_STATE_TTL_SECS: i64 = 10 * 60;

pub(super) const OAUTH_STATE_PURPOSE: &str = "oauth-state";

pub(super) fn verify_oauth_state(state: &str) -> Result<(), StatusCode> {
    let validation = Validation::default();
    let data = decode::<OAuthStateClaims>(
        state,
        &DecodingKey::from_secret(AUTHENTICATION_SECRET_KEY.as_bytes()),
        &validation,
    )
    .map_err(|e| {
        tracing::warn!("OAuth state rejected: {}", e);
        StatusCode::UNAUTHORIZED
    })?;
    if data.claims.purpose != OAUTH_STATE_PURPOSE {
        tracing::warn!("OAuth state has wrong purpose claim");
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

/// Exchange a GitHub OAuth authorization code for user profile info and the raw
/// GitHub access token. The token is returned so callers can store it for later
/// use (e.g. listing GitHub App installations without a second sign-in).
/// Base origin for an OAuth `redirect_uri` at token-exchange time.
///
/// Local multi-instance dev: when several instances share one registered
/// redirect URI via the OAuth bounce proxy, the token-exchange `redirect_uri`
/// must equal the one the provider saw at authorize time (the proxy origin),
/// not this instance's request origin. `OXY_OAUTH_REDIRECT_ORIGIN` overrides it;
/// unset → the request origin, exactly as before.
fn oauth_redirect_base(base_url: &str) -> String {
    std::env::var("OXY_OAUTH_REDIRECT_ORIGIN")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| base_url.to_string())
}

pub(super) async fn exchange_github_code_for_user_info(
    code: &str,
    base_url: &str,
) -> Result<OAuthUserInfo, OxyError> {
    let client_id = std::env::var("GITHUB_CLIENT_ID")
        .map_err(|_| OxyError::ConfigurationError("GITHUB_CLIENT_ID not configured".to_string()))?;
    let client_secret = std::env::var("GITHUB_CLIENT_SECRET").map_err(|_| {
        OxyError::ConfigurationError("GITHUB_CLIENT_SECRET not configured".to_string())
    })?;

    let redirect_uri = format!("{}/github/callback", oauth_redirect_base(base_url));

    let client = reqwest::Client::builder()
        .user_agent("Oxy/1.0")
        .build()
        .map_err(|e| OxyError::RuntimeError(e.to_string()))?;

    // Exchange the authorization code for an access token.
    let token_response = client
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("GitHub token request failed: {e}")))?;

    if !token_response.status().is_success() {
        return Err(OxyError::RuntimeError(format!(
            "GitHub token exchange error: {}",
            token_response.status()
        )));
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: Option<String>,
        error: Option<String>,
    }

    let token_data: TokenResponse = token_response.json().await.map_err(|e| {
        OxyError::RuntimeError(format!("Failed to parse GitHub token response: {e}"))
    })?;

    if let Some(err) = token_data.error {
        return Err(OxyError::RuntimeError(format!("GitHub OAuth error: {err}")));
    }

    let access_token = token_data.access_token.ok_or_else(|| {
        OxyError::RuntimeError("GitHub token response missing access_token".to_string())
    })?;

    let user_resp: GitHubUserInfo = client
        .get("https://api.github.com/user")
        .bearer_auth(&access_token)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("GitHub /user request failed: {e}")))?
        .json()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("Failed to parse GitHub user: {e}")))?;

    // Use the profile email if set; otherwise fetch the primary verified email.
    let email = if let Some(e) = user_resp.email.filter(|e| !e.is_empty()) {
        e
    } else {
        let emails: Vec<GitHubEmailEntry> = client
            .get("https://api.github.com/user/emails")
            .bearer_auth(&access_token)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
            .map_err(|e| {
                OxyError::RuntimeError(format!("GitHub /user/emails request failed: {e}"))
            })?
            .json()
            .await
            .map_err(|e| OxyError::RuntimeError(format!("Failed to parse GitHub emails: {e}")))?;

        emails
            .into_iter()
            .find(|e| e.primary && e.verified)
            .map(|e| e.email)
            .ok_or_else(|| {
                OxyError::RuntimeError(
                    "No verified primary email found on GitHub account".to_string(),
                )
            })?
    };

    let name = user_resp.name.unwrap_or_else(|| user_resp.login.clone());

    Ok(OAuthUserInfo {
        email,
        name,
        picture: user_resp.avatar_url,
    })
}

/// Insert a new user, handling the race condition where another request may have
/// created the same user concurrently.
pub(super) async fn insert_user_or_fetch_existing(
    new_user: users::ActiveModel,
    email: &str,
    connection: &DatabaseConnection,
) -> Result<users::Model, StatusCode> {
    match new_user.insert(connection).await {
        Ok(user) => Ok(user),
        Err(e) if is_unique_violation(&e) => {
            // Race condition: another request created the user concurrently.
            // Fetch the existing user.
            Users::find()
                .filter_by_email(email)
                .one(connection)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to query user after unique violation: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .ok_or_else(|| {
                    tracing::error!(
                        "User '{}' not found after unique constraint violation",
                        email
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                })
        }
        Err(e) => {
            tracing::error!("Failed to create user: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Build the auth token, the `UserInfo` payload, and the user's org memberships.
///
/// Called after every login (Google, Okta, GitHub, magic link verify). Role
/// and admin status are per-org and appear on `OrgInfo` below — not on
/// `UserInfo` — so that callers never have to reason about a global role.
pub(super) async fn finalize_login(
    user: users::Model,
    connection: &DatabaseConnection,
) -> Result<(String, UserInfo, Vec<OrgInfo>), StatusCode> {
    let token = create_auth_token(user.clone()).await.map_err(|e| {
        tracing::error!("Failed to create auth token: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // Reported on the payload, not decided here — the flag door, not a ring.
    let standing = crate::server::authz::globals::platform_standing(
        connection,
        user.email.as_deref().unwrap_or(""),
    )
    .await;
    let user_info = UserInfo {
        id: user.id.to_string(),
        email: user.label().to_string(),
        name: user.name.clone(),
        picture: user.picture.clone(),
        is_owner: standing.is_global_owner,
        is_app_admin: standing.is_global_admin,
    };

    use entity::org_members::{self, Entity as OrgMembers};
    use entity::organizations::{self, Entity as Organizations};

    let memberships = OrgMembers::find()
        .filter(org_members::Column::UserId.eq(user.id))
        .all(connection)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query org memberships: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let org_ids: Vec<uuid::Uuid> = memberships.iter().map(|m| m.org_id).collect();
    let orgs = if org_ids.is_empty() {
        vec![]
    } else {
        let org_rows = Organizations::find()
            .filter(organizations::Column::Id.is_in(org_ids))
            .all(connection)
            .await
            .map_err(|e| {
                tracing::error!("Failed to query organizations: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        org_rows
            .iter()
            .filter_map(|org| {
                let membership = memberships.iter().find(|m| m.org_id == org.id)?;
                Some(OrgInfo {
                    id: org.id.to_string(),
                    name: org.name.clone(),
                    slug: org.slug.clone(),
                    role: membership.role.as_str().to_string(),
                })
            })
            .collect()
    };

    Ok((token, user_info, orgs))
}

pub fn extract_base_url_from_headers(headers: &HeaderMap) -> String {
    pin_org_subdomain_to_app_host(extract_base_url_raw(headers))
}

fn extract_base_url_raw(headers: &HeaderMap) -> String {
    if let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) {
        let origin = origin.trim_end_matches('/');
        if origin.starts_with("http://") || origin.starts_with("https://") {
            return origin.to_string();
        }
        // Some reverse proxies/CDNs may forward Origin without scheme.
        // Default to https for non-localhost hosts.
        let scheme = if origin.starts_with("localhost") || origin.starts_with("127.0.0.1") {
            "http"
        } else {
            "https"
        };
        return format!("{scheme}://{origin}");
    }

    if let Some(referer) = headers.get("referer").and_then(|h| h.to_str().ok())
        && let Ok(url) = Url::parse(referer)
        && let Some(host) = url.host_str()
    {
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        return format!("{}://{}{}", url.scheme(), host, port);
    }
    "http://localhost:3000".to_string()
}

/// Centralized auth: OAuth callbacks and magic-link emails must resolve to a
/// single host (the app host) so providers need exactly one registered
/// callback. With the centralized-auth bounce in place, an auth request
/// already arrives on the app host — but if one ever lands on a bare org
/// subdomain (`pokehouse.oxygen-hq.com`), pin the base URL back to the app
/// host rather than echoing the subdomain. Non-subdomain hosts (app host,
/// custom-branded, localhost) pass through unchanged.
fn pin_org_subdomain_to_app_host(base: String) -> String {
    let Ok(url) = Url::parse(&base) else {
        return base;
    };
    let Some(host) = url.host_str() else {
        return base;
    };
    if oxy_app_core::org_host_dispatch::parse_org_subdomain(host).is_some()
        && let Some(app_host) = oxy_app_core::custom_apps_host_dispatch::admin_base_url()
    {
        return app_host;
    }
    base
}

pub(super) async fn exchange_google_code_for_user_info(
    code: &str,
    base_url: &str,
) -> Result<OAuthUserInfo, OxyError> {
    let auth_config = oxy::config::oxy::get_oxy_config()
        .ok()
        .and_then(|config| config.authentication);

    let google_config = auth_config.and_then(|auth| auth.google).ok_or_else(|| {
        OxyError::ConfigurationError("Google OAuth configuration not found".to_string())
    })?;

    let client = reqwest::Client::new();

    let redirect_uri = format!("{}/auth/google/callback", oauth_redirect_base(base_url));

    let client_secret = google_config.client_secret;

    let token_request = serde_json::json!({
        "client_id": google_config.client_id,
        "client_secret": client_secret,
        "code": code,
        "grant_type": "authorization_code",
        "redirect_uri": redirect_uri
    });

    // Note: Google supports application/json for token exchange (non-standard but accepted)
    // Standard OAuth 2.0 requires application/x-www-form-urlencoded
    let token_response = client
        .post("https://oauth2.googleapis.com/token")
        .header("Content-Type", "application/json")
        .json(&token_request)
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Failed to send token request to Google: {}", e);
            OxyError::ConfigurationError(format!("Failed to exchange code for token: {e}"))
        })?;

    let status = token_response.status();
    if !status.is_success() {
        let error_body = token_response.text().await.unwrap_or_default();
        tracing::error!(
            "Google token exchange failed with status {}: {}",
            status,
            error_body
        );
        return Err(OxyError::ConfigurationError(format!(
            "Google token exchange failed with status {}: {}",
            status, error_body
        )));
    }

    let token_data: serde_json::Value = token_response.json().await.map_err(|e| {
        tracing::error!("Failed to parse Google token response: {}", e);
        OxyError::ConfigurationError(format!("Failed to parse token response: {e}"))
    })?;

    let access_token = token_data["access_token"]
        .as_str()
        .ok_or_else(|| OxyError::ConfigurationError("No access token in response".to_string()))?;

    let user_info_response = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Failed to send userinfo request to Google: {}", e);
            OxyError::ConfigurationError(format!("Failed to get user info: {e}"))
        })?;

    let status = user_info_response.status();
    if !status.is_success() {
        let error_body = user_info_response.text().await.unwrap_or_default();
        tracing::error!(
            "Google userinfo request failed with status {}: {}",
            status,
            error_body
        );
        return Err(OxyError::ConfigurationError(format!(
            "Google userinfo request failed with status {}: {}",
            status, error_body
        )));
    }

    let user_info: OAuthUserInfo = user_info_response.json().await.map_err(|e| {
        tracing::error!("Failed to parse Google userinfo response: {}", e);
        OxyError::ConfigurationError(format!("Failed to parse user info: {e}"))
    })?;

    Ok(user_info)
}

pub(super) async fn exchange_okta_code_for_user_info(
    code: &str,
    base_url: &str,
) -> Result<OktaUserInfo, OxyError> {
    let auth_config = oxy::config::oxy::get_oxy_config()
        .ok()
        .and_then(|config| config.authentication);

    let okta_config = auth_config.and_then(|auth| auth.okta).ok_or_else(|| {
        OxyError::ConfigurationError("Okta OAuth configuration not found".to_string())
    })?;

    let client = reqwest::Client::new();

    let redirect_uri = format!("{base_url}/auth/okta/callback");

    let client_secret = okta_config.client_secret;
    let okta_domain = okta_config.domain;

    // Exchange authorization code for tokens
    // OAuth 2.0 requires application/x-www-form-urlencoded for token requests
    let token_params = [
        ("client_id", okta_config.client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("code", code),
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_uri.as_str()),
    ];

    // Use org authorization server (matches /oauth2/v1/authorize from frontend)
    let token_url = format!("https://{}/oauth2/v1/token", okta_domain);

    let token_response = client
        .post(&token_url)
        .form(&token_params)
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Failed to send token request to Okta: {}", e);
            OxyError::ConfigurationError(format!("Failed to exchange code for token: {e}"))
        })?;

    let status = token_response.status();
    if !status.is_success() {
        let error_body = token_response.text().await.unwrap_or_default();
        tracing::error!(
            "Okta token exchange failed with status {}: {}",
            status,
            error_body
        );
        return Err(OxyError::ConfigurationError(format!(
            "Okta token exchange failed with status {}: {}",
            status, error_body
        )));
    }

    let token_data: serde_json::Value = token_response.json().await.map_err(|e| {
        tracing::error!("Failed to parse Okta token response: {}", e);
        OxyError::ConfigurationError(format!("Failed to parse token response: {e}"))
    })?;

    let access_token = token_data["access_token"]
        .as_str()
        .ok_or_else(|| OxyError::ConfigurationError("No access token in response".to_string()))?;

    // Get user info using the access token (use org authorization server)
    let userinfo_url = format!("https://{}/oauth2/v1/userinfo", okta_domain);

    let user_info_response = client
        .get(&userinfo_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("Failed to send userinfo request to Okta: {}", e);
            OxyError::ConfigurationError(format!("Failed to get user info: {e}"))
        })?;

    let status = user_info_response.status();
    if !status.is_success() {
        let error_body = user_info_response.text().await.unwrap_or_default();
        tracing::error!(
            "Okta userinfo request failed with status {}: {}",
            status,
            error_body
        );
        return Err(OxyError::ConfigurationError(format!(
            "Okta userinfo request failed with status {}: {}",
            status, error_body
        )));
    }

    let user_info: OktaUserInfo = user_info_response.json().await.map_err(|e| {
        tracing::error!("Failed to parse Okta userinfo response: {}", e);
        OxyError::ConfigurationError(format!("Failed to parse user info: {e}"))
    })?;

    Ok(user_info)
}

// ─── Magic Link ────────────────────────────────────────────────────────────

/// Basic RFC-5321-bounded email format check. Not exhaustive, but filters out
/// obviously malformed inputs before they reach the DB or SES.
pub(super) fn is_valid_email_format(email: &str) -> bool {
    if email.len() > 254 {
        return false;
    }
    // split_once splits at the first '@'; rejecting any additional '@' in the
    // domain part enforces exactly one '@' (RFC 5321 §4.1.2).
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }
    // Domain must have at least one '.' with non-empty labels on both sides.
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2 && labels.iter().all(|l| !l.is_empty())
}

fn is_email_allowed(email: &str, config: &MagicLinkAuth) -> bool {
    // email is already lowercased at ingestion; normalize config values too so
    // operators can write "Gmail.com" or "gmail.com" interchangeably.
    for domain in &config.blocked_domains {
        if email.ends_with(&format!("@{}", domain.to_lowercase())) {
            return false;
        }
    }
    if !config.allowed_emails.is_empty() {
        return config
            .allowed_emails
            .iter()
            .any(|e| e.eq_ignore_ascii_case(email));
    }
    true
}

pub(super) async fn request_magic_link_inner(
    headers: HeaderMap,
    req: MagicLinkRequest,
) -> Result<Json<MessageResponse>, StatusCode> {
    let auth_config = oxy::config::oxy::get_oxy_config()
        .ok()
        .and_then(|c| c.authentication)
        .and_then(|a| a.magic_link);

    let magic_link_config = auth_config.ok_or_else(|| {
        tracing::error!("Magic link auth not configured");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Always return 200 — don't leak whether email is allowed
    if !is_email_allowed(&req.email, &magic_link_config) {
        tracing::info!(
            "Magic link requested for non-allowlisted email: {}",
            req.email
        );
        return Ok(Json(MessageResponse {
            message: "If your email is eligible, a sign-in link has been sent.".to_string(),
        }));
    }

    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let existing = Users::find()
        .filter_by_email(&req.email)
        .one(&connection)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query user: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Resolve the user model — either the existing one or a freshly inserted one.
    // We capture the model here so we can reuse it for the token update below
    // without an extra DB round-trip.
    let user_for_update = match existing {
        Some(u) if u.status == UserStatus::Deleted => {
            // Silently succeed — don't reveal account status
            return Ok(Json(MessageResponse {
                message: "If your email is eligible, a sign-in link has been sent.".to_string(),
            }));
        }
        Some(u) => u, // existing active user — reuse model directly
        None => {
            // Auto-create new user
            let name = req.email.split('@').next().unwrap_or("User").to_string();
            let new_user = users::ActiveModel {
                id: Set(Uuid::new_v4()),
                email: Set(Some(req.email.clone())),
                name: Set(name),
                picture: Set(None),
                email_verified: Set(false),
                magic_link_token: Set(None),
                magic_link_token_expires_at: Set(None),
                status: Set(UserStatus::Active),
                created_at: sea_orm::ActiveValue::NotSet,
                last_login_at: sea_orm::ActiveValue::NotSet,
            };
            match new_user.insert(&connection).await {
                Ok(inserted) => inserted,
                Err(e) if is_unique_violation(&e) => {
                    // Race condition — another request created the user concurrently.
                    Users::find()
                        .filter_active_by_email(&req.email)
                        .one(&connection)
                        .await
                        .map_err(|e| {
                            tracing::error!("Failed to query user after race condition: {}", e);
                            StatusCode::INTERNAL_SERVER_ERROR
                        })?
                        .ok_or_else(|| {
                            tracing::error!("User not found after unique violation: {}", req.email);
                            StatusCode::INTERNAL_SERVER_ERROR
                        })?
                }
                Err(e) => {
                    tracing::error!("Failed to create user: {}", e);
                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }
            }
        }
    };

    // Generate 256-bit random token
    let token_bytes: [u8; 32] = rand::random();
    let token = hex::encode(token_bytes);
    let expires_at = Utc::now() + Duration::minutes(15);

    // Update user row with token + expiry — reuses the model from above, no extra query.
    let mut active: users::ActiveModel = user_for_update.into();
    active.magic_link_token = Set(Some(token.clone()));
    active.magic_link_token_expires_at = Set(Some(expires_at.into()));
    active.update(&connection).await.map_err(|e| {
        tracing::error!("Failed to save magic link token: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Send email async
    let base_url = extract_base_url_from_headers(&headers);

    // Validate return_to before stuffing it into the email. The web-app
    // performs the actual redirect, but if the proxy hands us a malicious
    // URL we don't want to embed it in the outbound link at all.
    let return_to = match req.return_to.as_deref() {
        Some(url) if validate_return_to_url(url) => Some(url.to_string()),
        Some(url) => {
            tracing::warn!("rejecting return_to URL outside allowlist: {url}");
            None
        }
        None => None,
    };

    // Log the magic link URL at debug level only — the token is a session credential
    // and must not appear in production log aggregators.
    tracing::debug!(
        "Magic link for {}: {}/auth/magic-link/callback?token={}",
        req.email,
        base_url,
        token
    );

    let email_addr = req.email.clone();
    let cfg_clone = magic_link_config.clone();
    tokio::spawn(async move {
        if let Err(e) = send_magic_link_email(
            &email_addr,
            &token,
            &base_url,
            return_to.as_deref(),
            &cfg_clone,
        )
        .await
        {
            tracing::error!("Failed to send magic link email: {}", e);
        }
    });

    Ok(Json(MessageResponse {
        message: "If your email is eligible, a sign-in link has been sent.".to_string(),
    }))
}

async fn send_magic_link_email(
    to_email: &str,
    token: &str,
    base_url: &str,
    return_to: Option<&str>,
    config: &MagicLinkAuth,
) -> Result<(), OxyError> {
    use crate::emails::{
        EmailMessage, EmailProvider, local_test::LocalTestEmailProvider, ses::SesEmailProvider,
    };

    let magic_link_url = match return_to {
        Some(target) => format!(
            "{base_url}/auth/magic-link/callback?token={token}&return_to={}",
            urlencoding::encode(target)
        ),
        None => format!("{base_url}/auth/magic-link/callback?token={token}"),
    };
    let message = EmailMessage {
        subject: "Sign in to Oxygen".to_string(),
        html_body: build_magic_link_email_html(&magic_link_url, to_email)?,
        text_body: format!(
            "Your sign-in link for Oxygen\n\nClick the link below to sign in. For security, this link expires in 15 minutes and can only be used once.\n\n{magic_link_url}\n\nThis link was requested for {to_email}. If you didn't request this, you can safely ignore this email — your account remains secure."
        ),
    };

    if std::env::var("MAGIC_LINK_LOCAL_TEST").is_ok() {
        LocalTestEmailProvider
            .send(&config.from_email, to_email, message)
            .await
    } else {
        SesEmailProvider::new(config.aws_region.as_deref())
            .await
            .send(&config.from_email, to_email, message)
            .await
    }
}

static MAGIC_LINK_TEMPLATE: Lazy<Handlebars<'static>> = Lazy::new(|| {
    let mut hbs = Handlebars::new();
    hbs.register_template_string("magic_link", include_str!("../../../emails/magic_link.hbs"))
        .expect("magic_link.hbs is valid");
    hbs
});

fn build_magic_link_email_html(magic_link_url: &str, to_email: &str) -> Result<String, OxyError> {
    let data = serde_json::json!({
        "magic_link_url": magic_link_url,
        "to_email": to_email,
        "year": Utc::now().format("%Y").to_string(),
    });

    MAGIC_LINK_TEMPLATE
        .render("magic_link", &data)
        .map_err(|e| OxyError::RuntimeError(format!("Failed to render magic link template: {e}")))
}
