use axum::extract::State;
use axum::{
    extract,
    http::{HeaderMap, StatusCode},
    response::Json,
};
use chrono::{Duration, Utc};
use entity::{prelude::Users, users, users::UserStatus};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use uuid::Uuid;

use crate::server::router::AppState;
use oxy::{
    config::constants::AUTHENTICATION_SECRET_KEY,
    database::{client::establish_connection, filters::UserQueryFilterExt},
};

use super::dto::*;
use super::ops::*;

/// `GET /auth/return-to/validate?url=...` — public endpoint the web-app
/// calls before performing a post-login redirect. Returns 200 if the URL is
/// safe to follow, 403 otherwise. No body — status code is the result.
pub async fn validate_return_to(
    extract::Query(query): extract::Query<ValidateReturnToQuery>,
) -> StatusCode {
    if validate_return_to_url(&query.url) {
        StatusCode::OK
    } else {
        StatusCode::FORBIDDEN
    }
}

/// `GET /auth/session` — hydrate the SPA's auth state from the `oxy_session`
/// cookie.
///
/// The session cookie is scoped to `.oxygen-hq.com`, so it is shared across
/// every org subdomain — but the SPA's bearer token lives in `localStorage`,
/// which is **per-origin**. A browser that lands on `pokehouse.oxygen-hq.com`
/// with a valid cookie therefore has no SPA token and would otherwise render a
/// local login (whose OAuth `redirect_uri` is the subdomain → the provider
/// rejects it). This endpoint re-reads the cookie JWT, re-issues a token +
/// cookie, and returns the user + orgs so the SPA can `login()` without a
/// redundant round-trip to the app-host login. Returns `401` when no valid
/// session cookie is present, so the caller can fall back to centralized login.
pub async fn get_session(
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<AuthResponse>), StatusCode> {
    let jwt =
        oxy_auth::built_in::extract_session_cookie(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = decode::<Claims>(
        &jwt,
        &DecodingKey::from_secret(AUTHENTICATION_SECRET_KEY.as_bytes()),
        &Validation::default(),
    )
    .map_err(|e| {
        tracing::debug!("session hydrate: cookie JWT rejected: {e}");
        StatusCode::UNAUTHORIZED
    })?
    .claims;

    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("session hydrate: db connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user = Users::find_by_id(user_id)
        .one(&connection)
        .await
        .map_err(|e| {
            tracing::error!("session hydrate: user lookup failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .filter(|u| u.status == UserStatus::Active)
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let (token, user_info, orgs) = finalize_login(user, &connection).await?;
    Ok(login_response(&headers, token, user_info, orgs))
}

pub async fn issue_oauth_state() -> Result<Json<OAuthStateResponse>, StatusCode> {
    let now = Utc::now();
    let exp = now + Duration::seconds(OAUTH_STATE_TTL_SECS);
    let nonce_bytes: [u8; 16] = rand::random();
    let claims = OAuthStateClaims {
        nonce: hex::encode(nonce_bytes),
        purpose: OAUTH_STATE_PURPOSE.to_string(),
        exp: exp.timestamp() as usize,
        iat: now.timestamp() as usize,
    };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(AUTHENTICATION_SECRET_KEY.as_bytes()),
    )
    .map_err(|e| {
        tracing::error!("Failed to sign OAuth state: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(OAuthStateResponse { state: token }))
}

pub async fn get_config(
    State(app_state): State<AppState>,
    peer: super::PeerAddr,
) -> Result<Json<AuthConfigResponse>, StatusCode> {
    let auth_config = oxy::config::oxy::get_oxy_config()
        .ok()
        .and_then(|config| config.authentication);

    let has_google = auth_config
        .as_ref()
        .and_then(|auth| auth.google.as_ref())
        .is_some();
    let has_okta = auth_config
        .as_ref()
        .and_then(|auth| auth.okta.as_ref())
        .is_some();
    let has_magic_link = auth_config
        .as_ref()
        .and_then(|auth| auth.magic_link.as_ref())
        .is_some();

    let auth_enabled = (has_google || has_okta || has_magic_link) && !app_state.mode.is_local();

    let github_client_id = std::env::var("GITHUB_CLIENT_ID").ok();

    let observability_enabled = app_state.observability.is_some();
    let billing_enabled = crate::server::feature_flags::is_enabled("billing");
    // Per-caller, not process-wide: an off-box peer that the dev-login route
    // 404s must not be told from here that a bypass exists. See
    // `dev_login::dev_login_reachable_by`.
    let dev_login = super::dev_login_reachable_by(peer.0);

    if !auth_enabled || app_state.internal {
        return Ok(Json(AuthConfigResponse {
            auth_enabled: false,
            google: None,
            okta: None,
            magic_link: None,
            enterprise: app_state.enterprise,
            observability_enabled,
            github: github_client_id.map(|client_id| GitHubAuthConfig { client_id }),
            mode: app_state.mode.label(),
            billing_enabled,
            dev_login,
        }));
    }

    let google_client_id = auth_config
        .as_ref()
        .and_then(|auth| auth.google.as_ref())
        .map(|google| google.client_id.clone());
    let okta_config = auth_config
        .as_ref()
        .and_then(|auth| auth.okta.as_ref())
        .map(|okta| OktaConfig {
            client_id: okta.client_id.clone(),
            domain: okta.domain.clone(),
        });

    let config = AuthConfigResponse {
        auth_enabled: true,
        google: google_client_id.map(|client_id| GoogleConfig { client_id }),
        okta: okta_config,
        magic_link: if has_magic_link { Some(true) } else { None },
        enterprise: app_state.enterprise,
        observability_enabled,
        github: github_client_id.map(|client_id| GitHubAuthConfig { client_id }),
        mode: app_state.mode.label(),
        billing_enabled,
        dev_login,
    };

    Ok(Json(config))
}

/// Mint a session token with the default thirty-day window.
///
/// The window is [`super::ops::SESSION_TTL_SECS`], shared with the cookie that
/// wraps this JWT so the two cannot drift.
pub async fn create_auth_token(user: users::Model) -> Result<String, StatusCode> {
    create_auth_token_with_ttl(user, Duration::seconds(super::ops::SESSION_TTL_SECS)).await
}

/// Mint a session token with an explicit lifetime.
///
/// Split out for frontline sign-in, which gets twelve hours rather than the
/// thirty-day default:
/// that credential was proved by four digits typed on a shared tablet, and it
/// should not outlive the shift. Sharing the minting path rather than writing a
/// second one keeps there being exactly one place a session is created.
pub async fn create_auth_token_with_ttl(
    user: users::Model,
    ttl: Duration,
) -> Result<String, StatusCode> {
    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user_clone = user.clone();
    let mut user_update: users::ActiveModel = user.into();
    user_update.last_login_at = Set(chrono::Utc::now().into());
    user_update.update(&connection).await.map_err(|e| {
        tracing::error!("Failed to update user last login: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let now = Utc::now();
    let exp = now + ttl;

    let claims = Claims {
        sub: user_clone.id.to_string(),
        // The ADDRESS, not a display label. `sub` is what resolves the session
        // now (see `BuiltInAuthenticator::validate`), so this claim is only
        // read by older code paths and by a human reading a decoded token —
        // and a label there would put a non-unique `name` in a field every
        // reader takes for an email. Empty for a frontline worker, which is
        // true rather than convenient.
        email: user_clone.email.clone().unwrap_or_default(),
        exp: exp.timestamp() as usize,
        iat: now.timestamp() as usize,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(AUTHENTICATION_SECRET_KEY.as_bytes()),
    )
    .map_err(|e| {
        tracing::error!("Failed to generate JWT token: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(token)
}

pub async fn google_auth(
    headers: HeaderMap,
    extract::Json(google_request): extract::Json<GoogleAuthRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), StatusCode> {
    verify_oauth_state(&google_request.state)?;
    let base_url = extract_base_url_from_headers(&headers);
    let user_info = exchange_google_code_for_user_info(&google_request.code, &base_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to exchange Google code: {}", e);
            StatusCode::UNAUTHORIZED
        })?;

    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user = match Users::find()
        .filter_by_email(&user_info.email)
        .one(&connection)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query user: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        Some(existing_user) if existing_user.status == UserStatus::Active => {
            let mut user_update: users::ActiveModel = existing_user.clone().into();
            user_update.name = Set(user_info.name.clone());
            user_update.picture = Set(user_info.picture.clone());
            user_update.email_verified = Set(true);
            user_update.last_login_at = Set(chrono::Utc::now().into());
            user_update.update(&connection).await.map_err(|e| {
                tracing::error!("Failed to update user: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
        }
        Some(existing_user) if existing_user.status == UserStatus::Deleted => {
            // User account has been deleted - unauthorized
            tracing::warn!(
                "Deleted user {} attempted to authenticate via Google",
                user_info.email
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        Some(existing_user) => {
            // Handle any other status - update existing user info
            let mut user_update: users::ActiveModel = existing_user.clone().into();
            user_update.name = Set(user_info.name.clone());
            user_update.picture = Set(user_info.picture.clone());
            user_update.email_verified = Set(true);
            user_update.last_login_at = Set(chrono::Utc::now().into());
            user_update.update(&connection).await.map_err(|e| {
                tracing::error!("Failed to update user: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
        }
        None => {
            let new_user = users::ActiveModel {
                id: Set(Uuid::new_v4()),
                email: Set(Some(user_info.email.clone())),
                name: Set(user_info.name.clone()),
                picture: Set(user_info.picture.clone()),
                email_verified: Set(true),
                magic_link_token: sea_orm::ActiveValue::NotSet,
                magic_link_token_expires_at: sea_orm::ActiveValue::NotSet,
                status: Set(UserStatus::Active),
                created_at: sea_orm::ActiveValue::NotSet,
                last_login_at: sea_orm::ActiveValue::NotSet,
            };

            insert_user_or_fetch_existing(new_user, &user_info.email, &connection).await?
        }
    };

    let (token, user_info_payload, orgs) = finalize_login(user, &connection).await?;
    Ok(login_response(&headers, token, user_info_payload, orgs))
}

pub async fn okta_auth(
    headers: HeaderMap,
    extract::Json(okta_request): extract::Json<OktaAuthRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), StatusCode> {
    verify_oauth_state(&okta_request.state)?;
    let base_url = extract_base_url_from_headers(&headers);
    let user_info = exchange_okta_code_for_user_info(&okta_request.code, &base_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to exchange Okta code: {}", e);
            StatusCode::UNAUTHORIZED
        })?;

    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user = match Users::find()
        .filter_by_email(&user_info.email)
        .one(&connection)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query user: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        Some(existing_user) if existing_user.status == UserStatus::Deleted => {
            // User account has been deleted - unauthorized
            tracing::warn!(
                "Deleted user {} attempted to authenticate via Okta",
                user_info.email
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        Some(existing_user) => {
            let mut user_update: users::ActiveModel = existing_user.clone().into();
            user_update.name = Set(user_info.name.clone());
            user_update.picture = Set(user_info.picture.clone());
            user_update.email_verified = Set(true);
            user_update.last_login_at = Set(chrono::Utc::now().into());
            user_update.update(&connection).await.map_err(|e| {
                tracing::error!("Failed to update user: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
        }
        None => {
            let new_user = users::ActiveModel {
                id: Set(Uuid::new_v4()),
                email: Set(Some(user_info.email.clone())),
                name: Set(user_info.name.clone()),
                picture: Set(user_info.picture.clone()),
                email_verified: Set(true),
                magic_link_token: sea_orm::ActiveValue::NotSet,
                magic_link_token_expires_at: sea_orm::ActiveValue::NotSet,
                status: Set(UserStatus::Active),
                created_at: sea_orm::ActiveValue::NotSet,
                last_login_at: sea_orm::ActiveValue::NotSet,
            };

            insert_user_or_fetch_existing(new_user, &user_info.email, &connection).await?
        }
    };

    let (token, user_info_payload, orgs) = finalize_login(user, &connection).await?;
    Ok(login_response(&headers, token, user_info_payload, orgs))
}

pub async fn github_auth(
    headers: HeaderMap,
    extract::Json(payload): extract::Json<GitHubAuthRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), StatusCode> {
    verify_oauth_state(&payload.state)?;
    let base_url = extract_base_url_from_headers(&headers);
    let user_info = exchange_github_code_for_user_info(&payload.code, &base_url)
        .await
        .map_err(|e| {
            tracing::error!("Failed to exchange GitHub code: {}", e);
            StatusCode::UNAUTHORIZED
        })?;

    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user = match Users::find()
        .filter_by_email(&user_info.email)
        .one(&connection)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query user: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })? {
        Some(existing_user) if existing_user.status == UserStatus::Deleted => {
            tracing::warn!(
                "Deleted user {} attempted to authenticate via GitHub",
                user_info.email
            );
            return Err(StatusCode::UNAUTHORIZED);
        }
        Some(existing_user) => {
            let mut user_update: users::ActiveModel = existing_user.clone().into();
            user_update.name = Set(user_info.name.clone());
            user_update.picture = Set(user_info.picture.clone());
            user_update.email_verified = Set(true);
            user_update.last_login_at = Set(chrono::Utc::now().into());
            user_update.update(&connection).await.map_err(|e| {
                tracing::error!("Failed to update user: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
        }
        None => {
            let new_user = users::ActiveModel {
                id: Set(Uuid::new_v4()),
                email: Set(Some(user_info.email.clone())),
                name: Set(user_info.name.clone()),
                picture: Set(user_info.picture.clone()),
                email_verified: Set(true),
                magic_link_token: sea_orm::ActiveValue::NotSet,
                magic_link_token_expires_at: sea_orm::ActiveValue::NotSet,
                status: Set(UserStatus::Active),
                created_at: sea_orm::ActiveValue::NotSet,
                last_login_at: sea_orm::ActiveValue::NotSet,
            };
            insert_user_or_fetch_existing(new_user, &user_info.email, &connection).await?
        }
    };

    let (token, user_info_payload, orgs) = finalize_login(user, &connection).await?;
    Ok(login_response(&headers, token, user_info_payload, orgs))
}

pub async fn request_magic_link(
    headers: HeaderMap,
    extract::Json(req): extract::Json<MagicLinkRequest>,
) -> axum::response::Response {
    use axum::http::header::RETRY_AFTER;
    use axum::response::IntoResponse;

    // Normalize email to lowercase at the point of ingestion so all downstream
    // code (allowlist check, DB queries, SES) operates on a consistent value.
    let req = MagicLinkRequest {
        email: req.email.to_lowercase(),
        return_to: req.return_to,
    };

    // Validate email format before doing anything else.
    if !is_valid_email_format(&req.email) {
        return (
            StatusCode::BAD_REQUEST,
            Json(MessageResponse {
                message: "Invalid email address.".to_string(),
            }),
        )
            .into_response();
    }

    // Rate limit — checked before allowlist so timing cannot reveal allowlist membership.
    if let Some(retry_after_secs) = check_magic_link_rate_limit(&req.email) {
        let mins = retry_after_secs.div_ceil(60);
        tracing::warn!("Magic link rate limit exceeded for: {}", req.email);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                RETRY_AFTER,
                axum::http::HeaderValue::from_str(&retry_after_secs.to_string())
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("3600")),
            )],
            Json(MessageResponse {
                message: format!(
                    "Too many sign-in attempts. Please try again in {mins} minute{}.",
                    if mins == 1 { "" } else { "s" }
                ),
            }),
        )
            .into_response();
    }

    request_magic_link_inner(headers, req).await.into_response()
}

pub async fn verify_magic_link(
    headers: HeaderMap,
    extract::Json(req): extract::Json<MagicLinkVerifyRequest>,
) -> Result<(HeaderMap, Json<AuthResponse>), StatusCode> {
    let connection = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let user = Users::find()
        .filter_active_by_magic_link_token(&req.token)
        .one(&connection)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query user by magic link token: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    // Check expiry
    let expires_at = user
        .magic_link_token_expires_at
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if Utc::now() > expires_at.with_timezone(&Utc) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Clear token, mark email verified
    let mut user_update: users::ActiveModel = user.into();
    user_update.magic_link_token = Set(None);
    user_update.magic_link_token_expires_at = Set(None);
    user_update.email_verified = Set(true);
    let user = user_update.update(&connection).await.map_err(|e| {
        tracing::error!("Failed to clear magic link token: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let (token, user_info, orgs) = finalize_login(user, &connection).await?;
    Ok(login_response(&headers, token, user_info, orgs))
}

#[cfg(test)]
mod mode_field_tests {
    use super::*;
    use oxy_app_core::serve_mode::ServeMode;

    #[test]
    fn local_mode_serializes() {
        let response = AuthConfigResponse {
            auth_enabled: false,
            google: None,
            okta: None,
            magic_link: None,
            enterprise: false,
            observability_enabled: false,
            github: None,
            mode: ServeMode::Local.label(),
            billing_enabled: false,
            dev_login: false,
        };
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(json["mode"], "local");
    }

    #[test]
    fn cloud_mode_serializes() {
        let response = AuthConfigResponse {
            auth_enabled: false,
            google: None,
            okta: None,
            magic_link: None,
            enterprise: false,
            observability_enabled: false,
            github: None,
            mode: ServeMode::Cloud.label(),
            billing_enabled: false,
            dev_login: false,
        };
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(json["mode"], "cloud");
    }
}
