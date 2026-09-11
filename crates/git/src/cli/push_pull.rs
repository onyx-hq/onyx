use std::path::Path;

use oxy_shared::errors::OxyError;
use tracing::info;

use crate::cli::{branch, run};

/// Push the current branch in `root` to its upstream remote.
///
/// `push.autoSetupRemote=true` is passed transiently so the first push of a
/// new branch creates the upstream tracking ref without permanently mutating
/// `~/.gitconfig`.
pub async fn push_to_remote(root: &Path, token: Option<&str>) -> Result<(), OxyError> {
    let b = branch::get_current_branch(root).await?;
    info!("Pushing branch '{}' in {} to remote", b, root.display());

    run::run_with_token(
        root,
        &["-c", "push.autoSetupRemote=true", "push", "origin", &b],
        token,
    )
    .await?;
    info!("Push successful");
    Ok(())
}

/// Force-pushes the current branch using `--force-with-lease`.
pub async fn force_push_to_remote(root: &Path, token: Option<&str>) -> Result<(), OxyError> {
    let b = branch::get_current_branch(root).await?;
    info!(
        "Force-pushing branch '{}' in {} to remote",
        b,
        root.display()
    );
    run::run_with_token(
        root,
        &[
            "-c",
            "push.autoSetupRemote=true",
            "push",
            "--force-with-lease",
            "origin",
            &b,
        ],
        token,
    )
    .await?;
    info!("Force push successful");
    Ok(())
}

/// `git fetch origin <branch>` — non-destructive: updates only the
/// remote-tracking ref `refs/remotes/origin/<branch>` (and `FETCH_HEAD`).
/// The local branch is never touched, so this is safe to call from a
/// worktree that is currently on `branch` regardless of divergence.
pub async fn fetch_remote_ref(
    root: &Path,
    branch: &str,
    token: Option<&str>,
) -> Result<(), OxyError> {
    branch::validate_branch_name(branch)?;
    // `debug`: the fetch-maintenance loop runs this per workspace on a timer,
    // and at `info` it was the second-loudest line on `oxy-ide` (168 an hour in
    // prod) while saying nothing on the path where nothing went wrong — a
    // failure surfaces from `run_with_token` as an error with its own context.
    tracing::debug!(branch, root = %root.display(), "fetching origin ref");
    run::run_with_token(root, &["fetch", "origin", branch], token).await?;
    Ok(())
}

/// `git pull --rebase origin <branch>` inside a worktree.
///
/// Runs entirely inside `worktree_root` so rebase state is scoped to the
/// worktree's own gitdir and doesn't block other worktrees.
///
/// Returns what the pull actually *did*. The caller cannot infer this from a
/// bare `Ok(())`: `git pull` exits zero both when it fast-forwards twenty
/// commits and when it does nothing at all, and `run` captures only stderr so
/// git's own "Already up to date." never reaches us. Reporting a fixed success
/// string for both is what let a workspace sit un-advanced while the UI claimed
/// it had synced — see oxygen-workspace-sync-bugs.md bug 1.
pub async fn pull_from_remote(
    worktree_root: &Path,
    branch: &str,
    token: Option<&str>,
) -> Result<PullOutcome, OxyError> {
    info!("Pulling {} in {}", branch, worktree_root.display());
    let before = head_sha(worktree_root).await;

    run::run_with_token(
        worktree_root,
        &["pull", "--rebase", "origin", branch],
        token,
    )
    .await?;

    let after = head_sha(worktree_root).await;
    // Measured against the freshly-updated tracking ref, so both numbers
    // describe the post-pull world:
    //   pulled   — commits origin had that we didn't (0 ⇒ genuinely nothing to do)
    //   unpushed — local commits the rebase replayed on top, which remain
    //              absent from origin and will block the next fast-forward
    let tracking = get_tracking_ref_sha(worktree_root, branch).await;
    let (pulled, unpushed) = match tracking.as_deref() {
        Some(remote) => (
            count_range(worktree_root, &before, remote).await,
            count_range(worktree_root, remote, &after).await,
        ),
        None => (0, 0),
    };

    info!(
        "Pull complete: {} → {} ({pulled} pulled, {unpushed} local ahead)",
        short(&before),
        short(&after)
    );
    Ok(PullOutcome {
        before_sha: before,
        after_sha: after,
        pulled,
        unpushed,
    })
}

/// What a `pull_from_remote` actually changed.
#[derive(Debug, Clone)]
pub struct PullOutcome {
    pub before_sha: String,
    pub after_sha: String,
    /// Commits brought in from origin. `0` means the pull was a genuine no-op.
    pub pulled: usize,
    /// Local commits not on origin after the pull. Non-zero means the workspace
    /// is ahead — the state that blocks fast-forward pulls and makes restore refuse.
    pub unpushed: usize,
}

impl PullOutcome {
    /// A message that can be checked against reality, unlike a fixed
    /// "Pulled latest changes from remote".
    pub fn summary(&self) -> String {
        let mut msg = if self.pulled == 0 {
            "Already up to date with origin".to_string()
        } else {
            format!(
                "Pulled {} commit{} — now at {}",
                self.pulled,
                if self.pulled == 1 { "" } else { "s" },
                short(&self.after_sha)
            )
        };
        if self.unpushed > 0 {
            msg.push_str(&format!(
                "; {} local commit{} not on origin",
                self.unpushed,
                if self.unpushed == 1 { "" } else { "s" }
            ));
        }
        msg
    }
}

async fn head_sha(root: &Path) -> String {
    run::run(root, &["rev-parse", "HEAD"])
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// `git rev-list --count from..to`, or 0 when either end is unknown.
async fn count_range(root: &Path, from: &str, to: &str) -> usize {
    if from.is_empty() || to.is_empty() || from == to {
        return 0;
    }
    run::run(root, &["rev-list", "--count", &format!("{from}..{to}")])
        .await
        .ok()
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// Returns `true` if `local_sha` is behind `remote_sha`.  `remote_sha` not
/// being in the local object store is treated as "behind".
pub async fn is_behind_remote(root: &Path, local_sha: &str, remote_sha: &str) -> bool {
    get_ahead_behind_counts(root, local_sha, remote_sha).await.1 > 0
}

/// Returns `(ahead_count, behind_count)` of `local_sha` relative to `remote_sha`.
/// Returns `(0, 0)` if either SHA is empty. If `remote_sha` is missing from the
/// local object store, treats as fully behind.
pub async fn get_ahead_behind_counts(root: &Path, local_sha: &str, remote_sha: &str) -> (u64, u64) {
    if local_sha.is_empty() || remote_sha.is_empty() {
        return (0, 0);
    }
    if local_sha == remote_sha {
        return (0, 0);
    }
    let range = format!("{local_sha}...{remote_sha}");
    match run::run(root, &["rev-list", "--left-right", "--count", &range]).await {
        Ok(output) => {
            let mut parts = output.split_whitespace();
            let ahead = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let behind = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            (ahead, behind)
        }
        // rev-list failed (parse error, transient git, unreachable remote
        // SHA). Returning `(0, 1)` would surface a phantom ↓1 with an
        // enabled Pull button that just fails again; `(0, 0)` lets an
        // explicit Fetch recover real counts.
        Err(err) => {
            tracing::warn!(
                "get_ahead_behind_counts: rev-list failed for {range}: {err}; reporting (0, 0)"
            );
            (0, 0)
        }
    }
}

/// When this clone last contacted origin, from the mtime of `FETCH_HEAD`
/// (which every `fetch` and `pull` rewrites). `None` if it has never fetched.
///
/// This is the missing half of [`get_tracking_ref_sha`]: that function answers
/// "what did origin point at?" from a purely local cache, with no indication of
/// how old the answer is. A caller comparing against a tracking ref that has
/// not been refreshed in hours will conclude the workspace is up to date when
/// it is simply uninformed — the report's `behind: 0` on a demonstrably behind
/// workspace. Any surface that renders a remote SHA must render this alongside.
pub async fn last_fetch_at(root: &Path) -> Option<std::time::SystemTime> {
    let git_dir = run::run(root, &["rev-parse", "--git-dir"]).await.ok()?;
    let fetch_head = root.join(git_dir.trim()).join("FETCH_HEAD");
    tokio::fs::metadata(&fetch_head).await.ok()?.modified().ok()
}

/// Returns the SHA that `origin/{branch}` points to (locally cached; no
/// network call). See [`last_fetch_at`] for how stale that cache may be.
pub async fn get_tracking_ref_sha(root: &Path, branch: &str) -> Option<String> {
    let tracking_ref = format!("origin/{branch}");
    run::run(root, &["rev-parse", &tracking_ref])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Returns the URL of the `origin` remote, or `None` if not configured.
pub async fn get_remote_url(workspace_root: &Path) -> Option<String> {
    run::run(workspace_root, &["remote", "get-url", "origin"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
