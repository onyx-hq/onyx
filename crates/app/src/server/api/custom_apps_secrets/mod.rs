//! App-scoped secret management — the write path for `apps/<app_id>/<KEY>`, and
//! the reconciled view that says which keys an app is still missing.
//!
//! # Why this module exists
//!
//! `ctx.env` has always read secrets named `apps/<app_id>/<KEY>` out of the
//! project secret manager, but nothing could **create** one. The public secrets
//! API and the settings UI both reject `/` in a name (`validate_secret_name`),
//! so the only writer was `ctx.secrets.set` from inside a function — which
//! cannot bootstrap the secret the function needs in order to run. A `webhook:`
//! block was the sharpest form of it: 401 on every delivery, and no way to set
//! the signing key it named. (The docs told people to use Settings → Secrets;
//! that never worked.)
//!
//! Everything *else* about an app-scoped secret already worked through the
//! existing `/api/workspaces/{id}/secrets` routes, because those address by row
//! id rather than by name — list, reveal, rotate and delete all resolve fine.
//! So this module is deliberately narrow: it owns creation, and it owns the
//! app-shaped *view* (bare keys, grouped under one app, reconciled against what
//! the build declares) that made the raw `apps/<uuid>/KEY` rows unreadable.
//!
//! # Mounts
//!
//! One handler set, two mounts, each keeping the guard already on its surface:
//!
//! | Mount | Guard |
//! | --- | --- |
//! | `/api/customer-apps/{id}/secrets` | owner **or** app-admin, beside `/{id}/api-keys` |
//! | `/api/workspaces/{ws}/custom-apps/{app_id}/secrets` (POST only) | `WorkspaceAdmin` |
//!
//! No `/admin/apps/{id}/secrets` twin: `/customer-apps` is already reachable by
//! owners as well as app-admins (`oxy_owner_or_app_admin_guard`), and it is the
//! surface the staff console actually calls for app work — publish, builds,
//! functions, storage, api-keys. A second mount would add a route, not reach.
//!
//! The staff mount reaches any app by id, exactly as the rest of that surface
//! does. **The workspace mount must not** — see [`scoped_to_workspace`].
//!
//! All of it is Postgres-only (no workspace FS, no git), so every route is
//! `FleetOk`.

pub(crate) mod declared;

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use entity::{app_builds, app_functions, apps};
use oxy::database::client::establish_connection;
use oxy::service::secret_manager::SecretManagerService;
use oxy_app_core::audit;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_auth::types::AuthenticatedUser;
use oxy_server_authz::role_guards::WorkspaceAdmin;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use declared::{AppSecretEntry, StoredSecret};

/// Whether a submitted value would land as the empty string.
///
/// Split out and named so the trim is pinned by a test rather than by a reader
/// noticing it — the bug this replaced was a bare `is_empty()`, which looks
/// correct until you read `sanitize_secret_value` two crates away.
fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

/// Storage-name prefix for one app's secrets. The single definition — every
/// read strips it and every write is built from it, so the two can't drift.
fn prefix_for(app_id: Uuid) -> String {
    format!("apps/{app_id}/")
}

// ── DTOs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AppSecretsResponse {
    pub app_id: Uuid,
    pub app_slug: String,
    pub app_name: String,
    /// Declared ∪ stored, action-first. See [`declared::reconcile`].
    pub entries: Vec<AppSecretEntry>,
    /// Count of `required` keys with nothing stored — the badge the UI shows on
    /// the tab, and the one number that says "this app is not ready".
    pub missing_required: usize,
    /// The build whose declarations were read (published, else draft). `None`
    /// when the app has never been built, in which case nothing is declared and
    /// every stored key reads as undeclared — which is correct, not a bug.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaring_build_id: Option<String>,
    /// Set when the manifest's `env` block exists but could not be parsed.
    /// Surfaced rather than swallowed, so a typo doesn't look like an app that
    /// declares nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SetSecretRequest {
    /// Bare key — `STRIPE_API_KEY`, not the prefixed storage name. The prefix is
    /// system-built by `set_app_secret`, which is what keeps an app's write
    /// inside its own namespace.
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct RevealResponse {
    pub key: String,
    pub value: String,
}

// ── App resolution + scoping ─────────────────────────────────────────────────

type Failure = (StatusCode, String);

async fn connect() -> Result<DatabaseConnection, Failure> {
    establish_connection().await.map_err(|e| {
        tracing::error!("custom_apps_secrets DB connect failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "database unavailable".to_string(),
        )
    })
}

async fn load_app(db: &DatabaseConnection, app_id: Uuid) -> Result<apps::Model, Failure> {
    apps::Entity::find_by_id(app_id)
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("app lookup failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "app lookup failed".to_string(),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "app not found".to_string()))
}

/// Confine a tenant-surface request to its own workspace.
///
/// **This is the one place a cross-tenant read could open.** An app's secrets
/// live in the project secret store keyed by `apps.project_id`; a workspace
/// admin passing another org's `app_id` on their own workspace's route would
/// otherwise read (and rotate) that app's secrets, because `WorkspaceAdmin`
/// only proves standing in the workspace named in the path — never that the app
/// belongs to it.
///
/// Answers **404, not 403**: on a scoped surface, "not yours" and "does not
/// exist" must be indistinguishable, or the endpoint becomes a probe for which
/// app ids exist. Same reasoning as the App Operator scope boundary.
fn scoped_to_workspace(app: apps::Model, workspace_id: Uuid) -> Result<apps::Model, Failure> {
    if app.project_id != workspace_id {
        tracing::warn!(
            app_id = %app.id,
            requested_workspace = %workspace_id,
            owning_workspace = %app.project_id,
            "app-secrets request for an app outside the path workspace — answering 404"
        );
        return Err((StatusCode::NOT_FOUND, "app not found".to_string()));
    }
    Ok(app)
}

// ── Core operations ──────────────────────────────────────────────────────────

/// The build whose declarations are in force: published, else draft. Matches how
/// `list_functions` and the storage sweeper pick a manifest — the declarations
/// have to come from the same build as the code that reads them.
fn declaring_build(app: &apps::Model) -> Option<Uuid> {
    app.published_build_id.or(app.draft_build_id)
}

/// What the active build declares, plus why the answer might be incomplete.
///
/// A struct rather than a tuple because the third field is the interesting one:
/// every path that yields *no* declarations has to say whether that means "this
/// app declares nothing" or "we could not find out".
struct Declarations {
    keys: std::collections::BTreeMap<String, declared::DeclaredKey>,
    /// Non-`None` when the list is not the whole truth — a malformed `env`
    /// block, or a query that failed.
    error: Option<String>,
    build_id: Option<String>,
}

impl Declarations {
    /// The app has no build at all. Genuinely declares nothing — not an error.
    fn none() -> Self {
        Self {
            keys: Default::default(),
            error: None,
            build_id: None,
        }
    }

    /// We could not read the declarations. Distinct from `none()` on purpose:
    /// silently returning zero keys is the exact failure this module argues
    /// against for a malformed manifest — every stored key would flip to
    /// **Undeclared**, `missing_required` would fall to 0, and the header badge
    /// would vanish, all reading as a healthy app.
    fn unreadable(reason: &str) -> Self {
        Self {
            keys: Default::default(),
            error: Some(format!(
                "Could not read this app's declared keys ({reason}), so only what is \
                 already stored is listed below."
            )),
            build_id: None,
        }
    }
}

async fn build_declarations(db: &DatabaseConnection, app: &apps::Model) -> Declarations {
    let Some(build_pk) = declaring_build(app) else {
        return Declarations::none();
    };

    let build = match app_builds::Entity::find_by_id(build_pk).one(db).await {
        // A build row that has gone away declares nothing, which is true rather
        // than unknown — only the query FAILING is unknown.
        Ok(row) => row,
        Err(e) => {
            tracing::error!(app_id = %app.id, %build_pk, "build lookup failed: {e}");
            return Declarations::unreadable("its build could not be loaded");
        }
    };

    let (env, manifest_error) = declared::declared_env(
        build.as_ref().and_then(|b| b.manifest_json.as_ref()),
        app.id,
    );

    // Every function in the same build, for their `webhook.secretVar`s.
    let fn_manifests: Vec<serde_json::Value> = match app_functions::Entity::find()
        .filter(app_functions::Column::BuildId.eq(build_pk))
        .all(db)
        .await
    {
        Ok(rows) => rows.into_iter().filter_map(|r| r.manifest_json).collect(),
        Err(e) => {
            tracing::error!(app_id = %app.id, "function lookup failed: {e}");
            return Declarations::unreadable("its functions could not be loaded");
        }
    };

    Declarations {
        keys: declared::merge_declared(env, declared::webhook_secret_vars(fn_manifests.iter())),
        error: manifest_error,
        build_id: build.map(|b| b.build_id),
    }
}

/// Everything stored under this app's prefix, keys bared and attributed.
async fn stored_secrets(
    db: &DatabaseConnection,
    app: &apps::Model,
) -> Result<Vec<StoredSecret>, Failure> {
    let manager = SecretManagerService::new(app.project_id);
    let prefix = prefix_for(app.id);

    let rows = manager.list_secrets(db).await.map_err(|e| {
        tracing::error!(app_id = %app.id, "listing secrets failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not list secrets".to_string(),
        )
    })?;

    let scoped: Vec<_> = rows
        .into_iter()
        .filter_map(|s| {
            let key = s.name.strip_prefix(&prefix)?.to_string();
            Some((key, s))
        })
        .collect();

    // One batch lookup for the "last changed by" column, same as `list_secrets`.
    let actor_ids: Vec<Uuid> = scoped
        .iter()
        .filter_map(|(_, s)| s.updated_by.or(Some(s.created_by)))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let emails = crate::server::api::secrets::resolve_user_emails(db, &actor_ids).await;

    Ok(scoped
        .into_iter()
        .map(|(key, s)| StoredSecret {
            key,
            secret_id: s.id,
            updated_at: s.updated_at,
            updated_by_email: s
                .updated_by
                .or(Some(s.created_by))
                .and_then(|id| emails.get(&id).cloned()),
        })
        .collect())
}

async fn view(db: &DatabaseConnection, app: apps::Model) -> Result<AppSecretsResponse, Failure> {
    let stored = stored_secrets(db, &app).await?;
    let declarations = build_declarations(db, &app).await;
    let entries = declared::reconcile(declarations.keys, stored);
    Ok(AppSecretsResponse {
        missing_required: entries.iter().filter(|e| e.is_missing_required()).count(),
        app_id: app.id,
        app_slug: app.slug,
        app_name: app.name,
        entries,
        declaring_build_id: declarations.build_id,
        declaration_error: declarations.error,
    })
}

async fn set(
    db: &DatabaseConnection,
    app: &apps::Model,
    body: SetSecretRequest,
    actor: Uuid,
) -> Result<StatusCode, Failure> {
    let key = body.key.trim().to_string();
    // `trim()`, not `is_empty()`. An empty value resolves to an empty
    // `ctx.env.KEY`, which reads as "configured" everywhere downstream while
    // behaving like unset — the `usable_secret` / `timingSafeEqual` footguns
    // both start here, and for a `webhook.secretVar` it means verifying an HMAC
    // against an empty key instead of answering 401 with a nameable cause.
    //
    // A whitespace-only value is that same state wearing a disguise:
    // `sanitize_secret_value` tests emptiness BEFORE it trims and then stores
    // the trimmed string, so `" "` clears both its check and a bare
    // `is_empty()` here and lands as `""`. Deleting is how you unset.
    if is_blank(&body.value) {
        return Err((
            StatusCode::BAD_REQUEST,
            "value must not be empty or whitespace — delete the secret instead of blanking it"
                .to_string(),
        ));
    }

    SecretManagerService::new(app.project_id)
        .set_app_secret(db, app.id, &key, &body.value, actor)
        .await
        .map_err(|e| {
            // `set_app_secret` validates the caller's key against the name
            // charset; that's a request problem, not a server one.
            tracing::warn!(app_id = %app.id, "setting app secret failed: {e}");
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

    tracing::info!(app_id = %app.id, secret.key = %key, actor = %actor, "app secret set");
    Ok(StatusCode::NO_CONTENT)
}

async fn delete(
    db: &DatabaseConnection,
    app: &apps::Model,
    key: &str,
    actor: &AuthenticatedUser,
) -> Result<StatusCode, Failure> {
    // Trimmed, like `set` — otherwise `DELETE …/secrets/%20SIG` 404s on a key
    // the sibling POST would have normalised to `SIG`.
    let key = key.trim();
    let name = format!("{}{key}", prefix_for(app.id));
    SecretManagerService::new(app.project_id)
        .delete_secret(db, &name)
        .await
        .map_err(|e| {
            tracing::warn!(app_id = %app.id, "deleting app secret failed: {e}");
            // `delete_secret` distinguishes "no such row" from a database
            // failure by variant; collapsing both to 404 would report an
            // outage as a missing key.
            let code = match e {
                oxy_shared::errors::OxyError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::NOT_FOUND,
            };
            (code, e.to_string())
        })?;
    tracing::info!(
        app_id = %app.id,
        secret.key = %key,
        actor = %actor.label(),
        "app secret deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `SecretManagerService::get_secret` owns its own connection (and a 300s
/// decrypt cache), so unlike the other operations this one takes no `db`.
async fn reveal(
    db: &DatabaseConnection,
    app: &apps::Model,
    key: &str,
    actor: &AuthenticatedUser,
) -> Result<Json<RevealResponse>, Failure> {
    // Parity with project secrets, which are already revealable by id on the
    // existing route — an app-scoped one is not a different kind of secret, and
    // pretending otherwise would just push people back to the raw table.
    let key = key.trim();
    let name = format!("{}{key}", prefix_for(app.id));
    let value = SecretManagerService::new(app.project_id)
        .get_secret(&name)
        .await
        .ok_or((StatusCode::NOT_FOUND, "secret not found".to_string()))?;

    // The one action here that hands a human plaintext tenant secret material,
    // reachable by any in-scope App Operator — so it is the one worth a durable
    // record rather than only a log line. Best-effort: failing to write the
    // audit row must not fail the read the operator is entitled to, and the
    // helper logs its own failure.
    audit::record_best_effort(
        db,
        audit::AuditEntry::new(actor.label().to_string(), "custom_app.secret.revealed")
            .actor(actor.id, audit::ActorType::User)
            .org(app.org_id)
            .workspace(app.project_id)
            .target(
                "custom_app_secret",
                name.clone(),
                format!("{}/{key}", app.slug),
            ),
    )
    .await;

    tracing::info!(
        app_id = %app.id,
        secret.key = %key,
        actor = %actor.label(),
        "app secret revealed"
    );
    Ok(Json(RevealResponse {
        key: key.to_string(),
        value,
    }))
}

// ── Staff-surface handlers (`/admin/apps/{id}`, `/customer-apps/{id}`) ───────

pub async fn admin_list(Path(app_id): Path<Uuid>) -> Result<Json<AppSecretsResponse>, Failure> {
    let db = connect().await?;
    let app = load_app(&db, app_id).await?;
    Ok(Json(view(&db, app).await?))
}

pub async fn admin_set(
    Path(app_id): Path<Uuid>,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(body): Json<SetSecretRequest>,
) -> Result<StatusCode, Failure> {
    let db = connect().await?;
    let app = load_app(&db, app_id).await?;
    set(&db, &app, body, user.id).await
}

pub async fn admin_delete(
    Path((app_id, key)): Path<(Uuid, String)>,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
) -> Result<StatusCode, Failure> {
    let db = connect().await?;
    let app = load_app(&db, app_id).await?;
    delete(&db, &app, &key, &user).await
}

pub async fn admin_reveal(
    Path((app_id, key)): Path<(Uuid, String)>,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
) -> Result<Json<RevealResponse>, Failure> {
    let db = connect().await?;
    let app = load_app(&db, app_id).await?;
    reveal(&db, &app, &key, &user).await
}

// ── Tenant-surface handler (`/workspaces/{ws}/custom-apps/{app_id}`) ────────

/// `POST /api/workspaces/{ws}/custom-apps/{app_id}/secrets` — the tenant write
/// path, and **the only tenant verb here**.
///
/// Everything else an app secret needs already works on the project-secrets
/// routes next door, because those address a row by **id** rather than by name:
/// `GET /secrets` lists it, `GET /secrets/{id}/value` reveals it, `PUT` rotates
/// it, `DELETE` removes it. Only creation was impossible, because
/// `validate_secret_name` rejects the `/` in `apps/<app_id>/<KEY>` — so that is
/// the one thing added, rather than a parallel CRUD surface whose other three
/// verbs would duplicate working routes and have to be kept in step with them.
pub async fn workspace_set(
    _: WorkspaceAdmin,
    Path((workspace_id, app_id)): Path<(Uuid, Uuid)>,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(body): Json<SetSecretRequest>,
) -> Result<StatusCode, Failure> {
    let db = connect().await?;
    let app = scoped_to_workspace(load_app(&db, app_id).await?, workspace_id)?;
    set(&db, &app, body, user.id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_row(project_id: Uuid) -> apps::Model {
        apps::Model {
            id: Uuid::new_v4(),
            slug: "bookkeeping".to_string(),
            name: "Bookkeeping".to_string(),
            org_id: Uuid::new_v4(),
            project_id,
            branch: "main".to_string(),
            source_repo: "oxy-hq/customer-apps".to_string(),
            status: "created".to_string(),
            source_type: "s3".to_string(),
            source_config: serde_json::json!({}),
            last_synced_at: None,
            manifest_override: None,
            bootstrap_pr_url: None,
            published_at: None,
            repo_path: None,
            draft_build_id: None,
            published_build_id: None,
            last_promoted_by: None,
            last_promoted_at: None,
            visibility: "org".to_string(),
            created_at: chrono::Utc::now().fixed_offset(),
            updated_at: chrono::Utc::now().fixed_offset(),
        }
    }

    /// Regression: `sanitize_secret_value` tests emptiness BEFORE trimming and
    /// then stores the trimmed string, so a whitespace-only value cleared a bare
    /// `is_empty()` guard here and landed as `""` — the "configured but behaves
    /// like unset" state this guard exists to prevent.
    #[test]
    fn whitespace_is_not_a_value() {
        assert!(is_blank(""));
        assert!(is_blank(" "), "a space would otherwise be stored as empty");
        assert!(is_blank("\t\n  "));
        assert!(!is_blank("sk_test_123"));
        assert!(!is_blank("  padded  "), "a real value survives its padding");
    }

    #[test]
    fn prefix_is_the_namespace_ctx_env_reads() {
        let id = Uuid::nil();
        assert_eq!(prefix_for(id), format!("apps/{id}/"));
    }

    #[test]
    fn an_app_in_this_workspace_passes() {
        let ws = Uuid::new_v4();
        assert!(scoped_to_workspace(app_row(ws), ws).is_ok());
    }

    /// The cross-tenant guard. `WorkspaceAdmin` proves standing in the workspace
    /// named in the PATH — never that the app belongs to it.
    #[test]
    fn an_app_from_another_workspace_is_not_found() {
        let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());
        let err = scoped_to_workspace(app_row(theirs), mine).unwrap_err();
        assert_eq!(
            err.0,
            StatusCode::NOT_FOUND,
            "must be indistinguishable from a nonexistent app, or the route \
             becomes a probe for which app ids exist"
        );
    }

    #[test]
    fn published_build_declares_over_draft() {
        let mut app = app_row(Uuid::new_v4());
        let (published, draft) = (Uuid::new_v4(), Uuid::new_v4());
        app.draft_build_id = Some(draft);
        assert_eq!(declaring_build(&app), Some(draft), "draft when unpublished");
        app.published_build_id = Some(published);
        assert_eq!(declaring_build(&app), Some(published));
    }
}
