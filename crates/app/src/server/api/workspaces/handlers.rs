use axum::extract::{Extension, Json, Path, Query, State};
use axum::response::Json as ResponseJson;
use axum::response::{IntoResponse, Response};
use reqwest::StatusCode;
use tracing::{error, info};
use uuid::Uuid;

use crate::server::api::middlewares::org_context::OrgContextExtractor;
use crate::server::api::middlewares::role_guards::{
    OrgAdmin, WorkspaceAdmin, WorkspaceEditor, ensure_org_admin_or_workspace_creator,
};
use crate::server::api::middlewares::workspace_context::{
    BranchQuery, EffectiveWorkspaceRole, WorkspaceManagerWorkingCopy, WorkspacePath,
    WorkspaceRootWorkingCopy,
};
use crate::server::authz;
use crate::server::router::AppState;
use oxy::adapters::workspace::builder::WorkspaceBuilder;
use oxy::api_types::{
    BranchOrigin, BranchType, ProjectBranch, RecentCommitsResponse, RevisionInfoResponse,
};
use oxy::config::ConfigBuilder;
use oxy::github::default_git_client;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_git::{GitClient, ResetOutcome};

use super::dto::*;
use super::ops::*;
use oxy::config::WorkingCopy;

/// Detect the workspace's git mode from disk + environment. The
/// `GIT_REPOSITORY_URL` env var is treated as "remote configured" — cloud
/// deployments inject the remote that way.
pub async fn detect_git_mode(workspace_root: &std::path::Path) -> GitMode {
    let git = default_git_client();
    if !git.is_git_repo(workspace_root) {
        return GitMode::None;
    }
    let has_remote =
        std::env::var("GIT_REPOSITORY_URL").is_ok() || git.has_remote(workspace_root).await;
    if has_remote {
        GitMode::Connected
    } else {
        GitMode::Local
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────

pub async fn pull_changes(
    _: WorkspaceEditor,
    State(app_state): State<AppState>,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Extension(ws): Extension<entity::workspaces::Model>,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<BranchQuery>,
) -> Result<ResponseJson<PullChangesResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    let branch = resolve_branch(query.branch, worktree).await;
    let git = default_git_client();

    // Probe on-disk state rather than stderr — `git pull --rebase` exits
    // non-zero on conflict but the message is locale- and version-sensitive.
    match git_pull(worktree, &branch, &ws, user.id).await {
        Ok(outcome) => {
            if git.is_in_conflict(worktree).await {
                Ok(ResponseJson(PullChangesResponse {
                    success: false,
                    message: "Pull paused with conflicts — resolve them in the changes panel."
                        .to_string(),
                    state: PullState::Conflict,
                }))
            } else {
                // A pull rewrote the working copy in place. Without this the
                // world-model graph + semantic panels serve the pre-pull
                // revision until the TTL lapses (60s, both caches).
                // Same invalidation the branch switch does. The engine cache
                // is keyed on the layer SOURCE (working copy vs a compiled
                // revision) and the dialect map, so it is cleared for the whole
                // workspace: a pull changes the working copy and can change
                // which revision is promoted, and the writer cannot know which
                // of those entries that invalidates.
                app_state.semantic_layer_cache.invalidate_workspace(ws.id);
                app_state.semantic_engine_cache.invalidate_workspace(ws.id);
                // The pull rewrote the working copy, but reads are served from
                // the promoted revision — so without this the runtime keeps
                // serving the pre-pull configuration indefinitely, which is
                // precisely what a user pulling is trying to stop.
                if outcome.before_sha != outcome.after_sha {
                    crate::server::compile_trigger::compile_after_content_change(
                        ws.id,
                        worktree,
                        &branch,
                        "after pull",
                    )
                    .await;
                }
                // `Synced` only when the local branch genuinely matches origin.
                // A pull that brought nothing in because local commits sit on
                // top is NOT synced — that is the stranded state, and calling
                // it "Synced" is what sent an operator looking for the problem
                // everywhere except the one commit causing it.
                let state = if outcome.unpushed > 0 {
                    PullState::AheadOfRemote
                } else {
                    PullState::Synced
                };
                Ok(ResponseJson(PullChangesResponse {
                    success: true,
                    message: outcome.summary(),
                    state,
                }))
            }
        }
        Err(e) => {
            if git.is_in_conflict(worktree).await {
                Ok(ResponseJson(PullChangesResponse {
                    success: false,
                    message: "Pull paused with conflicts — resolve them in the changes panel."
                        .to_string(),
                    state: PullState::Conflict,
                }))
            } else {
                error!("Failed to pull changes: {}", e);
                Ok(ResponseJson(PullChangesResponse {
                    success: false,
                    message: format!("{e}"),
                    state: PullState::Error,
                }))
            }
        }
    }
}

pub async fn fetch_changes(
    _: WorkspaceEditor,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Extension(ws): Extension<entity::workspaces::Model>,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<BranchQuery>,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    let branch = resolve_branch(query.branch, worktree).await;

    match git_fetch(worktree, &branch, &ws, user.id).await {
        Ok(message) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message,
        })),
        Err(e) => {
            error!("Failed to fetch from remote: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn resolve_conflict_with_content(
    _: WorkspaceEditor,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<ResolveConflictWithContentQuery>,
    Json(body): Json<ResolveConflictWithContentBody>,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match default_git_client()
        .write_and_stage_file(worktree, &query.file, &body.content)
        .await
    {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: format!("Resolved {}", query.file),
        })),
        Err(e) => {
            error!("Failed to resolve conflict with content: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn resolve_conflict_file(
    _: WorkspaceEditor,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<ResolveConflictQuery>,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let use_mine = query.side == "mine";
    let worktree = wm.config_manager.workspace_path();
    match default_git_client()
        .resolve_conflict_file(worktree, &query.file, use_mine)
        .await
    {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: format!("Resolved {} using {}", query.file, query.side),
        })),
        Err(e) => {
            error!("Failed to resolve conflict file: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn unresolve_conflict_file(
    _: WorkspaceEditor,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<UnresolveConflictQuery>,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match default_git_client()
        .unresolve_conflict_file(worktree, &query.file)
        .await
    {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: format!("Conflict markers restored for {}", query.file),
        })),
        Err(e) => {
            error!("Failed to unresolve conflict file: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn force_push_branch(
    _: WorkspaceAdmin,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Extension(ws): Extension<entity::workspaces::Model>,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match git_force_push(worktree, &ws, user.id).await {
        Ok(message) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message,
        })),
        Err(e) => {
            error!("Failed to force push: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn get_recent_commits(
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<RecentCommitsQuery>,
) -> Result<ResponseJson<RecentCommitsResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    let limit = query.limit.unwrap_or(10).clamp(1, 100);
    let offset = query.offset.unwrap_or(0);
    Ok(ResponseJson(
        git_recent_commits(worktree, limit, offset).await,
    ))
}

pub async fn reset_to_commit(
    _: WorkspaceAdmin,
    Extension(ws): Extension<entity::workspaces::Model>,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<ResetToCommitQuery>,
) -> Result<ResponseJson<ResetToCommitResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match default_git_client()
        .reset_to_commit(worktree, &query.commit, query.force)
        .await
    {
        Ok(ResetOutcome::Done) => {
            // A restore rewrites the working copy and commits the result, so
            // the promoted revision no longer reflects the workspace. Ship it,
            // or the restore is invisible to everything that reads compiled
            // state. Note this promotes a local-only commit: that is what
            // restore *means*, and it already happened on the manual path —
            // this only removes the requirement to remember to click Compile.
            let branch = resolve_branch(query.branch.clone(), worktree).await;
            crate::server::compile_trigger::compile_after_content_change(
                ws.id,
                worktree,
                &branch,
                "after restore",
            )
            .await;
            Ok(ResponseJson(ResetToCommitResponse {
                success: true,
                message: format!("Restored to {}", query.commit),
                dirty: None,
                discarded_commits: None,
            }))
        }
        Ok(ResetOutcome::Dirty(dirty)) => Ok(ResponseJson(ResetToCommitResponse {
            success: false,
            message: format!("{} uncommitted change(s) would be discarded", dirty.len()),
            dirty: Some(dirty),
            discarded_commits: None,
        })),
        Ok(ResetOutcome::WouldDiscardCommits(commits)) => {
            let short = &query.commit[..query.commit.len().min(7)];
            Ok(ResponseJson(ResetToCommitResponse {
                success: false,
                message: format!(
                    "Restoring to {short} would discard {} commit(s) made after it.",
                    commits.len()
                ),
                dirty: None,
                discarded_commits: Some(commits.into_iter().map(commit_entry).collect()),
            }))
        }
        Err(e) => {
            error!("Failed to restore to commit: {}", e);
            Ok(ResponseJson(ResetToCommitResponse {
                success: false,
                message: format!("{e}"),
                dirty: None,
                discarded_commits: None,
            }))
        }
    }
}

pub async fn abort_rebase(
    _: WorkspaceEditor,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match default_git_client().abort_rebase(worktree).await {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: "Rebase aborted".to_string(),
        })),
        Err(e) => {
            error!("Failed to abort rebase: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn continue_rebase(
    _: WorkspaceEditor,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match default_git_client().continue_rebase(worktree).await {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: "Rebase continued successfully".to_string(),
        })),
        Err(e) => {
            error!("Failed to continue rebase: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn discard_all_changes(
    _: WorkspaceAdmin,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    match default_git_client().discard_all_changes(worktree).await {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: "Discarded all local changes".to_string(),
        })),
        Err(e) => {
            error!("Failed to discard local changes: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn push_changes(
    _: WorkspaceEditor,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Extension(ws): Extension<entity::workspaces::Model>,
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Json(request): Json<PushChangesRequest>,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    // Empty message = "push existing commits only". Never synthesize an
    // "Auto-commit" — a dirty working tree would get silently pushed.
    let commit_message = request.commit_message.unwrap_or_default();

    match git_push(worktree, &commit_message, &ws, user.id).await {
        Ok(message) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message,
        })),
        Err(e) => {
            error!("Failed to push changes: {}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn get_revision_info(
    WorkspaceManagerWorkingCopy(wm): WorkspaceManagerWorkingCopy,
    Query(query): Query<BranchQuery>,
) -> Result<ResponseJson<RevisionInfoResponse>, StatusCode> {
    let worktree = wm.config_manager.workspace_path();
    let branch = resolve_branch(query.branch, worktree).await;
    Ok(ResponseJson(git_revision_info(worktree, &branch).await))
}

/// `GET /{workspace_id}/meta` — the Postgres half of `/details`.
///
/// Additive: `/details` is untouched and still serves both halves. This exists
/// so the frontend can move one query at a time, and so the half that needs no
/// working copy stops being unavailable whenever the ide is.
///
/// It delegates rather than duplicating the local-mode short-circuit and the
/// storage-key computation, both of which have earned their comments. Splitting
/// the *surface* is what unblocks the frontend; splitting the *bodies* can wait
/// until the old route is retired.
pub async fn get_workspace_meta(
    state: State<AppState>,
    user: AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    project: Extension<entity::workspaces::Model>,
    path: Path<Uuid>,
) -> Result<ResponseJson<WorkspaceMetaResponse>, Response> {
    // Local mode keeps the delegation. Both of the fields that can only be
    // answered from disk — `requires_local_setup` and `workspace_error` — are
    // set nowhere else, and the short-circuit that computes them has earned
    // its comments.
    if state.mode.is_local() {
        let ResponseJson(d) =
            get_workspace(state, user, EffectiveWorkspaceRole(role), project, path).await?;
        return Ok(ResponseJson(WorkspaceMetaResponse {
            id: d.id,
            name: d.name,
            workspace_id: d.workspace_id,
            created_at: d.created_at,
            updated_at: d.updated_at,
            workspace_error: d.workspace_error,
            storage_key: d.storage_key,
            current_user_role: d.current_user_role,
            requires_local_setup: d.requires_local_setup,
        }));
    }

    // Cloud: not one field of this response comes off the filesystem.
    // `workspace_id`, `created_at` and `updated_at` are the same constants in
    // both arms of `build_workspace_details_response`; `workspace_error` is
    // only ever `Some` under `is_local`; `requires_local_setup` is `false`.
    //
    // Delegating anyway meant inheriting `get_workspace`'s work AND its
    // failures: four git subprocesses and an `Origin::Disk` ConfigManager per
    // request on a node that owns the files, and — worse — the 503 that
    // `build_workspace_details_response` raises when the workspace root is not
    // there yet on an fs-writable pod. `/meta` was returning "materializing"
    // for data no volume holds. FleetOk was safe only because a replica has no
    // files to trip over; now it is safe because there is nothing to ask.
    let Path(workspace_id) = path;
    let Extension(project) = project;
    let now = chrono::Utc::now().to_string();
    Ok(ResponseJson(WorkspaceMetaResponse {
        id: workspace_id,
        name: project.name,
        workspace_id: Uuid::nil(),
        created_at: now.clone(),
        updated_at: now,
        workspace_error: None,
        storage_key: compute_workspace_storage_key(workspace_id, None),
        current_user_role: role.as_str().to_string(),
        requires_local_setup: false,
    }))
}

/// `GET /{workspace_id}/git-state` — the `.git` half of `/details`.
///
/// Live state, so it cannot be compiled and the route stays IdeOnly. A 502 when
/// the ide is down is the honest answer, and the frontend already knows what to
/// do with an absent branch — it is what today's degraded `/details` produces.
pub async fn get_workspace_git_state(
    state: State<AppState>,
    user: AuthenticatedUserExtractor,
    role: EffectiveWorkspaceRole,
    project: Extension<entity::workspaces::Model>,
    path: Path<Uuid>,
) -> Result<ResponseJson<WorkspaceGitStateResponse>, Response> {
    let ResponseJson(d) = get_workspace(state, user, role, project, path).await?;
    Ok(ResponseJson(WorkspaceGitStateResponse {
        active_branch: d.active_branch,
        git_mode: d.git_mode,
        capabilities: d.capabilities,
        default_branch: d.default_branch,
        protected_branches: d.protected_branches,
    }))
}

/// Get detailed information about a specific workspace including its active branch.
///
/// This endpoint retrieves complete workspace details along with the currently active branch information.
/// Requires authentication and returns workspace metadata, provider info, and active branch details.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "Workspace details retrieved successfully", body = WorkspaceDetailsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Workspace not found"),
        (status = 500, description = "Internal server error"),
        (status = 503, description = "Workspace working copy is not on disk yet \
            (pod restart / rolling update). Transient — carries \
            `x-oxy-unavailable: workspace-materializing` and `Retry-After`; \
            retry rather than surfacing an error. Cloud only.")
    ),
    security(
        ("ApiKey" = [])
    ),
    tag = "Workspaces"
)]
pub async fn get_workspace(
    State(app_state): State<AppState>,
    AuthenticatedUserExtractor(_user): AuthenticatedUserExtractor,
    EffectiveWorkspaceRole(role): EffectiveWorkspaceRole,
    Extension(project): Extension<entity::workspaces::Model>,
    Path(workspace_id): Path<Uuid>,
) -> Result<ResponseJson<WorkspaceDetailsResponse>, Response> {
    info!("Getting workspace details for ID: {}", workspace_id);

    let role_str = role.as_str().to_string();

    // Local-mode short-circuit: when there is no resolvable config.yml,
    // skip the normal path (which 500s on path: None) and return the
    // "setup required" shape so the FE renders the bootstrap dialog.
    if app_state.mode.is_local() {
        let config_exists = match &project.path {
            Some(p) => tokio::fs::try_exists(std::path::Path::new(p).join("config.yml"))
                .await
                .unwrap_or(false),
            None => false,
        };
        if !config_exists {
            // `setup_demo` / `setup_empty` write into `startup_cwd`, so hashing
            // it here keeps the storage_key stable across the bootstrap → ready
            // transition (the workspace path resolves to the same dir post-setup).
            let storage_key =
                compute_workspace_storage_key(workspace_id, Some(app_state.startup_cwd.as_path()));
            return Ok(build_workspace_details_response_for_uninitialized_local(
                workspace_id,
                &project.name,
                role_str,
                storage_key,
            ));
        }
    }

    let workspace_root = workspace_root(&project)
        .await
        .map_err(IntoResponse::into_response)?;

    let local_path_for_key = app_state
        .mode
        .is_local()
        .then_some(workspace_root.as_path());
    let storage_key = compute_workspace_storage_key(workspace_id, local_path_for_key);

    // A missing working copy is "not ready yet" only when THIS instance owns the
    // workspace filesystem. A stateless serve replica has no PVC, so `exists()`
    // is false there in normal operation; answering 503 would turn a benign
    // degraded page into an unusable one.
    let missing_copy_is_transient =
        !app_state.mode.is_local() && crate::server::role_manifest::process_is_fs_writable();

    build_workspace_details_response(
        workspace_id,
        &project.name,
        &workspace_root,
        app_state.mode.is_local(),
        missing_copy_is_transient,
        role_str,
        storage_key,
    )
    .await
}

/// How long the FE should wait before retrying a `workspace-materializing` 503.
/// Matches `ide_proxy`'s ide-down `Retry-After` — same class of "come back in a
/// moment", and a shared number is one fewer thing to reconcile.
const MATERIALIZING_RETRY_AFTER_SECS: &str = "5";

/// The working copy is not on disk on an instance that OWNS one (ide / all).
/// That is a readiness state, not a failure: git is the source of truth, so the
/// checkout is by definition re-clonable and the only honest answer is "not
/// yet". Say so with `503` + `Retry-After` and let the caller back off.
///
/// Only reachable when `missing_copy_is_transient` — a serve replica with no
/// PVC hits the same `exists()` check in NORMAL operation while degrading for a
/// down ide, and must keep its flagged 200.
///
/// It previously returned `200` with a `workspace_error` string, which was
/// wrong in three compounding ways during a rolling update:
///   1. A `200` is invisible to every ide-down detector (they key on `502` +
///      `x-oxy-required-role: ide`), so nothing throttled it.
///   2. That `200` carries `x-oxy-served-by: ide`, so the FE's success
///      interceptor fired `reportIdeReachable()` — the response that should
///      raise the "unavailable" UI was retiring it instead.
///   3. The FE toasted the string AND navigated away, which remounted the
///      shell, refetched, and toasted again. That loop is the toast spam.
///
/// Deliberately NOT `502` and NOT `x-oxy-required-role: ide`: the ide is
/// REACHABLE and serving its other routes fine, so borrowing the ide-down
/// signal would conflate two states — and because `reportIdeReachable()` fires
/// on any successful ide response, a concurrent healthy request would flap the
/// banner off while this one was still raising it.
fn workspace_materializing_response(workspace_root: &std::path::Path) -> Response {
    tracing::info!(
        path = %workspace_root.display(),
        "workspace details: working copy not materialised yet — 503 (transient)"
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (
                crate::server::ide_proxy::HEADER_UNAVAILABLE,
                "workspace-materializing",
            ),
            (
                axum::http::header::RETRY_AFTER.as_str(),
                MATERIALIZING_RETRY_AFTER_SECS,
            ),
        ],
        ResponseJson(serde_json::json!({
            "error": "Workspace is still starting up",
            "code": "workspace_materializing",
        })),
    )
        .into_response()
}

pub async fn build_workspace_details_response(
    workspace_id: Uuid,
    name: &str,
    workspace_root: &std::path::Path,
    // Set to true in local mode so the response reports `GitMode::None`
    // even when a `.git` folder exists on disk. Opposite polarity from
    // the router's `include_git_features` — matches `ServeMode::is_local`.
    is_local: bool,
    // Whether a MISSING working copy should be reported as transient (503) or
    // as the flagged degrade (200). Deliberately a separate flag rather than
    // derived from `is_local`: three distinct situations produce a missing
    // directory and only one of them is transient.
    //   - ide/all in cloud, volume not populated yet → TRANSIENT. Ours to own.
    //   - serve replica in cloud → the ide is down and we are serving the
    //     documented graceful degrade. Not ours, not transient.
    //   - local mode → no upstream to restore from, so it is genuinely gone.
    missing_copy_is_transient: bool,
    current_user_role: String,
    storage_key: String,
) -> Result<ResponseJson<WorkspaceDetailsResponse>, Response> {
    let now = chrono::Utc::now().to_string();

    if !workspace_root.exists() {
        if missing_copy_is_transient {
            return Err(workspace_materializing_response(workspace_root));
        }
        // The 200 with safe defaults and git shown as unavailable. Whether it
        // carries a user-visible message depends on who is asking:
        //
        //   - LOCAL: the directory really is gone and the path is the user's
        //     own filesystem, so naming it is the actionable thing to say.
        //   - CLOUD serve replica: this is the DOCUMENTED degrade for a down
        //     ide, i.e. expected operation — not an error. A server-side path
        //     is our vocabulary, not the user's; they can neither verify nor
        //     act on it. And the FE toasts `workspace_error` AND redirects to
        //     the org root, which would defeat the very degrade this path
        //     exists to serve. `git_mode: None` already tells the UI that git
        //     is unavailable, and the ide-unavailable banner (raised by the
        //     other IdeOnly requests 502ing) explains why.
        let workspace_error = is_local.then(|| {
            format!(
                "Workspace directory not found: {}",
                workspace_root.display()
            )
        });
        return Ok(no_git_response(
            workspace_id,
            name,
            now,
            workspace_error,
            false,
            current_user_role,
            storage_key,
        ));
    }

    // Local-mode servers disable all git features — routes are not
    // mounted, capabilities must match. Force None regardless of
    // what lives on disk.
    if is_local {
        return Ok(no_git_response(
            workspace_id,
            name,
            now,
            None,
            false,
            current_user_role,
            storage_key,
        ));
    }

    let git = default_git_client();
    let git_mode = detect_git_mode(workspace_root).await;
    let has_local_repo = !matches!(git_mode, GitMode::None);

    let default_branch = if has_local_repo {
        git.get_default_branch(workspace_root).await
    } else {
        "main".to_string()
    };

    let current_branch = git
        .get_current_branch(workspace_root)
        .await
        .unwrap_or_else(|_| default_branch.clone());

    // Fall back to [default_branch] on any error or when there's no local repo.
    let protected_branches: Vec<String> = if has_local_repo {
        let config_branches = match ConfigBuilder::new().with_workspace_path(workspace_root) {
            Ok(builder) => match builder
                .build_with_working_copy(oxy::config::Origin::Disk, oxy::config::OnMissing::Empty)
                .await
            {
                Ok(manager) => manager.protected_branches().map(|b| b.to_vec()),
                Err(err) => {
                    tracing::warn!(
                        workspace_path = %workspace_root.display(),
                        error = %err,
                        "failed to build config for protected_branches; falling back to default"
                    );
                    None
                }
            },
            Err(err) => {
                tracing::warn!(
                    workspace_path = %workspace_root.display(),
                    error = %err,
                    "failed to build config for protected_branches; falling back to default"
                );
                None
            }
        };
        config_branches.unwrap_or_else(|| vec![default_branch.clone()])
    } else {
        vec![default_branch.clone()]
    };

    Ok(ResponseJson(WorkspaceDetailsResponse {
        id: workspace_id,
        name: name.to_string(),
        workspace_id: Uuid::nil(),
        created_at: now.clone(),
        updated_at: now.clone(),
        active_branch: Some(ProjectBranch {
            id: Uuid::nil(),
            name: current_branch,
            revision: String::new(),
            workspace_id,
            branch_type: BranchType::Local,
            origin: BranchOrigin::Both,
            created_at: now.clone(),
            updated_at: now,
        }),
        workspace_error: None,
        git_mode,
        capabilities: git_mode.into(),
        default_branch,
        protected_branches,
        requires_local_setup: false,
        current_user_role,
        storage_key,
    }))
}

/// Get all branches for a specific workspace.
///
/// This endpoint retrieves all branches (both local and remote) associated with a workspace.
/// Returns branch metadata including names, revisions, sync status, and timestamps.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}/branches",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "Branches retrieved successfully", body = WorkspaceBranchesResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Workspace not found"),
        (status = 500, description = "Internal server error")
    ),
    security(
        ("ApiKey" = [])
    ),
    tag = "Workspaces"
)]
pub async fn get_workspace_branches(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Extension(ws): Extension<entity::workspaces::Model>,
    root: WorkspaceRootWorkingCopy,
) -> Result<ResponseJson<WorkspaceBranchesResponse>, StatusCode> {
    info!("Getting branches for workspace: {}", ws.id);

    let root = root.root_path().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let branches = git_list_branches(&root, Some(&ws), ws.id, Some(user.id)).await;
    info!("Found {} branches", branches.len());
    Ok(ResponseJson(WorkspaceBranchesResponse { branches }))
}

pub async fn delete_branch(
    _: WorkspaceAdmin,
    Extension(ws): Extension<entity::workspaces::Model>,
    Path((_workspace_id, branch_name)): Path<(Uuid, String)>,
    root: WorkspaceRootWorkingCopy,
) -> Result<ResponseJson<ProjectResponse>, StatusCode> {
    let _ = &ws;
    let root = root.root_path().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    match git_delete_branch(&root, &branch_name).await {
        Ok(()) => Ok(ResponseJson(ProjectResponse {
            success: true,
            message: format!("Branch '{}' deleted", branch_name),
        })),
        Err(e) => {
            error!("{}", e);
            Ok(ResponseJson(ProjectResponse {
                success: false,
                message: format!("{e}"),
            }))
        }
    }
}

pub async fn switch_workspace_branch(
    _: WorkspaceEditor,
    State(app_state): State<AppState>,
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Extension(ws): Extension<entity::workspaces::Model>,
    root: WorkspaceRootWorkingCopy,
    Json(request): Json<SwitchBranchRequest>,
) -> Result<ResponseJson<ProjectBranch>, StatusCode> {
    info!("Switching branch for workspace: {}", ws.id);

    let root = root.root_path().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    // Validate branch name before it reaches the shell.
    if let Err(e) = default_git_client().validate_branch_name(&request.branch) {
        error!("Invalid branch name '{}': {}", request.branch, e);
        return Err(StatusCode::BAD_REQUEST);
    }

    let branch = git_switch_branch(&root, &request.branch, Some(&ws), ws.id, Some(user.id))
        .await
        .map_err(|e| {
            error!("{}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // A branch switch changes the working copy in place (TTL 60s, both
    // caches). Without this the world-model + semantic detail panels serve the
    // previous branch's layer until the TTL lapses. Same invalidation
    // `save_file` does. Note both branches' working copies key as
    // `LayerSource::WorkingCopy`, so a branch switch is still the only thing
    // separating their layers — the source in the key does not do it.
    app_state.semantic_layer_cache.invalidate_workspace(ws.id);
    app_state.semantic_engine_cache.invalidate_workspace(ws.id);

    Ok(ResponseJson(branch))
}

pub async fn get_workspace_status(
    State(_app_state): State<AppState>,
    WorkspaceManagerWorkingCopy(workspace_manager): WorkspaceManagerWorkingCopy,
    Extension(workspace): Extension<entity::workspaces::Model>,
    Path(WorkspacePath { workspace_id }): Path<WorkspacePath>,
) -> Result<axum::response::Json<ProjectStatus>, StatusCode> {
    use entity::workspaces::WorkspaceStatus;

    // Short-circuit when the row itself records a clone failure or a missing
    // `config.yml`. In both cases WorkspaceBuilder has nothing to load, so we
    // return the persisted error straight through.
    if matches!(
        workspace.status,
        WorkspaceStatus::Failed | WorkspaceStatus::NotOxyProject
    ) {
        return Ok(axum::response::Json(ProjectStatus {
            required_secrets: None,
            is_config_valid: false,
            error: workspace.error,
        }));
    }

    let workspace_path = workspace_manager.config_manager.workspace_path();

    let (is_config_valid, required_secrets, error) =
        match WorkspaceBuilder::<WorkingCopy>::new(workspace_id)
            .with_working_copy(&workspace_path, None, oxy::config::OnMissing::Fail)
            .await
        {
            Ok(_builder) => (true, Some(Vec::new()), None),
            Err(e) => {
                error!("Failed to create workspace builder: {}", e);
                (false, None, Some(e.to_string()))
            }
        };

    let status = ProjectStatus {
        required_secrets,
        is_config_valid,
        error,
    };

    Ok(axum::response::Json(status))
}

/// GET /orgs/{org_id}/workspaces — list workspaces in the given org.
/// Membership is enforced by org_middleware; this handler just scopes the query.
/// The workspaces in one organization.
///
/// The second step of turning a customer into ids: `{workspace_id}` is the
/// path parameter almost every other endpoint takes. Membership is enforced by
/// `org_middleware`, so this only ever returns workspaces the caller can reach.
#[utoipa::path(
    method(get),
    path = "/orgs/{org_id}/workspaces",
    params(
        ("org_id" = Uuid, Path, description = "Organization UUID")
    ),
    responses(
        (status = OK, description = "Workspaces in the organization", body = Vec<WorkspaceSummary>, content_type = "application/json")
    ),
    security(
        ("BearerAuth" = [])
    )
)]
pub async fn list_workspaces(
    crate::server::api::middlewares::org_context::OrgContextExtractor(ctx): crate::server::api::middlewares::org_context::OrgContextExtractor,
    State(_app_state): State<AppState>,
) -> Result<ResponseJson<Vec<WorkspaceSummary>>, StatusCode> {
    use entity::prelude::Workspaces;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|e| {
            error!("Failed to connect to database: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Never the nil UUID: it is the legacy `--local` workspace id, which this
    // (cloud-only) surface's own `workspace_middleware` refuses with a 404. A
    // database a `--local` run once touched keeps a nil "Local" row in its org,
    // and listing it lands the post-login dispatcher on a workspace that spins.
    let workspaces = Workspaces::find()
        .filter(entity::workspaces::Column::OrgId.eq(ctx.org.id))
        .filter(entity::workspaces::Column::Id.ne(Uuid::nil()))
        .all(&db)
        .await
        .map_err(|e| {
            error!("Failed to list workspaces: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Batch-fetch creator names for all workspaces that have a created_by set.
    let creator_ids: Vec<Uuid> = workspaces
        .iter()
        .filter_map(|p| p.created_by)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let creator_names: std::collections::HashMap<Uuid, String> = if creator_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        use entity::prelude::Users;
        use sea_orm::{ColumnTrait, QueryFilter};
        Users::find()
            .filter(entity::users::Column::Id.is_in(creator_ids))
            .all(&db)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|u| {
                let display = if u.name.is_empty() {
                    u.email.unwrap_or_default()
                } else {
                    u.name
                };
                (u.id, display)
            })
            .collect()
    };

    let summary_futures = workspaces.into_iter().map(|p| {
        let creator_names = creator_names.clone();
        async move {
            // Gather file counts and git metadata when the path exists on disk.
            let (agent_count, workflow_count, app_count, git_remote, git_commit, git_updated_at) =
                if let Some(ref path_str) = p.path {
                    let dir = std::path::PathBuf::from(path_str);
                    if dir.exists() {
                        let (agent_count, workflow_count, app_count) =
                            tokio::task::spawn_blocking({
                                let dir = dir.clone();
                                move || {
                                    (
                                        Some(count_yml_suffix(&dir, "agent")),
                                        Some(
                                            count_yml_suffix(&dir, "automation")
                                                + count_yml_suffix(&dir, "procedure")
                                                + count_yml_suffix(&dir, "workflow"),
                                        ),
                                        Some(count_yml_suffix(&dir, "app")),
                                    )
                                }
                            })
                            .await
                            .unwrap_or((None, None, None));
                        let git = default_git_client();
                        let (remote, (sha, msg), updated_at) = tokio::join!(
                            git.get_remote_url(&dir),
                            git.get_branch_commit(&dir, "HEAD"),
                            git.get_head_commit_relative_date(&dir)
                        );
                        let commit = if sha.is_empty() {
                            None
                        } else {
                            Some(format!("{} — {}", &sha[..sha.len().min(7)], msg))
                        };
                        (
                            agent_count,
                            workflow_count,
                            app_count,
                            remote,
                            commit,
                            updated_at,
                        )
                    } else {
                        (None, None, None, None, None, None)
                    }
                } else {
                    (None, None, None, None, None, None)
                };

            WorkspaceSummary {
                id: p.id,
                org_id: p.org_id,
                name: p.name,
                path: p.path,
                created_at: p.created_at.into(),
                last_opened_at: p.last_opened_at.map(|t| t.into()),
                created_by_name: p.created_by.and_then(|id| creator_names.get(&id).cloned()),
                agent_count,
                workflow_count,
                app_count,
                git_remote,
                git_commit,
                git_updated_at,
                status: p.status,
                error: p.error,
            }
        }
    });

    let summaries = futures::future::join_all(summary_futures).await;

    Ok(ResponseJson(summaries))
}

// SECURITY: the access rule is "Org Owner/Admin OR the workspace creator".
// The creator branch needs `ctx.membership.user_id` plus `workspace.created_by`,
// which is only available after a DB lookup — so we can't use a pure typed
// guard like `OrgAdmin` here. Instead we use `OrgContextExtractor` to surface
// the membership and delegate the role check to
// `ensure_org_admin_or_workspace_creator`, which lives in `role_guards.rs`
// alongside the other role checks.
pub async fn rename_workspace(
    OrgContextExtractor(ctx): OrgContextExtractor,
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path((org_id, workspace_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<RenameWorkspaceRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    use entity::prelude::Workspaces;
    use entity::workspaces;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, Set};

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Workspace name cannot be empty".to_string(),
        ));
    }

    // Guard against control characters and non-printable input.
    if !name.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err((
            StatusCode::BAD_REQUEST,
            "Workspace name contains invalid characters".to_string(),
        ));
    }

    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|e| {
            error!("Failed to connect to database: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Database connection failed: {e}"),
            )
        })?;

    // Fetch the workspace first so we can check ownership before doing anything else.
    let workspace = Workspaces::find_by_id(workspace_id)
        .one(&db)
        .await
        .map_err(|e| {
            error!("Failed to find workspace {}: {}", workspace_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to find workspace: {e}"),
            )
        })?
        .ok_or((StatusCode::NOT_FOUND, "Workspace not found".to_string()))?;

    if workspace.org_id != Some(org_id) {
        return Err((StatusCode::NOT_FOUND, "Workspace not found".to_string()));
    }

    // Org admins/owners can rename any workspace; Org Members can rename only those
    // they created (role_guards::ensure_org_admin_or_workspace_creator) — the one
    // authorization rule outside the role guards, now enforced through the unified
    // WorkspaceRename ring. `existing && unified`, so the model can only tighten it.
    let legacy = ensure_org_admin_or_workspace_creator(&ctx, &workspace).is_ok();
    let allowed = authz::enforce_for(
        &db,
        actor.id,
        actor.email.as_deref().unwrap_or(""),
        "workspace.rename",
        authz::Action::WorkspaceRename,
        authz::Resource::workspace_with_creator(workspace.id, org_id, workspace.created_by),
        legacy,
    )
    .await;
    if !allowed {
        return Err((
            StatusCode::FORBIDDEN,
            "Only admins or workspace creator can rename".to_string(),
        ));
    }

    // Reject duplicate names within the same org.
    let name_taken = Workspaces::find()
        .filter(workspaces::Column::Name.eq(&name))
        .filter(workspaces::Column::Id.ne(workspace_id))
        .filter(workspaces::Column::OrgId.eq(org_id))
        .one(&db)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to query workspaces: {e}"),
            )
        })?
        .is_some();

    if name_taken {
        return Err((
            StatusCode::CONFLICT,
            format!("A workspace named '{name}' already exists. Please choose a different name."),
        ));
    }

    let mut active: workspaces::ActiveModel = workspace.into_active_model();
    active.name = Set(name.clone());
    active.updated_at = Set(chrono::Utc::now().into());
    active.save(&db).await.map_err(|e| {
        error!("Failed to rename workspace {}: {}", workspace_id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to rename workspace: {e}"),
        )
    })?;

    info!("Renamed workspace {} to '{}'", workspace_id, name);
    Ok(StatusCode::OK)
}

pub async fn delete_workspace(
    // `OrgAdmin` FIRST: axum runs extractors in argument order, and resolving a
    // workspace path before checking authority would leak existence.
    _: OrgAdmin,
    State(_app_state): State<AppState>,
    Path((org_id, workspace_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<DeleteProjectQuery>,
    root: WorkspaceRootWorkingCopy,
) -> Result<StatusCode, StatusCode> {
    use sea_orm::ModelTrait;

    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|e| {
            error!("Failed to connect to database: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // The extractor already loaded it, and building the manager needed it — so
    // querying again here would be a second round trip for the same row.
    let workspace = root.workspace.clone().ok_or(StatusCode::NOT_FOUND)?;

    if workspace.org_id != Some(org_id) {
        return Err(StatusCode::NOT_FOUND);
    }

    // From the extractor, so the signature states that this handler reaches a
    // working copy. It used to resolve the path here by hand, which is how a
    // filesystem delete came to be described by a signature naming no manager.
    let workspace_path = root
        .manager
        .as_ref()
        .map(|m| m.config_manager.workspace_path().to_path_buf());

    workspace.delete(&db).await.map_err(|e| {
        error!("Failed to delete workspace {}: {}", workspace_id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    info!("Deleted workspace {}", workspace_id);

    // Clean up the now-orphaned schedule rows *after* the workspace is gone: if
    // the delete above had failed we'd have stripped a still-live workspace of
    // its workflow/monitor schedules (which, unlike `health_eval`, startup
    // reconcile never recreates).
    cleanup_workspace_schedules(&db, workspace_id).await;

    // Only remove files from disk when the caller explicitly opts in.
    // Without `?delete_files=true` we only remove the DB record, leaving
    // the directory intact so an accidental delete can be recovered.
    if query.delete_files
        && let Some(path) = workspace_path
    {
        if path.exists() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!("Failed to delete workspace directory {:?}: {}", path, e);
            } else {
                info!("Deleted workspace directory {:?}", path);
            }
        } else if !oxy::workspace_fs_probe::process_owns_workspace_files() {
            // The route is IdeOnly so this should be unreachable, but a missing
            // directory means two different things and only one of them is
            // "nothing to delete". On a pod with no working copy it means the
            // files are elsewhere, still there, and the 200 below is a lie.
            tracing::error!(
                %workspace_id,
                path = %path.display(),
                "delete_files requested on a process that owns no working copy; the \
                 directory was NOT deleted and still exists on the ide. This route is \
                 classified IdeOnly — reaching here means routing did not hold."
            );
        }
    }

    Ok(StatusCode::OK)
}

#[cfg(test)]
mod compute_ahead_behind_tests {
    use super::compute_ahead_behind;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::process::Command;

    async fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .await
            .expect("spawn git");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    async fn init_repo_with_main(n_commits: usize) -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]).await;
        git(root, &["config", "user.email", "t@t"]).await;
        git(root, &["config", "user.name", "T"]).await;
        for i in 0..n_commits {
            tokio::fs::write(root.join(format!("f{i}.txt")), format!("v{i}"))
                .await
                .unwrap();
            git(root, &["add", "."]).await;
            git(root, &["commit", "-m", &format!("c{i}")]).await;
        }
        dir
    }

    async fn current_sha(root: &Path) -> String {
        git(root, &["rev-parse", "HEAD"]).await.trim().to_string()
    }

    #[tokio::test]
    async fn never_pushed_feature_branch_reports_ahead_of_default() {
        let dir = init_repo_with_main(1).await;
        let root = dir.path();
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        )
        .await;
        git(root, &["checkout", "-b", "feature/x"]).await;
        for i in 0..3 {
            tokio::fs::write(root.join(format!("feat{i}.txt")), "x")
                .await
                .unwrap();
            git(root, &["add", "."]).await;
            git(root, &["commit", "-m", &format!("feat{i}")]).await;
        }
        let local_sha = current_sha(root).await;
        let (ahead, behind) = compute_ahead_behind(root, &local_sha, None, true).await;
        assert_eq!(
            (ahead, behind),
            (3, 0),
            "expected (3, 0), got ({ahead}, {behind})"
        );
    }

    #[tokio::test]
    async fn never_pushed_no_remote_reports_zero() {
        let dir = init_repo_with_main(1).await;
        let root = dir.path();
        git(root, &["checkout", "-b", "feature/y"]).await;
        tokio::fs::write(root.join("a.txt"), "a").await.unwrap();
        git(root, &["add", "."]).await;
        git(root, &["commit", "-m", "feat"]).await;
        let local_sha = current_sha(root).await;
        let (ahead, behind) = compute_ahead_behind(root, &local_sha, None, false).await;
        assert_eq!((ahead, behind), (0, 0));
    }

    #[tokio::test]
    async fn empty_local_sha_returns_zero() {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init", "-b", "main"]).await;
        let (ahead, behind) = compute_ahead_behind(dir.path(), "", None, true).await;
        assert_eq!((ahead, behind), (0, 0));
    }

    #[tokio::test]
    async fn tracking_ref_present_synced_returns_zero() {
        let dir = init_repo_with_main(1).await;
        let root = dir.path();
        let local_sha = current_sha(root).await;
        let (ahead, behind) = compute_ahead_behind(root, &local_sha, Some(&local_sha), true).await;
        assert_eq!((ahead, behind), (0, 0));
    }

    #[tokio::test]
    async fn default_branch_with_no_upstream_reports_zero() {
        let dir = init_repo_with_main(2).await;
        let root = dir.path();
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        )
        .await;
        let local_sha = current_sha(root).await;
        let (ahead, behind) = compute_ahead_behind(root, &local_sha, None, true).await;
        assert_eq!((ahead, behind), (0, 0));
    }
}
