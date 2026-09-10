use crate::server::api::middlewares::role_guards::WorkspaceAdmin;
use crate::server::api::middlewares::workspace_context::WorkspaceManagerWorkingCopy;
use crate::server::service::secret_manager::{
    CreateSecretParams, SecretInfo, SecretManagerService, UpdateSecretParams,
};
use axum::{
    extract::{self, Path},
    http::StatusCode,
    response::IntoResponse,
};
use entity::users::Entity as Users;
use garde::Validate;
use oxy::config::constants::{ANTHROPIC_API_KEY_VAR, GEMINI_API_KEY_VAR, OPENAI_API_KEY_VAR};
use oxy::config::model::{DatabaseType, IntegrationType, SnowflakeAuthType};
use oxy::database::client::establish_connection;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_shared::errors::OxyError;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Serialize)]
pub struct SecretResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub created_by: String,
    pub created_by_email: Option<String>,
    pub updated_by_email: Option<String>,
    pub is_active: bool,
}

impl From<SecretInfo> for SecretResponse {
    fn from(secret: SecretInfo) -> Self {
        Self {
            id: secret.id.to_string(),
            name: secret.name,
            description: secret.description,
            created_at: secret.created_at,
            updated_at: secret.updated_at,
            created_by: secret.created_by.to_string(),
            created_by_email: None, // populated by list_secrets after user lookup
            updated_by_email: None, // populated by list_secrets after user lookup
            is_active: secret.is_active,
        }
    }
}

/// Resolve a set of user UUIDs to their emails in one query.
pub(crate) async fn resolve_user_emails(
    db: &impl sea_orm::ConnectionTrait,
    ids: &[Uuid],
) -> HashMap<Uuid, String> {
    if ids.is_empty() {
        return HashMap::new();
    }
    match Users::find()
        .filter(entity::users::Column::Id.is_in(ids.to_vec()))
        .all(db)
        .await
    {
        Ok(users) => users
            .into_iter()
            .map(|u| (u.id, u.label().to_string()))
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to resolve user emails: {}", e);
            HashMap::new()
        }
    }
}

#[derive(Serialize)]
pub struct SecretListResponse {
    pub secrets: Vec<SecretResponse>,
    pub total: usize,
}

#[derive(Deserialize, Validate, Serialize)]
pub struct CreateSecretRequest {
    #[garde(length(min = 1, max = 255))]
    pub name: String,
    #[garde(length(min = 1, max = 10000))]
    pub value: String,
    #[garde(length(min = 0, max = 1000))]
    pub description: Option<String>,
}

#[derive(Deserialize, Validate)]
pub struct BulkCreateSecretsRequest {
    #[garde(length(min = 1, max = 100), dive)]
    pub secrets: Vec<CreateSecretRequest>,
}

#[derive(Serialize)]
pub struct FailedSecret {
    pub secret: CreateSecretRequest,
    pub error: String,
}

#[derive(Serialize)]
pub struct BulkCreateSecretsResponse {
    pub created_secrets: Vec<SecretResponse>,
    pub failed_secrets: Vec<FailedSecret>,
}

#[derive(Deserialize, Validate)]
pub struct UpdateSecretRequest {
    #[garde(length(min = 1, max = 10000))]
    pub value: Option<String>,
    #[garde(length(min = 0, max = 1000))]
    pub description: Option<String>,
}

pub async fn create_secret(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Path(workspace_id): Path<Uuid>,
    extract::Json(request): extract::Json<CreateSecretRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if let Err(validation_errors) = request.validate() {
        tracing::warn!("Secret creation validation failed: {}", validation_errors);
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "error": "Validation failed",
                "details": validation_errors.to_string()
            })),
        )
            .into_response());
    }

    // Reject secrets whose names collide with infrastructure / auth env vars.
    // These vars are owned by the server operator and must not be overridable
    // through the per-workspace secrets panel.
    if is_auth_env_var(&request.name) {
        tracing::warn!(
            "Rejected secret creation: name '{}' is reserved for infrastructure config",
            request.name
        );
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "error": format!(
                    "'{}' is a reserved environment variable name used by Oxygen's infrastructure. \
                     Choose a different name for your secret.",
                    request.name
                )
            })),
        )
            .into_response());
    }

    let db = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let secret_manager = SecretManagerService::new(workspace_id);

    let create_params = CreateSecretParams {
        name: request.name,
        value: request.value,
        description: request.description,
        created_by: user.id,
    };

    match secret_manager.create_secret(&db, create_params).await {
        Ok(secret_info) => {
            let response = SecretResponse::from(secret_info);
            Ok((StatusCode::CREATED, axum::Json(response)).into_response())
        }
        Err(OxyError::SecretManager(msg)) if msg.contains("already exists") => {
            Ok((StatusCode::CONFLICT, axum::Json(json!({ "error": msg }))).into_response())
        }
        Err(OxyError::SecretManager(msg)) => {
            Ok((StatusCode::BAD_REQUEST, axum::Json(json!({ "error": msg }))).into_response())
        }
        Err(e) => {
            tracing::error!("Failed to create secret: {}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to create secret" })),
            )
                .into_response())
        }
    }
}

pub async fn bulk_create_secrets(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Path(workspace_id): Path<Uuid>,
    extract::Json(request): extract::Json<BulkCreateSecretsRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if let Err(validation_errors) = request.validate() {
        tracing::warn!(
            "Bulk secret creation validation failed: {}",
            validation_errors
        );
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "error": "Validation failed",
                "details": validation_errors.to_string()
            })),
        )
            .into_response());
    }

    let db = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let secret_manager = SecretManagerService::new(workspace_id);
    let mut created_secrets = Vec::new();
    let mut failed_secrets = Vec::new();

    // Process each secret individually
    for secret_request in request.secrets {
        let create_params = CreateSecretParams {
            name: secret_request.name.clone(),
            value: secret_request.value.clone(),
            description: secret_request.description.clone(),
            created_by: user.id,
        };

        match secret_manager.create_secret(&db, create_params).await {
            Ok(secret_info) => {
                let response = SecretResponse::from(secret_info);
                created_secrets.push(response);
            }
            Err(e) => {
                let error_msg = match e {
                    OxyError::SecretManager(msg) => msg,
                    _ => "Failed to create secret".to_string(),
                };
                failed_secrets.push(FailedSecret {
                    secret: secret_request,
                    error: error_msg,
                });
            }
        }
    }

    let response = BulkCreateSecretsResponse {
        created_secrets,
        failed_secrets,
    };

    // Return 201 if all succeeded, 207 (Multi-Status) if partial success, 400 if all failed
    let status_code = if response.failed_secrets.is_empty() {
        StatusCode::CREATED
    } else if !response.created_secrets.is_empty() {
        StatusCode::MULTI_STATUS
    } else {
        StatusCode::BAD_REQUEST
    };

    Ok((status_code, axum::Json(response)).into_response())
}

/// List all secrets (without values)
pub async fn list_secrets(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(_user): AuthenticatedUserExtractor,
    Path(workspace_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let secret_manager = SecretManagerService::new(workspace_id);
    match secret_manager.list_secrets(&db).await {
        Ok(secrets) => {
            // Resolve created_by and updated_by UUIDs → emails in one batch query
            let user_ids: Vec<Uuid> = secrets
                .iter()
                .flat_map(|s| std::iter::once(s.created_by).chain(s.updated_by))
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            let email_map = resolve_user_emails(&db, &user_ids).await;

            let secret_responses: Vec<SecretResponse> = secrets
                .into_iter()
                .map(|s| {
                    let created_by_email = email_map.get(&s.created_by).cloned();
                    let updated_by_email = s.updated_by.and_then(|id| email_map.get(&id).cloned());
                    SecretResponse {
                        created_by_email,
                        updated_by_email,
                        ..SecretResponse::from(s)
                    }
                })
                .collect();

            let response = SecretListResponse {
                total: secret_responses.len(),
                secrets: secret_responses,
            };

            Ok((StatusCode::OK, axum::Json(response)).into_response())
        }
        Err(e) => {
            tracing::error!("Failed to list secrets: {}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to list secrets" })),
            )
                .into_response())
        }
    }
}

/// Get secret metadata by ID (without value)
pub async fn get_secret(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(_user): AuthenticatedUserExtractor,
    Path((workspace_id, id)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let secret_id = match Uuid::parse_str(&id) {
        Ok(uuid) => uuid,
        Err(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "Invalid secret ID format" })),
            )
                .into_response());
        }
    };

    let secret_manager = SecretManagerService::new(workspace_id);

    match secret_manager.get_secret_by_id(secret_id).await {
        Some(secret) => {
            Ok((StatusCode::OK, axum::Json(SecretResponse::from(secret))).into_response())
        }
        None => Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "Secret not found" })),
        )
            .into_response()),
    }
}

pub async fn update_secret(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Path((workspace_id, id)): Path<(Uuid, String)>,
    extract::Json(request): extract::Json<UpdateSecretRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    if request.value.is_none() && request.description.is_none() {
        return Ok((
            StatusCode::BAD_REQUEST,
            axum::Json(
                json!({ "error": "At least one of 'value' or 'description' must be provided" }),
            ),
        )
            .into_response());
    }

    if let Err(validation_errors) = request.validate() {
        tracing::warn!("Secret update validation failed: {}", validation_errors);
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "error": "Validation failed",
                "details": validation_errors.to_string()
            })),
        )
            .into_response());
    }

    let secret_id = match Uuid::parse_str(&id) {
        Ok(uuid) => uuid,
        Err(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "Invalid secret ID format" })),
            )
                .into_response());
        }
    };

    let db = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let secret_manager = SecretManagerService::new(workspace_id);

    let Some(secret) = secret_manager.get_secret_by_id(secret_id).await else {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "Secret not found" })),
        )
            .into_response());
    };

    let update_params = UpdateSecretParams {
        value: request.value,
        description: request.description,
        updated_by: user.id,
    };

    match secret_manager
        .update_secret(&db, &secret.name, update_params)
        .await
    {
        Ok(updated_secret) => Ok((
            StatusCode::OK,
            axum::Json(SecretResponse::from(updated_secret)),
        )
            .into_response()),
        Err(OxyError::SecretManager(msg)) => {
            Ok((StatusCode::BAD_REQUEST, axum::Json(json!({ "error": msg }))).into_response())
        }
        Err(e) => {
            tracing::error!("Failed to update secret: {}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to update secret" })),
            )
                .into_response())
        }
    }
}

/// Masks a secret value for safe display.
/// Shows first 4 + variable stars + last 4 for values longer than 8 chars; otherwise all stars.
/// Uses char-boundary safe indexing to handle multi-byte UTF-8 characters.
fn mask_secret_value(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let len = chars.len();
    if len <= 8 {
        return "*".repeat(len);
    }
    let start: String = chars[..4].iter().collect();
    let end: String = chars[len - 4..].iter().collect();
    format!("{}{}{}", start, "*".repeat(len - 8), end)
}

/// Parses a `.env` file and returns `(key, value)` pairs.
/// Skips blank lines, comment lines (`#`), and lines without `=`.
fn parse_dotenv_file(path: &std::path::Path) -> Vec<(String, String)> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let eq_pos = line.find('=')?;
            let key = line[..eq_pos].trim().to_string();
            if key.is_empty() {
                return None;
            }
            let raw = line[eq_pos + 1..].to_string();
            // Strip surrounding single or double quotes; for unquoted values strip inline comments
            let value = if raw.len() >= 2
                && ((raw.starts_with('"') && raw.ends_with('"'))
                    || (raw.starts_with('\'') && raw.ends_with('\'')))
            {
                raw[1..raw.len() - 1].to_string()
            } else {
                // Strip inline comment: anything after the first " #" (space + hash)
                let stripped = raw.split(" #").next().unwrap_or(&raw).trim_end();
                stripped.to_string()
            };
            Some((key, value))
        })
        .collect()
}

/// Environment variable name prefixes that belong to the authentication layer
/// (Okta, Google OAuth, magic-link) or Oxy's own infrastructure config.
/// These are server-side secrets that have no business appearing in the
/// per-workspace secrets panel — users should never need to view or override them there.
const AUTH_ENV_VAR_PREFIXES: &[&str] = &[
    "GOOGLE_CLIENT_",  // GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET
    "OKTA_",           // OKTA_CLIENT_ID, OKTA_CLIENT_SECRET, OKTA_DOMAIN
    "MAGIC_LINK_",     // all magic-link config vars
    "OXY_",            // internal Oxy infrastructure vars (OXY_DATABASE_URL, OXY_OWNER, …)
    "GIT_REPOSITORY_", // GIT_REPOSITORY_URL
    "GITHUB_",         // all GitHub vars: app config, OAuth login, webhooks, CI env vars
];

fn is_auth_env_var(name: &str) -> bool {
    AUTH_ENV_VAR_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Env vars that Oxy reads internally by default (not via config.yml *_var fields).
/// Always shown in the secrets panel so users know what the app needs, even when they
/// haven't yet referenced these vars in config.yml or .env.
const BUILT_IN_VARS: &[(&str, &str)] = &[
    // Read directly (not via config.yml) by intent classification
    // (crates/core/src/intent/types.rs) and Tier 2 metrics extraction
    // (crates/core/src/metrics/{context,extractor}.rs).
    (OPENAI_API_KEY_VAR, "intent + metrics extraction"),
    // Defaults for LLM model configs when created without a custom key_var.
    (ANTHROPIC_API_KEY_VAR, "models (Anthropic default)"),
    (GEMINI_API_KEY_VAR, "models (Google default)"),
];

/// Where a secret's value is currently set.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    /// Value is defined in the project's .env file.
    DotEnv,
    /// Value is set in the process/shell environment (not via .env).
    Environment,
    /// Variable is not currently set.
    NotSet,
}

/// Info about a secret environment variable known to Oxy.
#[derive(Serialize)]
pub struct EnvSecretInfo {
    /// The environment variable name (e.g. "SLACK_BOT_TOKEN")
    pub env_var: String,
    /// Where Oxy references this variable: a config.yml field path
    /// (e.g. "models[0].key_var") or built-in label (e.g. "embeddings").
    /// None if the variable appears only in .env and is not referenced by config.
    pub referenced_by: Option<String>,
    /// Where the secret value is currently set.
    pub source: SecretSource,
    /// Whether the environment variable is currently set (non-empty value)
    pub is_set: bool,
    /// Masked value of the env var if set (e.g. "sk-a****bcde"), None if not set.
    ///
    /// Only a masked value is ever exposed for env-sourced secrets — the raw
    /// plaintext is never returned. Env vars are resolved by the server at
    /// runtime; clients have no business seeing the raw value.
    pub masked_value: Option<String>,
}

/// List all environment-variable-referenced secrets from config.yml.
///
/// Scans the project config for `*_var` fields and returns their status.
/// This helps users see which secrets are being read from the environment
/// and potentially override them with database-stored secrets.
pub async fn list_env_secrets(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(_user): AuthenticatedUserExtractor,
    WorkspaceManagerWorkingCopy(workspace_manager): WorkspaceManagerWorkingCopy,
    Path(_workspace_id): Path<Uuid>,
) -> Result<impl IntoResponse, StatusCode> {
    let config = workspace_manager.config_manager.get_config();
    let mut env_secrets: Vec<EnvSecretInfo> = Vec::new();

    let mut seen_vars: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Parse .env upfront so source resolution works for all vars, including config-referenced ones.
    let dotenv_path = workspace_manager
        .config_manager
        .workspace_path()
        .join(".env");
    let dotenv_entries = parse_dotenv_file(&dotenv_path);
    let dotenv_keys: std::collections::HashSet<String> =
        dotenv_entries.iter().map(|(k, _)| k.clone()).collect();

    let make_entry = |env_var: String, referenced_by: Option<String>| {
        let value = std::env::var(&env_var).ok().filter(|v| !v.is_empty());
        // Note: DotEnv means the key is declared in the project's .env file.
        // is_set reflects std::env::var(), which reads vars loaded at startup
        // from the CWD .env (via dotenv().ok() in main.rs). If the project path
        // differs from the startup CWD, source may be DotEnv while is_set is
        // false — accurate but potentially surprising in non-standard setups.
        let source = if dotenv_keys.contains(&env_var) {
            SecretSource::DotEnv
        } else if value.is_some() {
            SecretSource::Environment
        } else {
            SecretSource::NotSet
        };
        EnvSecretInfo {
            source,
            referenced_by,
            is_set: value.is_some(),
            masked_value: value.as_deref().map(mask_secret_value),
            env_var,
        }
    };

    // Scan integrations
    for integration in &config.integrations {
        match &integration.integration_type {
            IntegrationType::Omni(omni) => {
                seen_vars.insert(omni.api_key_var.clone());
                env_secrets.push(make_entry(
                    omni.api_key_var.clone(),
                    Some(format!("integrations.{}.api_key_var", integration.name)),
                ));
            }
            IntegrationType::Looker(looker) => {
                seen_vars.insert(looker.client_id_var.clone());
                env_secrets.push(make_entry(
                    looker.client_id_var.clone(),
                    Some(format!("integrations.{}.client_id_var", integration.name)),
                ));
                seen_vars.insert(looker.client_secret_var.clone());
                env_secrets.push(make_entry(
                    looker.client_secret_var.clone(),
                    Some(format!(
                        "integrations.{}.client_secret_var",
                        integration.name
                    )),
                ));
            }
            IntegrationType::Toast(toast) => {
                seen_vars.insert(toast.webhook_secret_var.clone());
                env_secrets.push(make_entry(
                    toast.webhook_secret_var.clone(),
                    Some(format!(
                        "integrations.{}.webhook_secret_var",
                        integration.name
                    )),
                ));
            }
            IntegrationType::ToastAnalytics(ta) => {
                // Each auth var is optional (OAuth pair OR static token); register
                // only the ones the integration actually references.
                for (var, field) in [
                    (&ta.client_id_var, "client_id_var"),
                    (&ta.client_secret_var, "client_secret_var"),
                    (&ta.api_token_var, "api_token_var"),
                ] {
                    if let Some(v) = var {
                        seen_vars.insert(v.clone());
                        env_secrets.push(make_entry(
                            v.clone(),
                            Some(format!("integrations.{}.{}", integration.name, field)),
                        ));
                    }
                }
            }
            IntegrationType::OpenWeatherMap(owm) => {
                seen_vars.insert(owm.api_key_var.clone());
                env_secrets.push(make_entry(
                    owm.api_key_var.clone(),
                    Some(format!("integrations.{}.api_key_var", integration.name)),
                ));
            }
            IntegrationType::BestTime(bt) => {
                seen_vars.insert(bt.api_key_var.clone());
                env_secrets.push(make_entry(
                    bt.api_key_var.clone(),
                    Some(format!("integrations.{}.api_key_var", integration.name)),
                ));
            }
            IntegrationType::Unifi(unifi) => {
                seen_vars.insert(unifi.api_key_var.clone());
                env_secrets.push(make_entry(
                    unifi.api_key_var.clone(),
                    Some(format!("integrations.{}.api_key_var", integration.name)),
                ));
            }
        }
    }

    // Scan LLM model key_var references (e.g. OPENAI_API_KEY, ANTHROPIC_API_KEY)
    for model in &config.models {
        if let Some(key_var) = model.key_var()
            && seen_vars.insert(key_var.to_string())
        {
            env_secrets.push(make_entry(
                key_var.to_string(),
                Some(format!("models.{}.key_var", model.name())),
            ));
        }
    }

    // Scan database *_var fields (credentials that may be supplied via env)
    for db in &config.databases {
        let name = &db.name;
        let mut pairs: Vec<(String, String)> = Vec::new();
        match &db.database_type {
            DatabaseType::Postgres(pg) => {
                if let Some(v) = &pg.password_var {
                    pairs.push((v.clone(), format!("databases.{name}.password_var")));
                }
                if let Some(v) = &pg.host_var {
                    pairs.push((v.clone(), format!("databases.{name}.host_var")));
                }
                if let Some(v) = &pg.user_var {
                    pairs.push((v.clone(), format!("databases.{name}.user_var")));
                }
                if let Some(v) = &pg.port_var {
                    pairs.push((v.clone(), format!("databases.{name}.port_var")));
                }
                if let Some(v) = &pg.database_var {
                    pairs.push((v.clone(), format!("databases.{name}.database_var")));
                }
            }
            DatabaseType::Redshift(rs) => {
                if let Some(v) = &rs.password_var {
                    pairs.push((v.clone(), format!("databases.{name}.password_var")));
                }
                if let Some(v) = &rs.host_var {
                    pairs.push((v.clone(), format!("databases.{name}.host_var")));
                }
                if let Some(v) = &rs.user_var {
                    pairs.push((v.clone(), format!("databases.{name}.user_var")));
                }
                if let Some(v) = &rs.port_var {
                    pairs.push((v.clone(), format!("databases.{name}.port_var")));
                }
                if let Some(v) = &rs.database_var {
                    pairs.push((v.clone(), format!("databases.{name}.database_var")));
                }
            }
            DatabaseType::Airhouse(ah) => {
                if let Some(v) = &ah.password_var {
                    pairs.push((v.clone(), format!("databases.{name}.password_var")));
                }
                if let Some(v) = &ah.host_var {
                    pairs.push((v.clone(), format!("databases.{name}.host_var")));
                }
                if let Some(v) = &ah.user_var {
                    pairs.push((v.clone(), format!("databases.{name}.user_var")));
                }
                if let Some(v) = &ah.port_var {
                    pairs.push((v.clone(), format!("databases.{name}.port_var")));
                }
                if let Some(v) = &ah.database_var {
                    pairs.push((v.clone(), format!("databases.{name}.database_var")));
                }
            }
            DatabaseType::Mysql(my) => {
                if let Some(v) = &my.password_var {
                    pairs.push((v.clone(), format!("databases.{name}.password_var")));
                }
                if let Some(v) = &my.host_var {
                    pairs.push((v.clone(), format!("databases.{name}.host_var")));
                }
                if let Some(v) = &my.user_var {
                    pairs.push((v.clone(), format!("databases.{name}.user_var")));
                }
                if let Some(v) = &my.port_var {
                    pairs.push((v.clone(), format!("databases.{name}.port_var")));
                }
                if let Some(v) = &my.database_var {
                    pairs.push((v.clone(), format!("databases.{name}.database_var")));
                }
            }
            DatabaseType::ClickHouse(ch) => {
                if let Some(v) = &ch.password_var {
                    pairs.push((v.clone(), format!("databases.{name}.password_var")));
                }
                if let Some(v) = &ch.host_var {
                    pairs.push((v.clone(), format!("databases.{name}.host_var")));
                }
                if let Some(v) = &ch.user_var {
                    pairs.push((v.clone(), format!("databases.{name}.user_var")));
                }
                if let Some(v) = &ch.database_var {
                    pairs.push((v.clone(), format!("databases.{name}.database_var")));
                }
            }
            DatabaseType::Snowflake(sf) => {
                if let SnowflakeAuthType::PasswordVar { password_var } = &sf.auth_type {
                    pairs.push((
                        password_var.clone(),
                        format!("databases.{name}.password_var"),
                    ));
                }
            }
            DatabaseType::Bigquery(bq) => {
                if let Some(v) = &bq.key_path_var {
                    pairs.push((v.clone(), format!("databases.{name}.key_path_var")));
                }
            }
            DatabaseType::MotherDuck(md) => {
                pairs.push((md.token_var.clone(), format!("databases.{name}.token_var")));
            }
            DatabaseType::DOMO(domo) => {
                pairs.push((
                    domo.developer_token_var.clone(),
                    format!("databases.{name}.developer_token_var"),
                ));
            }
            DatabaseType::DuckDB(_) => {}
            // Managed Airhouse has no `*_var` fields — credentials are
            // sourced from oxy's own database via the per-user provisioning
            // flow.
            // Neither carries credentials in config.yml — airhouse resolves
            // them per user, postgres_managed per org from `oltp_tenants`.
            DatabaseType::AirhouseManaged(_) | DatabaseType::PostgresManaged(_) => {}
        }
        for (var, config_field) in pairs {
            if seen_vars.insert(var.clone()) {
                env_secrets.push(make_entry(var, Some(config_field)));
            }
        }
    }

    // Add .env-only vars not already covered by config references.
    // Skip authentication / infrastructure vars — those belong to the server
    // config, not to the per-workspace secrets panel.
    for (key, _) in &dotenv_entries {
        if is_auth_env_var(key) {
            continue;
        }
        if !seen_vars.insert(key.clone()) {
            continue;
        }
        env_secrets.push(make_entry(key.clone(), None));
    }

    // Add built-in app env vars not already covered by config or .env
    for (var, label) in BUILT_IN_VARS {
        if seen_vars.insert(var.to_string()) {
            env_secrets.push(make_entry(var.to_string(), Some(label.to_string())));
        }
    }

    Ok((StatusCode::OK, axum::Json(env_secrets)).into_response())
}

pub async fn delete_secret(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(_user): AuthenticatedUserExtractor,
    Path((workspace_id, id)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let secret_id = match Uuid::parse_str(&id) {
        Ok(uuid) => uuid,
        Err(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "Invalid secret ID format" })),
            )
                .into_response());
        }
    };

    let db = establish_connection().await.map_err(|e| {
        tracing::error!("Failed to establish database connection: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let secret_manager = SecretManagerService::new(workspace_id);

    let Some(secret) = secret_manager.get_secret_by_id(secret_id).await else {
        return Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "Secret not found" })),
        )
            .into_response());
    };

    match secret_manager.delete_secret(&db, &secret.name).await {
        Ok(()) => Ok((StatusCode::NO_CONTENT, axum::Json(json!({}))).into_response()),
        Err(OxyError::SecretManager(msg)) => {
            Ok((StatusCode::BAD_REQUEST, axum::Json(json!({ "error": msg }))).into_response())
        }
        Err(e) => {
            tracing::error!("Failed to delete secret: {}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({ "error": "Failed to delete secret" })),
            )
                .into_response())
        }
    }
}

/// Reveal the plaintext value of a DB secret by ID (WorkspaceAdmin only).
pub async fn reveal_secret(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(_user): AuthenticatedUserExtractor,
    Path((workspace_id, id)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let secret_id = match Uuid::parse_str(&id) {
        Ok(uuid) => uuid,
        Err(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": "Invalid secret ID format" })),
            )
                .into_response());
        }
    };

    let secret_manager = SecretManagerService::new(workspace_id);

    // 404 and 500 are different answers and the caller acts on them differently:
    // a missing secret gets re-created, an unreadable one gets investigated. The
    // old code returned 404 for both, under a message that admitted the
    // ambiguity ("Secret not found or decryption failed") while the status code
    // asserted one half of it.
    match secret_manager.get_secret_value_by_id(secret_id).await {
        Ok(Some(value)) => {
            Ok((StatusCode::OK, axum::Json(json!({ "value": value }))).into_response())
        }
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "Secret not found" })),
        )
            .into_response()),
        Err(e) => {
            tracing::error!(secret_id = %secret_id, "reveal_secret failed: {e}");
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({
                    "error": "Secret exists but could not be read by this instance",
                    "detail": e.to_string(),
                })),
            )
                .into_response())
        }
    }
}
