use axum::http::StatusCode;
use oxy::adapters::secrets::SecretsManager;
use oxy::service::secret_manager::SecretManagerService;
use oxy_app::server::service::workspace_provisioning as provisioning;
use uuid::Uuid;

use super::dto::*;

/// Validate and normalise a user-supplied subdirectory path.
///
/// Returns `Err` if the path is absolute or contains any `..` / `.` components
/// (path-traversal protection). Returns `Ok(None)` for empty/whitespace input.
pub(super) fn parse_subdir(raw: &str) -> Result<Option<std::path::PathBuf>, (StatusCode, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = std::path::Path::new(trimmed);
    if path.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Subdirectory must be a relative path".to_string(),
        ));
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Invalid subdirectory path: '{trimmed}'"),
                ));
            }
        }
    }
    Ok(Some(path.to_path_buf()))
}

// The three helpers below are thin adapters over `oxy_app`'s
// `workspace_provisioning` — the one implementation of creating a workspace,
// shared with the org-creation doors — kept so `setup_demo` / `setup_github`
// read the same as before. See that module for the behavior.

/// `<state_dir>/workspaces/<workspace_id>`, created if needed.
pub(super) fn resolve_project_dir(
    workspace_id: Uuid,
) -> Result<std::path::PathBuf, (StatusCode, String)> {
    provisioning::resolve_project_dir(workspace_id).map_err(Into::into)
}

/// A workspace display name unique within `org_id` (`base`, `base 2`, …).
pub(super) async fn unique_display_name(
    base: &str,
    org_id: Option<Uuid>,
) -> Result<String, (StatusCode, String)> {
    let db = connect().await?;
    provisioning::unique_display_name(&db, base, org_id)
        .await
        .map_err(Into::into)
}

/// Register the workspace in the DB and seed its health schedule. Returns the
/// workspace's UUID. Does NOT activate the workspace.
#[allow(clippy::too_many_arguments)]
pub(super) async fn register_project(
    project_dir: &std::path::Path,
    name: &str,
    workspace_id: Uuid,
    created_by: Option<Uuid>,
    org_id: Option<Uuid>,
    status: entity::workspaces::WorkspaceStatus,
    git_namespace_id: Option<Uuid>,
    git_remote_url: Option<String>,
) -> Result<Uuid, (StatusCode, String)> {
    let db = connect().await?;
    let row = provisioning::NewWorkspaceRow {
        name,
        created_by,
        org_id,
        status,
        git_namespace_id,
        git_remote_url,
    };
    let registered = provisioning::register_workspace(&db, project_dir, workspace_id, row).await?;
    if registered.created {
        provisioning::seed_health_schedule(registered.id).await;
    }
    Ok(registered.id)
}

async fn connect() -> Result<sea_orm::DatabaseConnection, (StatusCode, String)> {
    oxy::database::client::establish_connection()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database connection failed: {e}"),
            )
        })
}

/// Update the clone status and error message for a workspace row.
pub(super) async fn update_workspace_status(
    workspace_id: Uuid,
    status: entity::workspaces::WorkspaceStatus,
    error: Option<String>,
) -> Result<(), String> {
    use entity::workspaces;
    use oxy::database::client::establish_connection;
    use sea_orm::{ActiveModelTrait, EntityTrait, Set};

    let db = establish_connection()
        .await
        .map_err(|e| format!("Database connection failed: {e}"))?;

    let existing = workspaces::Entity::find_by_id(workspace_id)
        .one(&db)
        .await
        .map_err(|e| format!("Failed to load workspace {workspace_id}: {e}"))?
        .ok_or_else(|| format!("Workspace {workspace_id} no longer exists"))?;

    let mut active: workspaces::ActiveModel = existing.into();
    active.status = Set(status);
    active.error = Set(error);
    active.updated_at = Set(chrono::Utc::now().into());
    active
        .update(&db)
        .await
        .map_err(|e| format!("Failed to update workspace {workspace_id}: {e}"))?;
    Ok(())
}

/// True if `var_name` has a secret set for this workspace, resolved the same way
/// the runtime resolves it: DB-only in cloud (so an env var set on the server
/// can't mask a genuinely-missing workspace secret) and DB + env fallback in
/// local. Shared by `onboarding_readiness` and `github_setup` so the two
/// presence checks can't drift out of sync again.
pub(super) async fn secret_is_set(
    secrets_manager: &SecretsManager,
    db_secret_manager: &SecretManagerService,
    is_local: bool,
    var_name: &str,
) -> bool {
    if is_local {
        secrets_manager
            .resolve_secret(var_name)
            .await
            .ok()
            .flatten()
            .is_some()
    } else {
        db_secret_manager.get_secret(var_name).await.is_some()
    }
}

pub(super) fn vendor_label_for_model(model: &oxy::config::model::Model) -> String {
    use oxy::config::model::Model as M;
    match model {
        M::OpenAI { .. } => "OpenAI".to_string(),
        M::Anthropic { .. } => "Anthropic".to_string(),
        M::Google { .. } => "Google".to_string(),
        M::Ollama { .. } => "Ollama".to_string(),
        // Label the credential, not the protocol — this string completes
        // "Enter your ___ API key" in the onboarding prompt, and the user is
        // holding a key for whichever gateway they pointed `api_url` at.
        M::OpenAICompat { .. } => "OpenAI-compatible".to_string(),
    }
}

/// Enumerate the `*_var` credential fields declared on a warehouse, tagging
/// each with the config field it maps to and whether it's strictly required
/// (no inline plaintext fallback). The frontend uses this to build a
/// `credential_form` with one password-style input per entry.
pub(super) fn collect_warehouse_vars(
    database: &oxy::config::model::Database,
) -> Vec<GithubSetupMissingVar> {
    use oxy::config::model::{DatabaseType, SnowflakeAuthType};

    let mut out: Vec<GithubSetupMissingVar> = Vec::new();
    let push_opt =
        |out: &mut Vec<GithubSetupMissingVar>, field: &str, var: &Option<String>, inline: bool| {
            if let Some(v) = var {
                out.push(GithubSetupMissingVar {
                    field: field.to_string(),
                    var_name: v.clone(),
                    required: !inline,
                });
            }
        };
    let push_req = |out: &mut Vec<GithubSetupMissingVar>, field: &str, var: &str| {
        out.push(GithubSetupMissingVar {
            field: field.to_string(),
            var_name: var.to_string(),
            required: true,
        });
    };

    match &database.database_type {
        DatabaseType::Postgres(p) => {
            push_opt(&mut out, "host", &p.host_var, p.host.is_some());
            push_opt(&mut out, "port", &p.port_var, p.port.is_some());
            push_opt(&mut out, "user", &p.user_var, p.user.is_some());
            push_opt(&mut out, "password", &p.password_var, p.password.is_some());
            push_opt(&mut out, "database", &p.database_var, p.database.is_some());
        }
        DatabaseType::Airhouse(a) => {
            push_opt(&mut out, "host", &a.host_var, a.host.is_some());
            push_opt(&mut out, "port", &a.port_var, a.port.is_some());
            push_opt(&mut out, "user", &a.user_var, a.user.is_some());
            push_opt(&mut out, "password", &a.password_var, a.password.is_some());
            push_opt(&mut out, "database", &a.database_var, a.database.is_some());
        }
        DatabaseType::Redshift(r) => {
            push_opt(&mut out, "host", &r.host_var, r.host.is_some());
            push_opt(&mut out, "port", &r.port_var, r.port.is_some());
            push_opt(&mut out, "user", &r.user_var, r.user.is_some());
            push_opt(&mut out, "password", &r.password_var, r.password.is_some());
            push_opt(&mut out, "database", &r.database_var, r.database.is_some());
        }
        DatabaseType::Mysql(m) => {
            push_opt(&mut out, "host", &m.host_var, m.host.is_some());
            push_opt(&mut out, "port", &m.port_var, m.port.is_some());
            push_opt(&mut out, "user", &m.user_var, m.user.is_some());
            push_opt(&mut out, "password", &m.password_var, m.password.is_some());
            push_opt(&mut out, "database", &m.database_var, m.database.is_some());
        }
        DatabaseType::ClickHouse(c) => {
            push_opt(&mut out, "host", &c.host_var, c.host.is_some());
            push_opt(&mut out, "user", &c.user_var, c.user.is_some());
            push_opt(&mut out, "password", &c.password_var, c.password.is_some());
            push_opt(&mut out, "database", &c.database_var, c.database.is_some());
        }
        DatabaseType::Snowflake(s) => {
            if let SnowflakeAuthType::PasswordVar { password_var } = &s.auth_type {
                push_req(&mut out, "password", password_var);
            }
        }
        DatabaseType::Bigquery(b) => {
            push_opt(&mut out, "key_path", &b.key_path_var, b.key_path.is_some());
        }
        DatabaseType::MotherDuck(md) => {
            push_req(&mut out, "token", &md.token_var);
        }
        DatabaseType::DOMO(d) => {
            push_req(&mut out, "developer_token", &d.developer_token_var);
        }
        DatabaseType::DuckDB(_) => {
            // No credentials.
        }
        DatabaseType::AirhouseManaged(_) => {
            // No credentials in config.yml — they live in oxy's per-user
            // `airhouse_users` row.
        }
        DatabaseType::PostgresManaged(_) => {
            // No credentials in config.yml — they live on the org's
            // `oltp_tenants` row, sealed with the platform master key.
        }
    }
    out
}

// ── Warehouse file upload ──────────────────────────────────────────────────

/// Default subdirectory (relative to the workspace root) where uploaded DuckDB
/// data files are written when the client does not specify one.
pub(super) const DEFAULT_UPLOAD_SUBDIR: &str = ".db";

/// Per-file byte cap for onboarding data uploads.
pub(super) const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;

/// Per-request aggregate byte cap (across all files).
const MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Body-limit constant for the router. Sized slightly above `MAX_TOTAL_BYTES`
/// to accommodate multipart framing overhead (boundaries, headers).
pub const MAX_UPLOAD_BODY_BYTES: usize = (MAX_TOTAL_BYTES as usize) + 1024 * 1024;

/// Hard cap on the number of files accepted in a single upload request.
pub(super) const MAX_FILES_PER_REQUEST: usize = 25;

/// Supported file extensions, lowercased. Must mirror the DuckDB connector's
/// `collect_supported_files` so the connection test cannot find a file type
/// that the upload endpoint did not accept.
const SUPPORTED_EXTENSIONS: &[&str] = &["csv", "parquet"];

/// Return the basename of `raw` after rejecting anything that could escape the
/// target directory (absolute paths, `..`, embedded separators, hidden files).
pub(super) fn sanitise_upload_filename(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty_filename");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err("path_separator_in_filename");
    }
    if trimmed == "." || trimmed == ".." {
        return Err("invalid_filename");
    }
    if trimmed.starts_with('.') {
        return Err("hidden_filename");
    }
    if trimmed.contains('\0') {
        return Err("null_byte_in_filename");
    }
    Ok(trimmed.to_string())
}

pub(super) fn has_supported_extension(name: &str) -> bool {
    match std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
    {
        Some(ext) => SUPPORTED_EXTENSIONS
            .iter()
            .any(|supported| supported.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

pub(super) async fn drain_field(
    mut field: axum::extract::multipart::Field<'_>,
) -> Result<(), (StatusCode, String)> {
    while let Some(chunk) = field.chunk().await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to read multipart chunk: {e}"),
        )
    })? {
        let _ = chunk;
    }
    Ok(())
}

/// Stream one `file` field to `tmp_path`, enforcing per-file and request-wide
/// byte caps. On any error (including size overflow) the partial file is
/// removed so we don't leak a half-written upload.
pub(super) async fn stream_field_to_file(
    mut field: axum::extract::multipart::Field<'_>,
    tmp_path: &std::path::Path,
    max_file_bytes: u64,
    total_bytes: &mut u64,
) -> Result<u64, (StatusCode, String)> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(tmp_path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create upload temp file: {e}"),
        )
    })?;

    let mut written: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                let chunk_len = chunk.len() as u64;
                if written.saturating_add(chunk_len) > max_file_bytes {
                    let _ = tokio::fs::remove_file(tmp_path).await;
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "File exceeds per-file size limit of {} bytes",
                            max_file_bytes
                        ),
                    ));
                }
                if total_bytes.saturating_add(chunk_len) > MAX_TOTAL_BYTES {
                    let _ = tokio::fs::remove_file(tmp_path).await;
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "Upload exceeds aggregate size limit of {} bytes",
                            MAX_TOTAL_BYTES
                        ),
                    ));
                }
                file.write_all(&chunk).await.map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to write upload chunk: {e}"),
                    )
                })?;
                written += chunk_len;
                *total_bytes += chunk_len;
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(tmp_path).await;
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Failed to read upload stream: {e}"),
                ));
            }
        }
    }

    file.flush().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to flush upload: {e}"),
        )
    })?;
    Ok(written)
}
