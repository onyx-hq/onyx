//! `/api/admin/app-publish-tokens` — CRUD for app publish tokens (machine-auth bearer
//! credentials, primarily for `oxyc publish` in CI).
//!
//! Sits behind the `/admin` door plus `Cap::ManageApps` (publishing is what these are
//! for). The plaintext is returned **once** on create and never stored; only a SHA-256
//! hash and a non-secret display prefix are persisted.
//!
//! ## Tokens are owned by their minter
//!
//! This module used to state the opposite — "managed across admins, not owned by their
//! minter" — and that was a fair rule while every admin was the same homogeneous staff
//! population, all equally entitled to everything.
//!
//! The capability split falsified that premise. `Cap::ManageApps` is now held by App
//! Operators, who never existed when the rule was written, so an unfiltered list plus an
//! id-addressed revoke means a grant bounded to a single tenant can enumerate and revoke
//! **every Oxy engineer's CI publish token** — a cross-operator denial of service with no
//! boundary at all.
//!
//! So list and revoke are scoped to the caller's own tokens, and the shared cross-admin
//! view is what `Cap::OperatePlatform` buys — the capability that already means "operates
//! Oxy's own machinery". Minting is unrestricted and needs no boundary: a staff token
//! carries `app_id: None`, and `custom_apps_publish_authz::resolve_actor` re-resolves the
//! minter's own capability and scope at publish time, so a token can never out-reach the
//! person holding it.
//!
//! A live token authenticates as its minting app-admin **only on the
//! customer-apps admin surface** — see the `app_publish_token_scope` middleware and
//! `oxy_auth::app_publish_token_domain` for enforcement. This module owns lifecycle
//! (create/list/revoke); token generation + hashing live in the auth crate so
//! the acceptance path and the mint path share one implementation.

use axum::Json;
use axum::Router;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use chrono::Utc;
use entity::app_publish_tokens;
use entity::prelude::AppPublishTokens;
use oxy::database::client::establish_connection;
use oxy_auth::app_publish_token_domain::generate_token;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use sea_orm::{ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::server::router::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/app-publish-tokens", get(list_tokens).post(create_token))
        .route("/app-publish-tokens/{id}/revoke", post(revoke_token))
}

#[derive(Debug, Deserialize)]
pub struct CreateTokenBody {
    /// Human-readable label, e.g. "ci-publish". Falls back to a timestamped
    /// default when omitted/blank.
    pub name: Option<String>,
}

/// Create response: the **only** time the plaintext is ever returned. The
/// caller must copy it now; it cannot be retrieved later.
#[derive(Debug, Serialize)]
pub struct CreateTokenResponse {
    pub id: Uuid,
    /// Plaintext token — shown once. Paste into a CI secret as `OXY_TOKEN`.
    pub token: String,
    pub name: String,
    pub token_prefix: String,
    pub created_at: String,
}

/// Metadata-only view for listing — never carries the plaintext or hash.
#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub id: Uuid,
    pub name: String,
    pub token_prefix: String,
    pub created_by: Option<Uuid>,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub revoked: bool,
    pub revoked_at: Option<String>,
}

impl From<app_publish_tokens::Model> for TokenResponse {
    fn from(m: app_publish_tokens::Model) -> Self {
        Self {
            id: m.id,
            name: m.name,
            token_prefix: m.token_prefix,
            created_by: m.created_by,
            created_at: m.created_at.to_rfc3339(),
            last_used_at: m.last_used_at.map(|t| t.to_rfc3339()),
            revoked: m.revoked_at.is_some(),
            revoked_at: m.revoked_at.map(|t| t.to_rfc3339()),
        }
    }
}

pub async fn create_token(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    body: Option<Json<CreateTokenBody>>,
) -> Result<Json<CreateTokenResponse>, StatusCode> {
    let body = body
        .map(|Json(b)| b)
        .unwrap_or(CreateTokenBody { name: None });
    let name = body
        .name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("app-publish-token {}", Utc::now().format("%Y-%m-%d")));

    let db = establish_connection().await.map_err(|e| {
        tracing::error!("create_token DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let generated = generate_token();
    let now = Utc::now().fixed_offset();
    let id = Uuid::new_v4();

    app_publish_tokens::ActiveModel {
        id: ActiveValue::Set(id),
        name: ActiveValue::Set(name.clone()),
        token_hash: ActiveValue::Set(generated.token_hash),
        token_prefix: ActiveValue::Set(generated.token_prefix.clone()),
        created_by: ActiveValue::Set(Some(actor.id)),
        created_at: ActiveValue::Set(now),
        last_used_at: ActiveValue::Set(None),
        revoked_at: ActiveValue::Set(None),
        // Staff-minted tokens stay app-unscoped and non-expiring — the existing
        // Oxy-engineer CI flow. App-scoped fallback tokens (design §7) are minted
        // elsewhere with both set.
        app_id: ActiveValue::Set(None),
        expires_at: ActiveValue::Set(None),
    }
    .insert(&db)
    .await
    .map_err(|e| {
        tracing::error!("create_token insert failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(CreateTokenResponse {
        id,
        token: generated.plaintext,
        name,
        token_prefix: generated.token_prefix,
        created_at: now.to_rfc3339(),
    }))
}

/// Does this caller get the shared cross-admin view of every staff token?
///
/// `Cap::OperatePlatform` — the "operates Oxy's own machinery" capability. Held by
/// Global Admins and owners, not by App Operators, which is exactly the line: fleet
/// operators audit the token estate; an app publisher manages their own credential.
async fn sees_all_tokens(db: &sea_orm::DatabaseConnection, email: &str) -> bool {
    crate::server::authz::globals::platform_holds(db, email, oxy_authz::Cap::OperatePlatform).await
}

pub async fn list_tokens(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
) -> Result<Json<Vec<TokenResponse>>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("list_tokens DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let mut query = AppPublishTokens::find().order_by_desc(app_publish_tokens::Column::CreatedAt);
    if !sees_all_tokens(&db, actor.email.as_deref().unwrap_or("")).await {
        query = query.filter(app_publish_tokens::Column::CreatedBy.eq(actor.id));
    }
    let rows = query.all(&db).await.map_err(|e| {
        tracing::error!("list_tokens query failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ActiveValue;

    fn sample_model(revoked: bool) -> app_publish_tokens::Model {
        let now = Utc::now().fixed_offset();
        app_publish_tokens::Model {
            id: Uuid::new_v4(),
            name: "ci-publish".to_string(),
            token_hash: "deadbeef".repeat(8),
            token_prefix: "oxypublish_ab12cd34".to_string(),
            created_by: Some(Uuid::new_v4()),
            created_at: now,
            last_used_at: None,
            revoked_at: revoked.then_some(now),
            app_id: None,
            expires_at: None,
        }
    }

    #[test]
    fn list_view_never_leaks_secret_material() {
        let model = sample_model(false);
        let secret_hash = model.token_hash.clone();
        let resp = TokenResponse::from(model);
        let json = serde_json::to_string(&resp).unwrap();
        // The metadata view must expose neither the hash nor any plaintext.
        assert!(!json.contains(&secret_hash));
        assert!(!json.contains("token_hash"));
        assert!(json.contains("token_prefix"));
        assert!(!resp.revoked);
    }

    #[test]
    fn revoked_flag_reflects_revoked_at() {
        assert!(TokenResponse::from(sample_model(true)).revoked);
        assert!(!TokenResponse::from(sample_model(false)).revoked);
    }

    #[test]
    fn create_persists_hash_and_prefix_not_plaintext() {
        // Simulate the persistence step the handler performs: the ActiveModel
        // must carry the hash + prefix but never the plaintext.
        let generated = generate_token();
        let model = app_publish_tokens::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            name: ActiveValue::Set("t".to_string()),
            token_hash: ActiveValue::Set(generated.token_hash.clone()),
            token_prefix: ActiveValue::Set(generated.token_prefix.clone()),
            created_by: ActiveValue::Set(Some(Uuid::new_v4())),
            created_at: ActiveValue::Set(Utc::now().fixed_offset()),
            last_used_at: ActiveValue::Set(None),
            revoked_at: ActiveValue::Set(None),
            app_id: ActiveValue::Set(None),
            expires_at: ActiveValue::Set(None),
        };
        let ActiveValue::Set(stored_hash) = model.token_hash else {
            panic!("hash not set");
        };
        assert_eq!(stored_hash, generated.token_hash);
        assert_ne!(stored_hash, generated.plaintext);
    }
}

pub async fn revoke_token(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
) -> Result<Json<TokenResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("revoke_token DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let token = AppPublishTokens::find_by_id(id)
        .one(&db)
        .await
        .map_err(|e| {
            tracing::error!("revoke_token lookup failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Revoking someone else's token is a fleet-operator action, not an app-publishing
    // one — see the module docs. 404 rather than 403, so a bounded caller can't confirm
    // another operator's token exists by probing ids.
    if token.created_by != Some(actor.id)
        && !sees_all_tokens(&db, actor.email.as_deref().unwrap_or("")).await
    {
        return Err(StatusCode::NOT_FOUND);
    }

    // Idempotent: re-revoking an already-revoked token is a no-op success.
    if token.revoked_at.is_some() {
        return Ok(Json(token.into()));
    }

    let mut active: app_publish_tokens::ActiveModel = token.into();
    active.revoked_at = ActiveValue::Set(Some(Utc::now().fixed_offset()));
    let updated = active.update(&db).await.map_err(|e| {
        tracing::error!("revoke_token update failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(updated.into()))
}
