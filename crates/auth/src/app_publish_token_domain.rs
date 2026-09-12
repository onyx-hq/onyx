//! App-publish-token domain logic: generation, hashing, and DB-backed resolution.
//!
//! App publish tokens are long-lived bearer credentials for machine auth (primarily
//! `oxyc publish` in CI). An app-admin mints one, pastes the plaintext into a
//! CI secret (`OXY_TOKEN`), and the server accepts it as a bearer credential
//! **only on the customer-apps admin surface** (scope enforced in the request
//! path — see `app_publish_tokens_scope` middleware in the app crate).
//!
//! Security model:
//! - Only a SHA-256 hash of the plaintext is stored; the plaintext is shown
//!   once at creation and never persisted.
//! - Lookup hashes the presented bearer and matches on `token_hash`, so a DB
//!   dump does not leak usable credentials.
//! - Revoked tokens (`revoked_at IS NOT NULL`) never resolve.

use crate::types::AuthenticatedUser;
use entity::app_publish_tokens;
use entity::prelude::{AppPublishTokens, Users};
use oxy_shared::errors::OxyError;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use sha2::{Digest, Sha256};

/// Every app publish token starts with this prefix so a stray token is obvious in
/// logs/dumps and easy to grep for, and so bearer-scheme dispatch can cheaply
/// recognise "this is an app publish token, not a JWT" before hitting the DB.
pub const APP_PUBLISH_TOKEN_PREFIX: &str = "oxypublish_";

/// Number of random bytes of entropy in the token body (256 bits).
const TOKEN_ENTROPY_BYTES: usize = 32;

/// A freshly minted token: the plaintext (returned to the caller **once**),
/// plus the hash and display prefix that get persisted.
#[derive(Debug, Clone)]
pub struct GeneratedToken {
    /// Full plaintext, e.g. `oxypublish_<64 hex chars>`. Never stored.
    pub plaintext: String,
    /// SHA-256 hex digest of `plaintext` — the only form persisted.
    pub token_hash: String,
    /// Short, non-secret display fragment, e.g. `oxypublish_ab12cd34`.
    pub token_prefix: String,
}

/// True when a bearer value looks like an app publish token. Cheap prefix check used
/// to route dispatch away from JWT decoding before touching the DB.
pub fn is_app_publish_token(candidate: &str) -> bool {
    candidate.starts_with(APP_PUBLISH_TOKEN_PREFIX)
}

/// SHA-256 hex digest of the plaintext token. Deterministic so lookup can
/// re-hash a presented bearer and match the stored `token_hash`.
pub fn hash_token(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    hex::encode(digest)
}

/// Generate a new app publish token with 256 bits of entropy. Returns the plaintext
/// (to hand to the operator once), its hash, and a display prefix.
pub fn generate_token() -> GeneratedToken {
    let bytes: [u8; TOKEN_ENTROPY_BYTES] = rand::random();
    let body = hex::encode(bytes);
    let plaintext = format!("{APP_PUBLISH_TOKEN_PREFIX}{body}");
    let token_hash = hash_token(&plaintext);
    // Prefix = scheme + first 8 hex chars of the body. Non-secret; enough to
    // disambiguate tokens in a list without revealing the credential.
    let token_prefix = format!("{APP_PUBLISH_TOKEN_PREFIX}{}", &body[..8]);
    GeneratedToken {
        plaintext,
        token_hash,
        token_prefix,
    }
}

/// A successfully resolved app publish token: the owning user plus the token row id
/// (so the caller can attach an `AppPublishTokenAuth` scope marker).
#[derive(Debug, Clone)]
pub struct ResolvedAppPublishToken {
    /// The owning user — real for a staff token, a synthetic machine principal for
    /// an OIDC-minted app-scoped token (which has no human `created_by`).
    pub user: AuthenticatedUser,
    pub token_id: uuid::Uuid,
    /// Set iff this is an app-scoped machine token. Flows into
    /// `AppPublishTokenAuth.app_id`.
    pub app_id: Option<uuid::Uuid>,
}

/// Resolve a presented plaintext app publish token to its owning user.
///
/// Returns `Ok(None)` when the token is not live (unknown, or revoked) or the
/// owner no longer exists — the caller decides how to surface that. Bumps
/// `last_used_at` on a successful resolve (best-effort; a failure to bump is
/// logged, not fatal).
pub async fn resolve_app_publish_token(
    db: &DatabaseConnection,
    plaintext: &str,
) -> Result<Option<ResolvedAppPublishToken>, OxyError> {
    let token_hash = hash_token(plaintext);

    let token = AppPublishTokens::find()
        .filter(app_publish_tokens::Column::TokenHash.eq(token_hash))
        .filter(app_publish_tokens::Column::RevokedAt.is_null())
        .one(db)
        .await
        .map_err(|e| OxyError::DBError(format!("app publish token lookup: {e}")))?;

    let Some(token) = token else {
        return Ok(None);
    };

    // Expiry — machine tokens carry one; staff tokens historically don't (NULL =
    // non-expiring). An expired token authenticates as nothing.
    if let Some(exp) = token.expires_at
        && exp < chrono::Utc::now().fixed_offset()
    {
        return Ok(None);
    }

    match token.created_by {
        // Classic staff token — authenticates as its minting user.
        Some(uid) => {
            let user = Users::find_by_id(uid)
                .one(db)
                .await
                .map_err(|e| OxyError::DBError(format!("app publish token owner lookup: {e}")))?;
            let Some(user) = user else {
                tracing::warn!(
                    token_id = %token.id,
                    "app publish token owner {uid} is gone; treating as unauthenticated"
                );
                return Ok(None);
            };
            touch_last_used(db, token.id).await;
            Ok(Some(ResolvedAppPublishToken {
                user: AuthenticatedUser::from(user),
                token_id: token.id,
                app_id: token.app_id,
            }))
        }
        // OIDC-minted machine token — no human. It MUST carry an app_id (the
        // exchange always sets one); a NULL-creator token without an app_id is
        // malformed and authenticates as nothing. Downstream authorizes by app_id
        // + consent, never by this principal, so it is a placeholder identity
        // scoped to the publish path by the token-scope middleware.
        None => {
            let Some(app_id) = token.app_id else {
                tracing::warn!(token_id = %token.id, "machine publish token has no app_id; rejecting");
                return Ok(None);
            };
            touch_last_used(db, token.id).await;
            Ok(Some(ResolvedAppPublishToken {
                user: AuthenticatedUser::machine_publisher(),
                token_id: token.id,
                app_id: Some(app_id),
            }))
        }
    }
}

/// Best-effort `last_used_at` bump. A transient failure here must not fail the
/// request, so it's logged and swallowed.
async fn touch_last_used(db: &DatabaseConnection, token_id: uuid::Uuid) {
    let update = app_publish_tokens::ActiveModel {
        id: ActiveValue::Unchanged(token_id),
        last_used_at: ActiveValue::Set(Some(chrono::Utc::now().fixed_offset())),
        ..Default::default()
    };
    if let Err(e) = update.update(db).await {
        tracing::warn!(token_id = %token_id, "failed to bump app publish token last_used_at: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_has_recognisable_prefix() {
        let t = generate_token();
        assert!(t.plaintext.starts_with(APP_PUBLISH_TOKEN_PREFIX));
        assert!(t.token_prefix.starts_with(APP_PUBLISH_TOKEN_PREFIX));
        assert_eq!(t.token_prefix.len(), APP_PUBLISH_TOKEN_PREFIX.len() + 8);
        // scheme (9) + 64 hex chars (32 bytes).
        assert_eq!(t.plaintext.len(), APP_PUBLISH_TOKEN_PREFIX.len() + 64);
    }

    #[test]
    fn is_app_publish_token_recognises_scheme() {
        assert!(is_app_publish_token("oxypublish_deadbeef"));
        assert!(!is_app_publish_token("oxy_app_deadbeef"));
        assert!(!is_app_publish_token("eyJhbGciOi.jwt.token"));
    }

    #[test]
    fn hash_is_deterministic_and_matches_generation() {
        let t = generate_token();
        assert_eq!(hash_token(&t.plaintext), t.token_hash);
        // Re-hashing the same plaintext yields the same digest (lookup relies
        // on this).
        assert_eq!(hash_token(&t.plaintext), hash_token(&t.plaintext));
    }

    #[test]
    fn hash_differs_for_different_tokens() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a.plaintext, b.plaintext);
        assert_ne!(a.token_hash, b.token_hash);
    }

    #[test]
    fn hash_is_not_the_plaintext() {
        let t = generate_token();
        // Never store the plaintext; the hash must not contain it.
        assert!(!t.token_hash.contains(&t.plaintext));
        // SHA-256 hex digest is always 64 chars.
        assert_eq!(t.token_hash.len(), 64);
    }
}
