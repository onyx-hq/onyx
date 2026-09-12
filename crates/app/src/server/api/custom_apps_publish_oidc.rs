//! GitHub Actions OIDC trusted publishing (design §6).
//!
//! A CI job presents an OIDC JWT proving "I am a run in repo X on ref Y"; we
//! verify it and mint a short-lived, app-scoped publish credential. The customer
//! stores no secret.
//!
//! This module is layered so the security-critical part — the **claim matching** —
//! is a pure function with no network and no DB, and is exhaustively unit-tested:
//!
//!   * `verify_claims` — given the decoded claims and the set of publisher configs
//!     for the repo, returns the app ids whose config matches (a monorepo can
//!     publish several apps from one repo). Every match rule from the design lives
//!     here.
//!   * Signature verification (RS256 against GitHub's JWKS), audience pinning,
//!     `jti` single-use, and the exchange endpoint sit on top in sibling steps.
//!
//! The rules that get platforms owned are exactly the ones NOT to leave out:
//! never match `sub` (immutable-format changeover), require the `environment`
//! claim, reject `pull_request_target`, require a github-hosted runner, and match
//! on the numeric `repository_owner_id` (the account-resurrection defence).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

/// GitHub's OIDC issuer + JWKS. Constants, not config: there is exactly one
/// GitHub Actions OIDC provider.
const GITHUB_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";
const GITHUB_JWKS_URL: &str = "https://token.actions.githubusercontent.com/.well-known/jwks";
/// The audience WE require. GitHub's default audience is the repo owner's URL;
/// pinning our own value and rejecting others (strict aud) stops any workflow in
/// the org replaying an unrelated token into us. The generated workflow requests
/// exactly this.
pub const OXY_OIDC_AUDIENCE: &str = "oxy-publish";

/// The subset of GitHub Actions OIDC claims we verify. Every custom claim GitHub
/// emits is a **string**, including the numeric-looking `repository_owner_id`.
#[derive(Clone, Debug, Deserialize)]
pub struct GithubOidcClaims {
    /// "owner/repo" — case-insensitive.
    pub repository: String,
    pub repository_owner: String,
    /// GitHub's NUMERIC account id, as a string. The account-resurrection defence:
    /// a deleted-and-recreated owner with the same name gets a new id.
    pub repository_owner_id: String,
    /// e.g. "owner/repo/.github/workflows/oxy-publish.yml@refs/heads/main".
    pub job_workflow_ref: String,
    /// The deployment environment. REQUIRED by us — a token minted by a job with no
    /// `environment:` has no way to be gated behind required-reviewers.
    pub environment: Option<String>,
    pub event_name: String,
    /// "github-hosted" | "self-hosted".
    pub runner_environment: String,
    /// One-time id; burned by the replay store on the signature path.
    pub jti: String,
}

/// One publisher config to match against — the fields of an `app_publishers` row
/// that participate in the decision, plus the app it authorizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherConfig {
    pub app_id: Uuid,
    pub repo_owner: String,
    pub repo_owner_id: i64,
    pub repo_name: String,
    /// Just the workflow path, e.g. ".github/workflows/oxy-publish.yml".
    pub workflow_ref: String,
    pub environment: String,
}

/// Why a token was refused. The token-envelope reasons (`bad signature`, `wrong
/// aud`, `expired`, `replayed jti`) are handled on the signature path; these are
/// the claim-matching reasons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OidcReject {
    /// A fork PR running with base-repo permissions — never a publish identity.
    PullRequestTarget,
    /// A self-hosted runner is a standing token-minting box inside the partner's
    /// network; we only trust github-hosted runners.
    SelfHostedRunner,
    /// The token carries no `environment` claim, so no publisher (which all require
    /// one) can match it.
    MissingEnvironment,
    /// No publisher config for this repo matched the token's claims.
    NoMatchingPublisher,
}

/// The pure decision. Returns the app ids whose publisher config matches the
/// token — usually one, more than one only for a monorepo that publishes several
/// apps from the same repo+workflow+environment.
///
/// `publishers` is expected to already be the set of configs for this repo (the
/// caller narrows by `repository_owner_id` + `repository` at the DB layer); every
/// rule is nonetheless re-checked here so the decision stands alone.
pub fn verify_claims(
    claims: &GithubOidcClaims,
    publishers: &[PublisherConfig],
) -> Result<Vec<Uuid>, OidcReject> {
    // Token-level gates first — these reject regardless of any publisher.
    if claims.event_name == "pull_request_target" {
        return Err(OidcReject::PullRequestTarget);
    }
    if claims.runner_environment != "github-hosted" {
        return Err(OidcReject::SelfHostedRunner);
    }
    let Some(token_env) = claims.environment.as_deref() else {
        return Err(OidcReject::MissingEnvironment);
    };

    let matches: Vec<Uuid> = publishers
        .iter()
        .filter(|p| publisher_matches(claims, p, token_env))
        .map(|p| p.app_id)
        .collect();

    if matches.is_empty() {
        Err(OidcReject::NoMatchingPublisher)
    } else {
        Ok(matches)
    }
}

/// Exact, case-insensitive equality on every claim — never a prefix, never a
/// wildcard, never `sub`.
fn publisher_matches(claims: &GithubOidcClaims, p: &PublisherConfig, token_env: &str) -> bool {
    let expected_repo = format!("{}/{}", p.repo_owner, p.repo_name);
    // The workflow path portion of job_workflow_ref, before the "@<ref>".
    let expected_workflow_path = format!("{}/{}/{}", p.repo_owner, p.repo_name, p.workflow_ref);
    let token_workflow_path = claims
        .job_workflow_ref
        .split_once('@')
        .map(|(path, _ref)| path)
        .unwrap_or(&claims.job_workflow_ref);

    eq_ci(&claims.repository, &expected_repo)
        // Numeric owner id — the resurrection defence. Claim is a string.
        && claims.repository_owner_id == p.repo_owner_id.to_string()
        && eq_ci(token_workflow_path, &expected_workflow_path)
        && eq_ci(token_env, &p.environment)
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Cached JWKS keyset. Refreshed only on an unknown `kid` (key rotation) and
/// served stale on a fetch error — never fetched per request, which would make
/// GitHub a hard availability dependency and a self-DoS vector.
struct JwksCache {
    keys: JwkSet,
    fetched_at: Instant,
}

fn jwks_cache() -> &'static RwLock<Option<JwksCache>> {
    static CACHE: OnceLock<RwLock<Option<JwksCache>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// The envelope-level outcome (bad signature, wrong issuer/audience, expired,
/// replayed). Distinct from `OidcReject` (claim-matching) so the caller can log
/// precisely; both surface to CI as a 401.
#[derive(Debug)]
pub enum OidcError {
    /// Could not reach or parse GitHub's JWKS and had no cached copy.
    JwksUnavailable,
    /// Header had no `kid`, or no key matched even after a refresh.
    UnknownKey,
    /// Signature / issuer / audience / expiry verification failed.
    InvalidToken(String),
    /// The `jti` was already spent — a replay.
    Replayed,
    /// A DB error recording the `jti`. Fails closed (we do not accept a token we
    /// could not mark used).
    Db(String),
}

/// Fetch GitHub's JWKS, refreshing the cache. Best-effort: on error, leave any
/// existing cache in place.
async fn refresh_jwks() -> Result<(), OidcError> {
    let set = reqwest::get(GITHUB_JWKS_URL)
        .await
        .map_err(|_| OidcError::JwksUnavailable)?
        .json::<JwkSet>()
        .await
        .map_err(|_| OidcError::JwksUnavailable)?;
    *jwks_cache().write().await = Some(JwksCache {
        keys: set,
        fetched_at: Instant::now(),
    });
    Ok(())
}

/// Find the decoding key for `kid`, refreshing the cache once on a miss (a rotated
/// key), and rate-limiting refreshes to at most once per 30s so a stream of
/// unknown-kid tokens can't turn into a fetch storm.
async fn decoding_key_for(kid: &str) -> Result<DecodingKey, OidcError> {
    if let Some(cache) = jwks_cache().read().await.as_ref()
        && let Some(jwk) = cache.keys.find(kid)
    {
        return DecodingKey::from_jwk(jwk).map_err(|_| OidcError::UnknownKey);
    }

    // Miss — refresh at most once per 30s, then look again.
    let stale = jwks_cache()
        .read()
        .await
        .as_ref()
        .map(|c| c.fetched_at.elapsed() > Duration::from_secs(30))
        .unwrap_or(true);
    if stale {
        refresh_jwks().await?;
    }

    let guard = jwks_cache().read().await;
    let cache = guard.as_ref().ok_or(OidcError::JwksUnavailable)?;
    let jwk = cache.keys.find(kid).ok_or(OidcError::UnknownKey)?;
    DecodingKey::from_jwk(jwk).map_err(|_| OidcError::UnknownKey)
}

/// Verify a GitHub Actions OIDC JWT end to end: RS256 signature against GitHub's
/// JWKS, issuer + audience pinned, expiry checked, and the `jti` burned so the
/// token cannot be replayed. Returns the decoded claims for `verify_claims` to
/// match against publisher configs.
///
/// `db` is used only to record the `jti`. Fails closed on any DB error — a token
/// we cannot mark used is a token we do not accept.
pub async fn verify_token(
    db: &DatabaseConnection,
    token: &str,
) -> Result<GithubOidcClaims, OidcError> {
    let header = decode_header(token).map_err(|e| OidcError::InvalidToken(e.to_string()))?;
    let kid = header.kid.ok_or(OidcError::UnknownKey)?;
    let key = decoding_key_for(&kid).await?;

    // RS256 only — never trust the header's alg. Pin issuer + our audience;
    // `validate_aud` defaults on, and a single required audience with a
    // non-matching or multi-aud token is rejected.
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[GITHUB_OIDC_ISSUER]);
    validation.set_audience(&[OXY_OIDC_AUDIENCE]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "aud"]);
    validation.leeway = 30;

    let data = decode::<GithubOidcClaims>(token, &key, &validation)
        .map_err(|e| OidcError::InvalidToken(e.to_string()))?;
    let claims = data.claims;

    burn_jti(db, &claims.jti).await?;
    Ok(claims)
}

/// Record the `jti` as spent. A PK conflict means it was already used → replay.
/// TTL is generous (an hour past now) since the token itself expires in minutes;
/// a sweeper prunes by `expires_at`.
async fn burn_jti(db: &DatabaseConnection, jti: &str) -> Result<(), OidcError> {
    let expires_at = (chrono::Utc::now() + chrono::Duration::hours(1)).fixed_offset();
    let row = entity::oidc_used_jti::ActiveModel {
        jti: ActiveValue::Set(jti.to_string()),
        expires_at: ActiveValue::Set(expires_at),
    };
    match row.insert(db).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // A unique/PK violation is the replay case; anything else fails closed.
            let msg = e.to_string();
            if msg.contains("duplicate") || msg.contains("unique") {
                Err(OidcError::Replayed)
            } else {
                Err(OidcError::Db(msg))
            }
        }
    }
}

/// Load the publisher configs for a repo, for `verify_claims`. Narrowed by the
/// numeric owner id + repo name so the pure matcher sees only plausibly-relevant
/// rows.
pub async fn publishers_for_repo(
    db: &DatabaseConnection,
    repo_owner_id: i64,
    repo_name: &str,
) -> Result<Vec<PublisherConfig>, OidcError> {
    use entity::prelude::AppPublishers;
    use sea_orm::{ColumnTrait, QueryFilter};
    let rows = AppPublishers::find()
        .filter(entity::app_publishers::Column::RepoOwnerId.eq(repo_owner_id))
        .filter(entity::app_publishers::Column::RepoName.eq(repo_name))
        .all(db)
        .await
        .map_err(|e| OidcError::Db(e.to_string()))?;
    Ok(rows
        .into_iter()
        .map(|r| PublisherConfig {
            app_id: r.app_id,
            repo_owner: r.repo_owner,
            repo_owner_id: r.repo_owner_id,
            repo_name: r.repo_name,
            workflow_ref: r.workflow_ref,
            environment: r.environment,
        })
        .collect())
}

/// How long an exchanged credential lives. Short — it exists only to carry one
/// publish from the CI job that just proved its identity.
const EXCHANGE_TTL_MINUTES: i64 = 15;

#[derive(Deserialize)]
pub struct ExchangeRequest {
    /// The app being published, "org-slug/app-slug". The token proves the *repo*;
    /// this names *which app* in it (a monorepo publishes several).
    pub app: String,
}

#[derive(Serialize)]
pub struct ExchangeResponse {
    /// The short-lived, app-scoped publish token. The CLI uses it as the bearer on
    /// the normal publish call. Returned once, never stored in plaintext.
    pub token: String,
    pub expires_at: String,
}

fn bad(status: StatusCode, msg: &str) -> (StatusCode, String) {
    (status, msg.to_string())
}

/// `POST /customer-apps/publish/oidc-exchange` — the trusted-publishing entry
/// point. **Unauthenticated by construction**: the OIDC JWT in the Authorization
/// header IS the credential. Verify it, confirm the token's repo is a registered
/// publisher for the named app, and mint a short-lived app-scoped token.
///
/// Consent is NOT checked here — it is re-checked at publish time (a session
/// between exchange and publish must not let a stale credential outlive a revoke).
/// The exchange only proves "this CI job may mint a credential for this app".
pub async fn oidc_exchange_handler(
    headers: HeaderMap,
    Json(req): Json<ExchangeRequest>,
) -> Result<Json<ExchangeResponse>, (StatusCode, String)> {
    let jwt = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| bad(StatusCode::UNAUTHORIZED, "missing bearer OIDC token"))?;

    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("db: {e}")))?;

    // 1. Verify the token envelope (signature, iss, aud, exp, jti single-use).
    let claims = verify_token(&db, jwt).await.map_err(|e| {
        tracing::warn!("oidc-exchange: token rejected: {e:?}");
        match e {
            OidcError::Replayed => bad(StatusCode::UNAUTHORIZED, "token already used"),
            OidcError::Db(m) => bad(StatusCode::INTERNAL_SERVER_ERROR, &m),
            _ => bad(StatusCode::UNAUTHORIZED, "invalid OIDC token"),
        }
    })?;

    // 2. Resolve the named app.
    let (org_slug, app_slug) = req
        .app
        .split_once('/')
        .ok_or_else(|| bad(StatusCode::BAD_REQUEST, "app must be 'org-slug/app-slug'"))?;
    let app = resolve_app_by_slugs(&db, org_slug, app_slug)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, &e))?
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "app not found"))?;

    // 3. Match the token's claims against the publishers registered for its repo.
    let owner_id: i64 = claims
        .repository_owner_id
        .parse()
        .map_err(|_| bad(StatusCode::UNAUTHORIZED, "malformed repository_owner_id"))?;
    let repo_name = claims
        .repository
        .split_once('/')
        .map(|(_owner, name)| name.to_string())
        .unwrap_or_default();
    let publishers = publishers_for_repo(&db, owner_id, &repo_name)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")))?;
    let matched = verify_claims(&claims, &publishers).map_err(|e| {
        bad(
            StatusCode::FORBIDDEN,
            &format!("no trusted publisher: {e:?}"),
        )
    })?;

    // The token may match several apps (monorepo); it must match THE ONE being
    // published.
    if !matched.contains(&app.id) {
        return Err(bad(
            StatusCode::FORBIDDEN,
            "this workflow is not a registered publisher for this app",
        ));
    }

    // 4. Mint the app-scoped machine token.
    let minted = mint_app_scoped_token(&db, app.id)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, &e))?;

    tracing::info!(app_id = %app.id, repo = %claims.repository, "oidc-exchange: minted app-scoped publish token");
    Ok(Json(minted))
}

async fn resolve_app_by_slugs(
    db: &DatabaseConnection,
    org_slug: &str,
    app_slug: &str,
) -> Result<Option<entity::apps::Model>, String> {
    use entity::prelude::{Apps, Organizations};
    let Some(org) = Organizations::find()
        .filter(entity::organizations::Column::Slug.eq(org_slug))
        .one(db)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    Apps::find()
        .filter(entity::apps::Column::OrgId.eq(org.id))
        .filter(entity::apps::Column::Slug.eq(app_slug))
        .one(db)
        .await
        .map_err(|e| e.to_string())
}

/// Insert an app-scoped, expiring, creator-less token row and return its plaintext.
async fn mint_app_scoped_token(
    db: &DatabaseConnection,
    app_id: Uuid,
) -> Result<ExchangeResponse, String> {
    let generated = oxy_auth::app_publish_token_domain::generate_token();
    let expires_at =
        (chrono::Utc::now() + chrono::Duration::minutes(EXCHANGE_TTL_MINUTES)).fixed_offset();
    entity::app_publish_tokens::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        name: ActiveValue::Set(format!("oidc:{app_id}")),
        token_hash: ActiveValue::Set(generated.token_hash),
        token_prefix: ActiveValue::Set(generated.token_prefix),
        // No human — this is the machine principal (design §6, Option A).
        created_by: ActiveValue::Set(None),
        created_at: ActiveValue::Set(chrono::Utc::now().fixed_offset()),
        last_used_at: ActiveValue::Set(None),
        revoked_at: ActiveValue::Set(None),
        app_id: ActiveValue::Set(Some(app_id)),
        expires_at: ActiveValue::Set(Some(expires_at)),
    }
    .insert(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(ExchangeResponse {
        token: generated.plaintext,
        expires_at: expires_at.to_rfc3339(),
    })
}

// ── publisher registration (staff surface) ──────────────────────────────────
//
// Who may publish which app via OIDC is deliberately explicit: a publisher row is
// registered here, and only a matching workflow can then trust-publish. Bootstrap
// rule (matches crates.io/npm): the app must already exist — a leaked credential
// must not be able to squat a new app name.

#[derive(Deserialize)]
pub struct RegisterPublisherBody {
    pub repo_owner: String,
    /// GitHub's NUMERIC account id — the resurrection defence. The operator reads
    /// it from the org's GitHub settings (or the API); we never accept just a name.
    pub repo_owner_id: i64,
    pub repo_name: String,
    /// Default ".github/workflows/oxy-publish.yml" — what `oxyc init-ci` generates.
    pub workflow_ref: String,
    /// Required — the environment the publish job runs in, so it can be gated
    /// behind required-reviewers.
    pub environment: String,
}

#[derive(Serialize)]
pub struct PublisherDto {
    pub id: Uuid,
    pub app_id: Uuid,
    pub repo_owner: String,
    pub repo_owner_id: i64,
    pub repo_name: String,
    pub workflow_ref: String,
    pub environment: String,
    pub created_at: String,
}

impl From<entity::app_publishers::Model> for PublisherDto {
    fn from(m: entity::app_publishers::Model) -> Self {
        Self {
            id: m.id,
            app_id: m.app_id,
            repo_owner: m.repo_owner,
            repo_owner_id: m.repo_owner_id,
            repo_name: m.repo_name,
            workflow_ref: m.workflow_ref,
            environment: m.environment,
            created_at: m.created_at.to_rfc3339(),
        }
    }
}

/// `GET /customer-apps/{id}/publishers`
pub async fn list_publishers(
    axum::extract::Path(app_id): axum::extract::Path<Uuid>,
) -> Result<Json<Vec<PublisherDto>>, StatusCode> {
    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rows = entity::prelude::AppPublishers::find()
        .filter(entity::app_publishers::Column::AppId.eq(app_id))
        .all(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(rows.into_iter().map(PublisherDto::from).collect()))
}

/// `POST /customer-apps/{id}/publishers`
pub async fn register_publisher(
    axum::extract::Path(app_id): axum::extract::Path<Uuid>,
    oxy_auth::extractor::AuthenticatedUserExtractor(actor): oxy_auth::extractor::AuthenticatedUserExtractor,
    Json(body): Json<RegisterPublisherBody>,
) -> Result<Json<PublisherDto>, StatusCode> {
    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Bootstrap: the app must exist.
    if entity::prelude::Apps::find_by_id(app_id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .is_none()
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let saved = entity::app_publishers::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        app_id: ActiveValue::Set(app_id),
        repo_owner: ActiveValue::Set(body.repo_owner),
        repo_owner_id: ActiveValue::Set(body.repo_owner_id),
        repo_name: ActiveValue::Set(body.repo_name),
        workflow_ref: ActiveValue::Set(body.workflow_ref),
        environment: ActiveValue::Set(body.environment),
        created_by: ActiveValue::Set(Some(actor.id)),
        created_at: ActiveValue::NotSet,
    }
    .insert(&db)
    .await
    // The UNIQUE claim tuple makes a duplicate a 409, not a 500.
    .map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(PublisherDto::from(saved)))
}

/// `DELETE /customer-apps/{id}/publishers/{publisher_id}`
pub async fn delete_publisher(
    axum::extract::Path((app_id, publisher_id)): axum::extract::Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode> {
    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Scope the delete to the `{app_id}` in the URL — otherwise the path segment
    // is decorative and a publisher could be removed via any app's path. Deleting
    // by (id AND app_id) means a mismatched app deletes nothing → 404.
    let res = entity::prelude::AppPublishers::delete_many()
        .filter(entity::app_publishers::Column::Id.eq(publisher_id))
        .filter(entity::app_publishers::Column::AppId.eq(app_id))
        .exec(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if res.rows_affected == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Uuid {
        Uuid::from_u128(1)
    }

    fn publisher() -> PublisherConfig {
        PublisherConfig {
            app_id: app(),
            repo_owner: "acme-consulting".into(),
            repo_owner_id: 42,
            repo_name: "northwind-dashboard".into(),
            workflow_ref: ".github/workflows/oxy-publish.yml".into(),
            environment: "oxy-publish".into(),
        }
    }

    fn claims() -> GithubOidcClaims {
        GithubOidcClaims {
            repository: "acme-consulting/northwind-dashboard".into(),
            repository_owner: "acme-consulting".into(),
            repository_owner_id: "42".into(),
            job_workflow_ref:
                "acme-consulting/northwind-dashboard/.github/workflows/oxy-publish.yml@refs/heads/main"
                    .into(),
            environment: Some("oxy-publish".into()),
            event_name: "push".into(),
            runner_environment: "github-hosted".into(),
            jti: "abc123".into(),
        }
    }

    #[test]
    fn exact_match_returns_the_app() {
        assert_eq!(verify_claims(&claims(), &[publisher()]), Ok(vec![app()]));
    }

    #[test]
    fn case_insensitive_repo_and_env() {
        let mut c = claims();
        c.repository = "Acme-Consulting/Northwind-Dashboard".into();
        c.environment = Some("OXY-PUBLISH".into());
        c.job_workflow_ref =
            "Acme-Consulting/Northwind-Dashboard/.github/workflows/oxy-publish.yml@refs/heads/main"
                .into();
        assert_eq!(verify_claims(&c, &[publisher()]), Ok(vec![app()]));
    }

    #[test]
    fn wrong_repo_name_does_not_match() {
        let mut c = claims();
        c.repository = "acme-consulting/globex-dashboard".into();
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::NoMatchingPublisher)
        );
    }

    #[test]
    fn same_repo_name_different_owner_id_is_rejected() {
        // The resurrection attack: a new account named "acme-consulting" (new
        // numeric id) must not match a publisher registered to the old one, even
        // though the `repository` string is identical.
        let mut c = claims();
        c.repository_owner_id = "999".into();
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::NoMatchingPublisher)
        );
    }

    #[test]
    fn wrong_workflow_ref_does_not_match() {
        // A different workflow file in the same repo — e.g. an attacker's PR adding
        // `.github/workflows/evil.yml` — must not publish.
        let mut c = claims();
        c.job_workflow_ref =
            "acme-consulting/northwind-dashboard/.github/workflows/evil.yml@refs/heads/main".into();
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::NoMatchingPublisher)
        );
    }

    #[test]
    fn wrong_environment_does_not_match() {
        let mut c = claims();
        c.environment = Some("staging".into());
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::NoMatchingPublisher)
        );
    }

    #[test]
    fn missing_environment_is_rejected_outright() {
        let mut c = claims();
        c.environment = None;
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::MissingEnvironment)
        );
    }

    #[test]
    fn pull_request_target_is_rejected() {
        let mut c = claims();
        c.event_name = "pull_request_target".into();
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::PullRequestTarget)
        );
    }

    #[test]
    fn self_hosted_runner_is_rejected() {
        let mut c = claims();
        c.runner_environment = "self-hosted".into();
        assert_eq!(
            verify_claims(&c, &[publisher()]),
            Err(OidcReject::SelfHostedRunner)
        );
    }

    #[test]
    fn monorepo_matches_multiple_apps() {
        // Two apps published from the same repo+workflow+environment.
        let a2 = Uuid::from_u128(2);
        let mut p2 = publisher();
        p2.app_id = a2;
        let got = verify_claims(&claims(), &[publisher(), p2]).unwrap();
        assert!(got.contains(&app()) && got.contains(&a2) && got.len() == 2);
    }

    #[test]
    fn no_publishers_never_matches() {
        assert_eq!(
            verify_claims(&claims(), &[]),
            Err(OidcReject::NoMatchingPublisher)
        );
    }
}
