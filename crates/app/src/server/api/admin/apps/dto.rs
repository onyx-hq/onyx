//! Request/response DTOs for the customer-apps admin endpoints.
//!
//! Serde types shared by `handlers.rs`; the internal helpers that build and
//! consume them live in `ops.rs`.

use axum::Json;
use axum::http::StatusCode;
use entity::apps;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::server::api::custom_apps_source::SourceSpec;

use oxy_shared::utils::custom_app_url::build_pretty_url;

/// Standard JSON error body for 4xx/5xx responses. The frontend reads
/// `err.response.data.message` for actionable messaging in the create
/// dialog, so every fail path here surfaces a `message` field rather
/// than relying on the status code alone.
#[derive(Serialize, Debug)]
pub struct ErrorBody {
    pub message: String,
}

/// Tuple form axum recognises as a response: `(StatusCode, Json<body>)`.
/// Use this for all 4xx returns from the apps admin handlers.
pub type ApiErr = (StatusCode, Json<ErrorBody>);

#[derive(Deserialize, Debug)]
pub struct CreateAppRequest {
    pub name: String,
    /// Owning org. The admin UI's org picker resolves the org by name and
    /// supplies the uuid directly — no slug lookup required.
    pub org_id: Uuid,
    pub project_id: Uuid,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Optional URL slug override. If absent, derived from `name` and
    /// de-duplicated within the org by appending `-2`, `-3`, … on collision.
    /// Must match the same shape as auto-derived slugs when provided.
    #[serde(default)]
    pub slug: Option<String>,
    /// Where the app's bundle comes from. Default to s3 so older clients
    /// that omit this field keep behaving exactly as before.
    #[serde(default = "default_source")]
    pub source: SourceSpec,
    /// When true and `source` is `s3`, open a PR on
    /// `OXY_CUSTOMER_APPS_REPO` scaffolding the apps/<org>/<slug>/ folder
    /// before returning. PR URL ends up on `bootstrap_pr_url`.
    #[serde(default)]
    pub scaffold_pr: bool,
    /// When true and `source` is `local` with an empty `path`, oxy
    /// creates `$OXY_STATE_DIR/customer-apps/<uuid>/source/` itself
    /// and pre-populates `source_config.path` with that path. Lets
    /// the Create-new-app dialog hand the engineer a ready-made
    /// folder without making them think about the filesystem layout.
    ///
    /// Rejected when `OXY_STATE_DIR` is unset or when the deployment
    /// is in cloud mode (no engineer-reachable filesystem to write
    /// to).
    #[serde(default)]
    pub provision_local_source: bool,
    /// Curated template id to scaffold from. Defaults to `"vite"` when
    /// absent (back-compat). Validated against the registry; unknown
    /// ids return 400 before any row is inserted.
    #[serde(default)]
    pub template_id: Option<String>,
    /// Stable bundle identifier — the `<repo-org>/<repo-slug>` path
    /// under the customer-apps git repo where this bundle's source
    /// lives. Drives the S3 key
    /// (`customer-apps/<repo_path>/{draft,published}/...`) so the
    /// bundle has the same storage path across every environment.
    ///
    /// Only meaningful for `source: s3`. Defaults to `<org_slug>/<slug>`
    /// when absent — covers the common case where the operator's
    /// admin-row identity matches the repo layout. Operators with
    /// per-env slug drift type this field explicitly so dev and prod
    /// stay aligned.
    #[serde(default)]
    pub repo_path: Option<String>,
}

fn default_source() -> SourceSpec {
    SourceSpec::S3
}

fn default_branch() -> String {
    "main".to_string()
}

#[derive(Serialize, Clone, Debug)]
pub struct AppResponse {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub org_id: Uuid,
    /// Denormalised on the response so the frontend doesn't have to parse
    /// the URL to build a sync path. Source of truth is the orgs table.
    pub org_slug: String,
    pub project_id: Uuid,
    pub branch: String,
    pub source_repo: String,
    pub status: String,
    /// Canonical pretty URL `<base>/customer-apps/<org_slug>/<app_slug>/`.
    /// Always set; works for every source_type.
    pub url: String,
    /// Subdomain URL for v0 sources, e.g.
    /// `https://mars--command-center.customer-apps-dev.oxygen-hq.com/`.
    ///
    /// There is **no** env var for this. The zone is auto-derived from
    /// `OXY_API_URL` by `custom_apps_host_dispatch::custom_apps_zone`,
    /// mapping the admin host's `app{-env}` first label to
    /// `customer-apps{-env}`. `None` when `OXY_API_URL` is unset/malformed,
    /// the admin host has no `.` (e.g. `localhost`), or its first label
    /// doesn't start with `app` (custom-branded host).
    ///
    /// `None` otherwise — the admin UI shows whichever URLs are present
    /// and hides the row when both are unavailable for the current source.
    pub url_subdomain: Option<String>,
    pub source_type: String,
    pub source_config: serde_json::Value,
    /// Set after a successful PR scaffold; null otherwise.
    pub bootstrap_pr_url: Option<String>,
    pub last_synced_at: Option<String>,
    /// Set by `POST /api/admin/apps/{id}/publish`. NULL = draft.
    /// Customers (non-app-admins) only see / can reach an app when this
    /// is set; app admins always see.
    pub published_at: Option<String>,
    /// Stable bundle identifier in the customer-apps git repo
    /// (`<repo-org>/<repo-slug>`). Drives the S3 key. NULL on
    /// non-S3 sources; on S3 sources, defaults to the row's
    /// `<org_slug>/<slug>` when not explicitly overridden.
    pub repo_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// `MAX(custom_app_view_event.viewed_at)` for this app, or `None`
    /// when nobody has opened the app yet. Drives the list-level
    /// "last active" column on the Custom apps admin page so operators
    /// can sort by "stale apps". Populated by `list_apps` via a single
    /// batched query — `from_model_with_org` leaves it `None` because
    /// per-row queries would be N+1 on the list page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<String>,
    /// Email of whoever last promoted a build for this app (published /
    /// made-live / rolled back). Resolved by `list_apps` in one batched
    /// query; `None` on the cheap single responses (the detail view reads
    /// richer per-build attribution from `/builds`). Drives the list's
    /// "promoted by" line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_promoted_by_email: Option<String>,
    /// When that last promotion happened. Taken straight from the model
    /// column on every response, so the UI can show "promoted 2d ago".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_promoted_at: Option<String>,
    /// Manifest-derived app glyph URL (`<url><manifest.icon>`), or `None` when
    /// the app declares no `icon`. Same source + shape the homepage launcher
    /// uses (there is no favicon.ico probe); the frontend renders it with a
    /// monogram fallback via `AppMark`. Populated by the list/get handlers via
    /// the shared resolver. See the `oxy-app-visual-identity` skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    /// Manifest-derived preview-image URL (`<url><manifest.art>`), or `None`.
    /// Rendered with a letter-tile fallback via `AppArt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_url: Option<String>,
    /// True when the build this app is currently serving records no usable
    /// git source (missing repo **or** missing commit — either one alone
    /// can't get you to the code). Drives the "source not recorded" warning
    /// in the admin list, which is how an operator spots an app nobody can
    /// maintain *before* the person who wrote it moves on.
    ///
    /// Populated by `list_apps` via one batched query; left `false` on the
    /// cheap single responses. `false` on an app with no build at all —
    /// nothing is deployed, so nothing is orphaned yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_unrecorded: bool,
    /// Soft warnings the UI should surface to the operator. Populated
    /// by `create_app` + `update_app` after side-effect validation
    /// (e.g. "no index.html at the configured local path"). The row
    /// itself was still persisted — these are hints, not errors. List
    /// + Get endpoints leave this empty to keep them cheap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl AppResponse {
    pub(super) fn from_model_with_org(m: apps::Model, org_slug: &str) -> Self {
        let url = build_pretty_url(org_slug, &m.slug);
        // Subdomain URL applies to every source type that gets served
        // through the customer-apps surface — both v0 (reverse-proxied
        // to Vercel) and s3 (served from this oxy backend's bundle
        // cache). The host dispatcher's `already_canonicalized` guard
        // (#2466) made S3 apps work on the subdomain too, but this
        // function still had the original v0-only gate from when the
        // feature first shipped — so the admin UI surfaced subdomain
        // URLs only for v0 apps and operators wondered why their
        // `oxy publish`-deployed apps showed just the subpath. Drop
        // the gate; `subdomain_url_for` returns None when the
        // cluster's admin host doesn't fit the auto-derivation
        // convention (local dev / custom-branded host), which is the
        // only case where the row should be hidden.
        let url_subdomain =
            oxy_app_core::custom_apps_host_dispatch::subdomain_url_for(org_slug, &m.slug);
        Self {
            id: m.id,
            slug: m.slug,
            name: m.name,
            org_id: m.org_id,
            org_slug: org_slug.to_string(),
            project_id: m.project_id,
            branch: m.branch,
            source_repo: m.source_repo,
            status: m.status,
            url,
            url_subdomain,
            source_type: m.source_type,
            source_config: m.source_config,
            bootstrap_pr_url: m.bootstrap_pr_url,
            last_synced_at: m.last_synced_at.map(|d| d.to_rfc3339()),
            published_at: m.published_at.map(|d| d.to_rfc3339()),
            repo_path: m.repo_path,
            created_at: m.created_at.to_rfc3339(),
            updated_at: m.updated_at.to_rfc3339(),
            last_active_at: None,
            last_promoted_by_email: None,
            last_promoted_at: m.last_promoted_at.map(|d| d.to_rfc3339()),
            // Manifest-derived; left None on the cheap constructor and filled by
            // the list/get handlers (which have `db`) via the batched resolver
            // (`icon_art_by_app` / `resolve_manifests_batch`).
            icon_url: None,
            art_url: None,
            source_unrecorded: false,
            warnings: Vec::new(),
        }
    }
}

#[derive(Serialize)]
pub struct BuildConfigResponse {
    pub project_id: Uuid,
    pub branch: String,
    /// Org slug the app is registered under. Echoed back so the
    /// customer-apps `just build` recipe can construct the exact
    /// `OXY_APP_BASE_PATH=/customer-apps/<org>/<slug>/` value without
    /// having to ask the operator — the org might differ from the
    /// folder name (a bundle in `apps/pokehouse/franchise-report/`
    /// can be linked under `test` for local smoke-testing, and the
    /// build still needs to bake the linked org).
    pub org_slug: String,
    /// App slug as registered. Mirrors the URL slug exactly; useful
    /// for the same OXY_APP_BASE_PATH derivation and as a sanity
    /// check against the bundle's own oxy-app.json.
    pub app_slug: String,
}

#[derive(Serialize)]
pub struct OrgForProjectResponse {
    pub project_id: Uuid,
    pub org_slug: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct ListAppsQuery {
    #[serde(default = "default_limit")]
    pub limit: u64,
    #[serde(default)]
    pub offset: u64,
}

/// Page size for the admin app list. Tuned for "recently active is
/// usually what you want" — 50 covers a working session without
/// requiring scroll for typical org sizes, and keeps the first
/// payload small enough to render quickly even with a few hundred
/// apps in the DB.
fn default_limit() -> u64 {
    50
}

#[derive(Serialize)]
pub struct ListAppsResponse {
    pub items: Vec<AppResponse>,
    /// Offset for the next page. `None` when this response returned
    /// fewer items than `limit` (= we're at the tail). Lets the
    /// frontend's infinite query stop fetching without a separate
    /// `total` round trip.
    pub next_offset: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub struct UpdateAppRequest {
    pub name: Option<String>,
    pub slug: Option<String>,
    pub project_id: Option<Uuid>,
    pub branch: Option<String>,
    pub status: Option<String>,
    /// Repoint the bundle source. Most useful for LocalFolder paths
    /// (e.g. fixing a wrong-folder mistake) and for moving an app
    /// between v0 / local / s3 without delete+recreate.
    pub source: Option<SourceSpec>,
}

/// Response for a manual function-job trigger: the seeded run to watch.
#[derive(Debug, Serialize)]
pub struct RunFunctionJobResponse {
    pub run_id: String,
}

/// One row of an app's build history (newest first), with flags marking
/// which build each channel currently points at.
#[derive(Debug, Serialize)]
pub struct BuildSummary {
    /// `app_builds.id` — pass this to rollback.
    pub id: Uuid,
    /// Engineer-facing version string (git sha / CI run id).
    pub build_id: String,
    pub created_at: String,
    pub is_draft: bool,
    pub is_published: bool,
    /// Email of the app-admin who ran the publish. `None` for builds
    /// created before the `published_by` column existed.
    pub published_by_email: Option<String>,
    /// Git provenance captured by `oxyc publish` (all `None` for legacy /
    /// non-git builds). `source_repo` is the raw remote URL; the frontend
    /// normalizes it to a GitHub link against `commit_sha`.
    pub source_repo: Option<String>,
    pub commit_sha: Option<String>,
    pub source_branch: Option<String>,
}

/// `GET /{id}/builds` response: the build history plus who last promoted a
/// build to live (distinct from each build's original publisher).
#[derive(Debug, Serialize)]
pub struct BuildHistoryResponse {
    pub builds: Vec<BuildSummary>,
    pub promoted_by_email: Option<String>,
    pub promoted_at: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RollbackRequest {
    /// `app_builds.id` (from `GET .../builds`) to make live.
    pub build_id: Uuid,
}

/// Request body for every batch endpoint: the app ids to act on.
#[derive(Debug, Deserialize)]
pub struct BatchIdsRequest {
    pub ids: Vec<Uuid>,
}

/// One app's outcome in a batch response. `ok = false` carries a short reason
/// (e.g. "App not found.") so the UI can name which apps failed.
#[derive(Debug, Serialize)]
pub struct BatchItemResult {
    pub id: Uuid,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BatchItemResult {
    pub(super) fn ok(id: Uuid) -> Self {
        Self {
            id,
            ok: true,
            error: None,
        }
    }

    pub(super) fn failed(id: Uuid, message: String) -> Self {
        Self {
            id,
            ok: false,
            error: Some(message),
        }
    }
}

/// Aggregate result of a batch mutation. The request is 200 whenever it is
/// well-formed — individual failures live in `results`, not the status code —
/// so the UI can report "published 4, 1 failed" from a single response.
#[derive(Debug, Serialize)]
pub struct BatchResponse {
    pub succeeded: usize,
    pub failed: usize,
    pub results: Vec<BatchItemResult>,
}

impl BatchResponse {
    pub(super) fn from_results(results: Vec<BatchItemResult>) -> Self {
        let succeeded = results.iter().filter(|r| r.ok).count();
        Self {
            failed: results.len() - succeeded,
            succeeded,
            results,
        }
    }
}
