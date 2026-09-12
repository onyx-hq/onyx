//! Open a PR on `OXY_CUSTOMER_APPS_REPO` scaffolding the
//! `apps/<org_slug>/<app_slug>/` directory for a freshly-registered app.
//!
//! Reuses the existing GitHub App credentials (`GITHUB_APP_ID` +
//! `GITHUB_APP_PRIVATE_KEY`) via [`oxy::github::app_auth::GitHubAppAuth`].
//! The app must be installed on the owner of `OXY_CUSTOMER_APPS_REPO`
//! (e.g. `oxy-hq`) with `contents: write` and `pull_requests: write`
//! permissions on that repo. The installation id is looked up once at
//! first use and cached for the process lifetime.
//!
//! Each scaffold produces **one** commit on the new branch regardless of
//! template-file count. The Contents API (`PUT /contents/{path}`) creates
//! a separate commit per file — wrong for our use case, where the whole
//! scaffold is one logical unit. So we go through the Git Data API
//! instead: create blobs in parallel, create a single tree referencing
//! them, create one commit, point the branch at it, then open the PR.
//! Wall-clock cost is dominated by the parallel blob-create batch
//! (≈1 round-trip total) rather than N sequential file-PUTs.
//!
//! All GitHub API calls go through `reqwest` directly with a 30s per-call
//! timeout — without it a stalled GitHub leaves the scaffold (and the
//! caller's POST) hanging until the proxy gives up.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use entity::{apps, organizations};
use futures::future::try_join_all;
use oxy::github::app_auth::GitHubAppAuth;
use reqwest::{Client, header::AUTHORIZATION};
use sea_orm::DatabaseConnection;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

/// Per-call timeout for every GitHub API request. Picked so a stalled
/// upstream surfaces as a clean `Transport` error within a connection-pool
/// keep-alive window, instead of the request hanging until Axum's outer
/// timeout fires (if any).
const GITHUB_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors the scaffold service surfaces. Display impls aim for log
/// readability — the create-app handler maps anything from here to a 502
/// after rolling the app row back, so the caller just sees "scaffold
/// failed, app not created".
#[derive(Debug)]
pub enum ScaffoldError {
    /// `OXY_CUSTOMER_APPS_REPO` is set but malformed (no `/`).
    MalformedRepo(String),
    /// No installation of the GitHub App on the configured repo's owner.
    InstallationMissing(String),
    /// Inner GitHub auth failure (JWT signing, token mint, …).
    Auth(String),
    /// Any GitHub API call returned a non-2xx response.
    GitHubApi(String),
    /// reqwest/transport failure.
    Transport(String),
    /// Template renderer rejected the request (unknown `template_id`,
    /// malformed `template.json`, …). Caller-controlled state, so we
    /// surface as a real error variant rather than panicking — the
    /// handler validator catches this up-front today, but if a future
    /// caller bypasses the validator we'd rather 500 cleanly than
    /// take down the service.
    Template(String),
}

impl std::fmt::Display for ScaffoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScaffoldError::MalformedRepo(s) => {
                write!(f, "OXY_CUSTOMER_APPS_REPO={s:?} is not in owner/repo form")
            }
            ScaffoldError::InstallationMissing(owner) => write!(
                f,
                "GitHub App is not installed on org {owner:?}; install it with contents:write + pull_requests:write"
            ),
            ScaffoldError::Auth(e) => write!(f, "GitHub auth failed: {e}"),
            ScaffoldError::GitHubApi(e) => write!(f, "GitHub API error: {e}"),
            ScaffoldError::Transport(e) => write!(f, "Transport error: {e}"),
            ScaffoldError::Template(e) => write!(f, "Template render failed: {e}"),
        }
    }
}

impl std::error::Error for ScaffoldError {}

/// Top-level entry. Returns the URL of the opened PR on success.
///
/// `_db` is accepted but currently unused — kept on the signature so the
/// service can persist scaffold state in a future iteration without
/// changing every caller.
///
/// `template_id` selects which registered template to render. Must be a
/// validated id from the registry (callers should use
/// `handlers::validate_template_id` before calling this).
pub async fn scaffold_pr(
    _db: &DatabaseConnection,
    app: &apps::Model,
    org: &organizations::Model,
    template_id: &str,
) -> Result<String, ScaffoldError> {
    // `OXY_CUSTOMER_APPS_REPO` defaults to the canonical oxy-hq monorepo
    // so admins running against the standard infra don't have to set it.
    // Override only when forking the repo or testing against a sandbox.
    let repo_full = std::env::var("OXY_CUSTOMER_APPS_REPO")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "oxy-hq/customer-apps".to_string());
    let (owner, repo) = repo_full
        .split_once('/')
        .ok_or_else(|| ScaffoldError::MalformedRepo(repo_full.clone()))?;

    let auth = GitHubAppAuth::from_env().map_err(|e| ScaffoldError::Auth(e.to_string()))?;
    let installation_id = resolve_installation_id(&auth, owner).await?;
    let token = auth
        .get_installation_token(&installation_id)
        .await
        .map_err(|e| ScaffoldError::Auth(e.to_string()))?;

    let client = Client::builder()
        .user_agent("oxy-app/customer-apps-scaffold")
        .timeout(GITHUB_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
    let api = GitHubApi {
        client,
        token,
        owner: owner.to_string(),
        repo: repo.to_string(),
    };

    let main_commit_sha = api.head_sha("main").await?;
    let main_tree_sha = api.commit_tree_sha(&main_commit_sha).await?;
    let branch = format!("bootstrap/customer-app-{}-{}", org.slug, app.slug);

    // Stage every scaffold file as a blob in parallel; the resulting
    // (path, blob_sha) entries become a single tree, then a single commit.
    let dir = format!("apps/{}/{}", org.slug, app.slug);
    let files = scaffold_files(app, org, template_id)?;
    let blob_creates = files.into_iter().map(|(rel_path, contents)| {
        let api = &api;
        let path = format!("{dir}/{rel_path}");
        async move {
            let sha = api.create_blob(&contents).await?;
            Ok::<TreeEntry, ScaffoldError>(TreeEntry { path, sha })
        }
    });
    let tree_entries = try_join_all(blob_creates).await?;

    let new_tree_sha = api.create_tree(&main_tree_sha, &tree_entries).await?;
    let commit_message = format!("scaffold: bootstrap {dir} ({} files)", tree_entries.len());
    let new_commit_sha = api
        .create_commit(&commit_message, &new_tree_sha, &main_commit_sha)
        .await?;
    api.create_ref(&branch, &new_commit_sha).await?;

    let title = format!(
        "feat: bootstrap custom app {} ({}/{})",
        app.name, org.slug, app.slug
    );
    let body = format!(
        "Automated PR opened by oxy on app registration. Merge to deploy the app.\n\
         \n\
         - **App ID:** `{}`\n\
         - **Path after merge + first sync:** `/customer-apps/{}/{}/` on the oxy deployment\n",
        app.id, org.slug, app.slug,
    );
    api.open_pr(&branch, "main", &title, &body).await
}

/// Cache the installation id we resolve at first use. TTL bounds the
/// staleness window so a GitHub App reinstall, transfer, or revocation
/// is picked up within `INSTALLATION_ID_CACHE_TTL` instead of waiting
/// for an oxy process restart.
///
/// 10 minutes is a balance — long enough that the cache provides
/// meaningful relief from the round trip (which is the only reason it
/// exists), short enough that operators don't have to remember to
/// restart oxy after rotating the GitHub App.
const INSTALLATION_ID_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

struct InstallationCacheEntry {
    owner: String,
    id: String,
    cached_at: std::time::Instant,
}

fn installation_id_cache() -> &'static Mutex<Option<InstallationCacheEntry>> {
    static CACHE: OnceLock<Arc<Mutex<Option<InstallationCacheEntry>>>> = OnceLock::new();
    CACHE.get_or_init(|| Arc::new(Mutex::new(None)))
}

async fn resolve_installation_id(
    auth: &GitHubAppAuth,
    owner: &str,
) -> Result<String, ScaffoldError> {
    let mut cache = installation_id_cache().lock().await;
    if let Some(entry) = cache.as_ref()
        && entry.owner == owner
        && entry.cached_at.elapsed() < INSTALLATION_ID_CACHE_TTL
    {
        return Ok(entry.id.clone());
    }

    let installations = auth
        .list_installations()
        .await
        .map_err(|e| ScaffoldError::Auth(e.to_string()))?;
    let target = installations
        .into_iter()
        .find(|i| i.slug.eq_ignore_ascii_case(owner))
        .ok_or_else(|| ScaffoldError::InstallationMissing(owner.to_string()))?;
    let id = target.id.to_string();
    *cache = Some(InstallationCacheEntry {
        owner: owner.to_string(),
        id: id.clone(),
        cached_at: std::time::Instant::now(),
    });
    Ok(id)
}

/// Render the bundle skeleton that lands in `apps/<org>/<app>/` of the
/// customer-apps repo using the template identified by `template_id`.
///
/// `template_id` must already be validated against the registry by the
/// caller; passing an unknown id panics at the `expect` so failures
/// surface clearly in development rather than silently at PR-open time.
fn scaffold_files(
    app: &apps::Model,
    org: &organizations::Model,
    template_id: &str,
) -> Result<Vec<(String, String)>, ScaffoldError> {
    // Bake the served URL prefix into the rendered template so a
    // freshly-cloned bundle builds with correct asset paths on the
    // engineer's laptop without any env-var setup. CI still wins via
    // OXY_APP_BASE_PATH (see vite.config.ts in the template); this is
    // the local-dev fallback.
    let base_path = format!("/customer-apps/{}/{}/", org.slug, app.slug);
    let sub = crate::custom_app_template::Substitutions {
        app_slug: &app.slug,
        app_display_name: &app.name,
        app_base_path: &base_path,
    };
    crate::custom_app_template::render_template_files(template_id, &sub)
        .map_err(ScaffoldError::Template)
}

// ── GitHub REST surface (minimal) ────────────────────────────────────────

struct GitHubApi {
    client: Client,
    token: String,
    owner: String,
    repo: String,
}

#[derive(Deserialize)]
struct RefObject {
    sha: String,
}

#[derive(Deserialize)]
struct GitRef {
    object: RefObject,
}

#[derive(Deserialize)]
struct PrCreated {
    html_url: String,
}

#[derive(Deserialize)]
struct CommitObject {
    tree: CommitTree,
}

#[derive(Deserialize)]
struct CommitTree {
    sha: String,
}

#[derive(Deserialize)]
struct CreatedSha {
    sha: String,
}

/// A staged file ready to land in the new tree: a path relative to the
/// repo root plus the blob sha returned by `create_blob`.
struct TreeEntry {
    path: String,
    sha: String,
}

impl GitHubApi {
    fn url(&self, path: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}{path}",
            self.owner, self.repo
        )
    }

    async fn head_sha(&self, branch: &str) -> Result<String, ScaffoldError> {
        let url = self.url(&format!("/git/ref/heads/{branch}"));
        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "GET {url} → {}",
                resp.status()
            )));
        }
        let parsed: GitRef = resp
            .json()
            .await
            .map_err(|e| ScaffoldError::GitHubApi(format!("parse ref: {e}")))?;
        Ok(parsed.object.sha)
    }

    async fn create_ref(&self, branch: &str, sha: &str) -> Result<(), ScaffoldError> {
        let url = self.url("/git/refs");
        let body = json!({ "ref": format!("refs/heads/{branch}"), "sha": sha });
        let resp = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "POST {url} → {}",
                resp.status()
            )));
        }
        Ok(())
    }

    /// Read a commit object so we can pin the new tree to the same base
    /// as the parent commit (avoids enumerating every file on `main`).
    async fn commit_tree_sha(&self, commit_sha: &str) -> Result<String, ScaffoldError> {
        let url = self.url(&format!("/git/commits/{commit_sha}"));
        let resp = self
            .client
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "GET {url} → {}",
                resp.status()
            )));
        }
        let parsed: CommitObject = resp
            .json()
            .await
            .map_err(|e| ScaffoldError::GitHubApi(format!("parse commit: {e}")))?;
        Ok(parsed.tree.sha)
    }

    /// Stage a file's contents as a Git blob; returns the blob sha so the
    /// caller can reference it from a tree entry. `contents` is UTF-8;
    /// the API also accepts base64 for binary blobs but our templates are
    /// all text.
    async fn create_blob(&self, contents: &str) -> Result<String, ScaffoldError> {
        let url = self.url("/git/blobs");
        // We base64-encode regardless of textness — keeps the body wire
        // shape stable, and the API decodes back to bytes losslessly.
        let encoded = base64::engine::general_purpose::STANDARD.encode(contents.as_bytes());
        let body = json!({
            "content": encoded,
            "encoding": "base64",
        });
        let resp = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "POST {url} → {}",
                resp.status()
            )));
        }
        let parsed: CreatedSha = resp
            .json()
            .await
            .map_err(|e| ScaffoldError::GitHubApi(format!("parse blob: {e}")))?;
        Ok(parsed.sha)
    }

    /// Build a single tree containing every staged blob, anchored on
    /// `base_tree` so unrelated paths in `main` are preserved.
    async fn create_tree(
        &self,
        base_tree: &str,
        entries: &[TreeEntry],
    ) -> Result<String, ScaffoldError> {
        let url = self.url("/git/trees");
        let tree: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "path": e.path,
                    // 100644 = non-executable regular file, the only mode
                    // the scaffold template emits.
                    "mode": "100644",
                    "type": "blob",
                    "sha": e.sha,
                })
            })
            .collect();
        let body = json!({ "base_tree": base_tree, "tree": tree });
        let resp = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "POST {url} → {}",
                resp.status()
            )));
        }
        let parsed: CreatedSha = resp
            .json()
            .await
            .map_err(|e| ScaffoldError::GitHubApi(format!("parse tree: {e}")))?;
        Ok(parsed.sha)
    }

    /// Create one commit for the whole scaffold. Parent is `main`'s HEAD,
    /// so the new branch ref points at exactly one new commit.
    async fn create_commit(
        &self,
        message: &str,
        tree_sha: &str,
        parent_sha: &str,
    ) -> Result<String, ScaffoldError> {
        let url = self.url("/git/commits");
        let body = json!({
            "message": message,
            "tree": tree_sha,
            "parents": [parent_sha],
        });
        let resp = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "POST {url} → {}",
                resp.status()
            )));
        }
        let parsed: CreatedSha = resp
            .json()
            .await
            .map_err(|e| ScaffoldError::GitHubApi(format!("parse commit: {e}")))?;
        Ok(parsed.sha)
    }

    async fn open_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<String, ScaffoldError> {
        let url = self.url("/pulls");
        let payload = json!({
            "title": title,
            "head": head,
            "base": base,
            "body": body,
        });
        let resp = self
            .client
            .post(&url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| ScaffoldError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(ScaffoldError::GitHubApi(format!(
                "POST {url} → {}",
                resp.status()
            )));
        }
        let parsed: PrCreated = resp
            .json()
            .await
            .map_err(|e| ScaffoldError::GitHubApi(format!("parse pr: {e}")))?;
        Ok(parsed.html_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_app(name: &str, slug: &str) -> apps::Model {
        apps::Model {
            visibility: "org".to_string(),
            id: uuid::Uuid::nil(),
            slug: slug.to_string(),
            name: name.to_string(),
            org_id: uuid::Uuid::nil(),
            project_id: uuid::Uuid::nil(),
            branch: "main".to_string(),
            source_repo: "oxy-hq/customer-apps".to_string(),
            status: "created".to_string(),
            source_type: "s3".to_string(),
            source_config: serde_json::json!({}),
            bootstrap_pr_url: None,
            last_synced_at: None,
            manifest_override: None,
            published_at: None,
            repo_path: None,
            draft_build_id: None,
            published_build_id: None,
            last_promoted_by: None,
            last_promoted_at: None,
            created_at: chrono::Utc::now().fixed_offset(),
            updated_at: chrono::Utc::now().fixed_offset(),
        }
    }

    fn fake_org(slug: &str) -> organizations::Model {
        organizations::Model {
            id: uuid::Uuid::nil(),
            slug: slug.to_string(),
            name: slug.to_string(),
            logo: None,
            logo_content_type: None,
            created_at: chrono::Utc::now().fixed_offset(),
            updated_at: chrono::Utc::now().fixed_offset(),
        }
    }

    #[test]
    fn scaffold_files_emits_vite_bundle_skeleton() {
        // Smoke test: scaffold delegates to the shared Vite template
        // renderer (whose substitution + filtering invariants live in
        // custom_app_template::tests). Here we just confirm that
        // the delegation produces a recognisable Vite layout — if we
        // ever swap the default template, these assertions are the
        // tripwire that flags consumers to update.
        let app = fake_app("Acme Analytics", "acme-analytics");
        let org = fake_org("acme");
        let files = scaffold_files(&app, &org, "vite").expect("vite scaffold");
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert!(names.contains(&"package.json"), "got: {names:?}");
        assert!(names.contains(&"vite.config.ts"), "got: {names:?}");
        assert!(names.contains(&"src/App.tsx"), "got: {names:?}");
        assert!(names.contains(&"oxy-app.json"), "got: {names:?}");
    }

    #[test]
    fn scaffold_files_substitutes_app_slug_into_package_json() {
        let app = fake_app("Acme Analytics", "acme-analytics");
        let files = scaffold_files(&app, &fake_org("acme"), "vite").expect("vite scaffold");
        let pkg = files
            .iter()
            .find(|(p, _)| p == "package.json")
            .expect("package.json present");
        assert!(
            pkg.1.contains("\"name\": \"acme-analytics\""),
            "got: {}",
            pkg.1
        );
    }

    #[test]
    fn scaffold_files_omits_shared_repo_automation_examples() {
        // The customer-apps repo has a shared root workflow; per-app
        // workflow files would conflict. The shared template renderer
        // filters `.example` workflows out — re-asserted here so a
        // regression in the renderer is visible from the scaffold's
        // side too.
        let files =
            scaffold_files(&fake_app("X", "x"), &fake_org("o"), "vite").expect("vite scaffold");
        for (p, _) in &files {
            assert!(!p.starts_with(".github/"), "scaffold leaked workflow: {p}",);
        }
    }
}
