//! Creating a workspace: its working copy on node-local disk, and its row.
//!
//! One implementation for every door that makes a workspace. Onboarding's
//! "start from scratch" (`POST /orgs/{org_id}/onboarding/new`) and both
//! org-creation doors (`POST /admin/orgs`, `POST /partners/{id}/orgs`) call
//! [`create_blank_workspace`]; the demo and GitHub imports reuse the pieces.
//! Every new org gets a Ready `Default` workspace, because Home only exists
//! under a workspace route and a customer's first sign-in should land there,
//! not in a workspace wizard.
//!
//! Lives in `oxy-app` rather than `oxy-project`: registering a workspace seeds
//! its health schedule through `agentic_pipeline`, and a platform crate may
//! never import an agentic one. Both sibling surface crates already depend on
//! `oxy-app`, and `oxy-app` depends on neither.
//!
//! Everything here writes node-local disk, so every route that reaches it is
//! IdeOnly — and [`create_blank_workspace`] refuses on a serve replica anyway,
//! so a misclassified route fails loudly instead of scaffolding a working copy
//! onto a pod that will never serve it.

use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use entity::prelude::Workspaces;
use entity::workspaces::{self, WorkspaceStatus};
use oxy::adapters::workspace::workspace_root_path;
use oxy::database::client::establish_connection;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, Set,
};
use uuid::Uuid;

/// Display name of the workspace every new org is created with.
pub const DEFAULT_WORKSPACE_NAME: &str = "Default";

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// Another workspace in the same org already carries this display name.
    #[error("A workspace named '{0}' already exists. Please choose a different name.")]
    NameTaken(String),
    /// This process owns no working copy — a stateless serve replica.
    #[error("{0}")]
    NoWorkingCopy(String),
    #[error("{0}")]
    Filesystem(String),
    #[error("{0}")]
    Database(String),
}

impl ProvisionError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::NameTaken(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<ProvisionError> for (StatusCode, String) {
    fn from(e: ProvisionError) -> Self {
        (e.status_code(), e.to_string())
    }
}

fn query_failed(e: DbErr) -> ProvisionError {
    ProvisionError::Database(format!("Failed to query workspaces: {e}"))
}

/// How a new workspace is named.
pub enum WorkspaceName<'a> {
    /// Exactly this. A clash within the org is [`ProvisionError::NameTaken`].
    Exact(&'a str),
    /// This, or the first free "`base` 2", "`base` 3", … within the org.
    UniqueFrom(&'a str),
}

/// A blank workspace to create for `org_id`, attributed to `created_by`.
pub struct BlankWorkspace<'a> {
    pub org_id: Uuid,
    pub created_by: Uuid,
    pub name: WorkspaceName<'a>,
}

/// A workspace whose working copy is on disk and whose row was written on the
/// caller's connection — possibly an open transaction that has not committed.
///
/// Settle it exactly once: [`finish`](Self::finish) after the row committed on
/// a plain connection, [`commit`](Self::commit) to commit the transaction it was
/// written in, or [`discard`](Self::discard) when the row never will. Dropping
/// it unsettled leaves an orphaned directory with no row — harmless, and safer
/// than a drop guard, which a cancelled request could fire after the commit
/// landed and so delete the working copy of a live workspace.
#[must_use = "settle a staged workspace with `finish`, `commit` or `discard`"]
pub struct StagedWorkspace {
    pub id: Uuid,
    dir: PathBuf,
}

impl StagedWorkspace {
    /// The row has committed: seed its health schedule and hand back its id.
    pub async fn finish(self) -> Uuid {
        seed_health_schedule(self.id).await;
        self.id
    }

    /// Commit `txn`, which holds this workspace's row, then [`finish`](Self::finish).
    /// A failed commit removes the working copy, so neither half survives.
    pub async fn commit(self, txn: DatabaseTransaction) -> Result<Uuid, DbErr> {
        match txn.commit().await {
            Ok(()) => Ok(self.finish().await),
            Err(e) => {
                self.discard();
                Err(e)
            }
        }
    }

    /// The row will never commit: remove the working copy.
    pub fn discard(self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            tracing::warn!(dir = ?self.dir, error = %e, "failed to remove discarded workspace directory");
        }
    }
}

/// Create a Ready blank workspace: a minimal `config.yml` in a fresh working
/// copy, and its row written on `conn`. Pass an open transaction to make the
/// row part of a larger write — the org-creation doors do, so an org is never
/// committed without its workspace.
pub async fn create_blank_workspace<C: ConnectionTrait>(
    conn: &C,
    spec: BlankWorkspace<'_>,
) -> Result<StagedWorkspace, ProvisionError> {
    crate::server::role_manifest::ensure_fs_writable("create a workspace working copy")
        .map_err(|e| ProvisionError::NoWorkingCopy(e.to_string()))?;

    let id = Uuid::new_v4();
    let staged = StagedWorkspace {
        id,
        dir: resolve_project_dir(id)?,
    };
    match scaffold_and_register(conn, &staged, spec).await {
        Ok(()) => Ok(staged),
        Err(e) => {
            staged.discard();
            Err(e)
        }
    }
}

/// The workspace every new org is created with — [`create_blank_workspace`]
/// named [`DEFAULT_WORKSPACE_NAME`].
pub async fn create_default_workspace<C: ConnectionTrait>(
    conn: &C,
    org_id: Uuid,
    created_by: Uuid,
) -> Result<StagedWorkspace, ProvisionError> {
    let spec = BlankWorkspace {
        org_id,
        created_by,
        name: WorkspaceName::UniqueFrom(DEFAULT_WORKSPACE_NAME),
    };
    create_blank_workspace(conn, spec).await
}

async fn scaffold_and_register<C: ConnectionTrait>(
    conn: &C,
    staged: &StagedWorkspace,
    spec: BlankWorkspace<'_>,
) -> Result<(), ProvisionError> {
    if !staged.dir.join("config.yml").exists() {
        oxy_project::write_minimal_config_yml(&staged.dir)
            .await
            .map_err(|e| ProvisionError::Filesystem(format!("Failed to write config.yml: {e}")))?;
    }
    let name = match spec.name {
        WorkspaceName::Exact(name) => name.to_string(),
        WorkspaceName::UniqueFrom(base) => {
            unique_display_name(conn, base, Some(spec.org_id)).await?
        }
    };
    let row = NewWorkspaceRow {
        name: &name,
        created_by: Some(spec.created_by),
        org_id: Some(spec.org_id),
        status: WorkspaceStatus::Ready,
        git_namespace_id: None,
        git_remote_url: None,
    };
    // The directory is named after a fresh UUID, so this always creates.
    register_workspace(conn, &staged.dir, staged.id, row).await?;
    Ok(())
}

/// `<state_dir>/workspaces/<workspace_id>`, created if needed. The workspace
/// UUID as the directory name guarantees uniqueness with no collision logic.
pub fn resolve_project_dir(workspace_id: Uuid) -> Result<PathBuf, ProvisionError> {
    let dir = workspace_root_path(workspace_id);
    std::fs::create_dir_all(&dir).map_err(|e| {
        ProvisionError::Filesystem(format!(
            "Failed to create workspace directory '{dir:?}': {e}"
        ))
    })?;
    Ok(dir)
}

/// A workspace display name unique within `org_id`: `base`, or the first free
/// "`base` 2" … "`base` 99".
///
/// Builds the whole candidate list and asks once with `WHERE name IN (…)`. `IN`
/// rather than `LIKE`, because `base` can be caller-controlled (a GitHub repo
/// name containing `_` or `%`). Names in other orgs don't count.
pub async fn unique_display_name<C: ConnectionTrait>(
    conn: &C,
    base: &str,
    org_id: Option<Uuid>,
) -> Result<String, ProvisionError> {
    let candidates: Vec<String> = std::iter::once(base.to_string())
        .chain((2u32..=99).map(|i| format!("{base} {i}")))
        .collect();

    let query = Workspaces::find().filter(workspaces::Column::Name.is_in(candidates.clone()));
    let taken: std::collections::HashSet<String> = in_org(query, org_id)
        .all(conn)
        .await
        .map_err(query_failed)?
        .into_iter()
        .map(|w| w.name)
        .collect();

    candidates
        .into_iter()
        .find(|candidate| !taken.contains(candidate))
        .ok_or_else(|| {
            ProvisionError::Database(format!("Could not find a unique name for '{base}'"))
        })
}

fn in_org(query: sea_orm::Select<Workspaces>, org_id: Option<Uuid>) -> sea_orm::Select<Workspaces> {
    match org_id {
        Some(id) => query.filter(workspaces::Column::OrgId.eq(id)),
        None => query.filter(workspaces::Column::OrgId.is_null()),
    }
}

/// The columns a caller chooses for a new workspace row.
pub struct NewWorkspaceRow<'a> {
    pub name: &'a str,
    pub created_by: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub status: WorkspaceStatus,
    pub git_namespace_id: Option<Uuid>,
    pub git_remote_url: Option<String>,
}

/// What [`register_workspace`] found or wrote.
pub struct Registered {
    pub id: Uuid,
    /// `false` when the directory was already registered and nothing was
    /// written — so there is no new row to seed a health schedule for, and
    /// re-seeding one would reset a schedule its compile already enabled.
    pub created: bool,
}

/// Insert the workspace row on `conn`. Idempotent on path: an already
/// registered directory returns its existing id. Does NOT activate the
/// workspace, and does not seed its health schedule — that needs a committed
/// row, so it is [`StagedWorkspace::finish`]'s job (or [`seed_health_schedule`]).
pub async fn register_workspace<C: ConnectionTrait>(
    conn: &C,
    project_dir: &Path,
    workspace_id: Uuid,
    row: NewWorkspaceRow<'_>,
) -> Result<Registered, ProvisionError> {
    let path = project_dir.to_string_lossy().to_string();
    let existing = Workspaces::find()
        .filter(workspaces::Column::Path.eq(path.clone()))
        .one(conn)
        .await
        .map_err(query_failed)?;
    if let Some(existing) = existing {
        return Ok(Registered {
            id: existing.id,
            created: false,
        });
    }

    // Display names are unique per org; other orgs have their own namespace.
    let clash = in_org(
        Workspaces::find().filter(workspaces::Column::Name.eq(row.name)),
        row.org_id,
    )
    .one(conn)
    .await
    .map_err(query_failed)?;
    if clash.is_some() {
        return Err(ProvisionError::NameTaken(row.name.to_string()));
    }

    let now = chrono::Utc::now();
    workspaces::ActiveModel {
        id: Set(workspace_id),
        name: Set(row.name.to_string()),
        git_namespace_id: Set(row.git_namespace_id),
        git_remote_url: Set(row.git_remote_url),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
        path: Set(Some(path.clone())),
        last_opened_at: Set(None),
        created_by: Set(row.created_by),
        org_id: Set(row.org_id),
        status: Set(row.status),
        error: Set(None),
        monthly_vlm_budget_micros: Set(None),
        current_revision_id: Set(None),
    }
    .insert(conn)
    .await
    .map_err(|e| {
        ProvisionError::Database(format!(
            "Failed to register workspace '{}' in DB: {e}",
            row.name
        ))
    })?;
    tracing::info!("Registered workspace '{}' at '{}'", row.name, path);
    Ok(Registered {
        id: workspace_id,
        created: true,
    })
}

/// Seed the workspace's health schedule row, **disabled**. Best-effort: never
/// fails the caller.
///
/// Health checks are opt-in and a fresh workspace has compiled no `config.yml`,
/// so nothing says it wants them; the compile worker enables the row from
/// `health_check` on the first promoted compile. Seeding it anyway keeps that
/// reconcile a plain update. Both values come from the resolvers the compile
/// worker uses, so there is one definition of "unconfigured" — and the cadence
/// matches, so that compile leaves `next_run_at` alone.
pub async fn seed_health_schedule(workspace_id: Uuid) {
    let db = match establish_connection().await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(target: "health_eval", error = %e, %workspace_id, "failed to seed health schedule for new workspace");
            return;
        }
    };
    if let Err(e) = agentic_pipeline::scheduler::reconcile_health_schedule(
        &db,
        workspace_id,
        oxy::config::health_check::resolve_interval(None),
        oxy::config::health_check::resolve_enabled(None),
    )
    .await
    {
        tracing::warn!(
            target: "health_eval",
            error = %e,
            %workspace_id,
            "failed to seed health schedule for new workspace"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Org creation maps every provisioning failure to a 500, but onboarding
    /// surfaces this mapping directly — a clash is the one thing a user fixes.
    #[test]
    fn only_a_name_clash_is_the_callers_to_fix() {
        assert_eq!(
            ProvisionError::NameTaken("x".into()).status_code(),
            StatusCode::CONFLICT
        );
        for e in [
            ProvisionError::NoWorkingCopy("x".into()),
            ProvisionError::Filesystem("x".into()),
            ProvisionError::Database("x".into()),
        ] {
            assert_eq!(e.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
}
