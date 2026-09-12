//! `POST /api/customer-apps/publish` — the one-way publish entry point.
//!
//! CI (or a local `oxy publish`) uploads a gzipped tar of the built
//! bundle. The service validates it, stores each file in S3 under a
//! per-build prefix ([`super::custom_apps_build_store`]), records an
//! `app_builds` row, upserts the `apps` row (creating it on first
//! publish), and points the draft channel (and published, with
//! `--promote`) at the new build. Replaces the old
//! `ensure` + `aws s3 sync` + callback-`/sync` dance.
//!
//! Gating: this route is deliberately **not** app-admin-gated — partners and CI
//! publish through it — so [`authorize_publish`] IS the whole decision (never
//! assume the caller is trusted staff). It resolves the caller to a
//! [`custom_apps_publish_authz::PublishActor`] and defers to the pure, tested
//! `publish_decision`: staff may publish unless the workspace locked Oxy out
//! (`workspace_oxy_lockdown`); an org **Admin+** may publish their own app; a
//! **partner** needs all three gates (assigned + `manage_apps` + client consent);
//! a plain Member and an Outsider are **denied**. We also validate that the target
//! project belongs to the named org to catch fat-finger cross-org publishes.

use std::io::Read;

use axum::Json;
use axum::extract::Multipart;
use axum::http::StatusCode;
use chrono::Utc;
use entity::{app_builds, app_functions, apps, organizations, workspaces};
use flate2::read::GzDecoder;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait,
    QueryFilter, QueryOrder,
};
use serde::Serialize;
use tar::Archive;
use uuid::Uuid;

use super::{
    custom_apps_asset_manifest as asset_manifest, custom_apps_auth,
    custom_apps_build_store as store, custom_apps_bundle_cache as cache,
    custom_apps_migrations as migrations, custom_apps_precompress as precompress,
};

/// How many builds to retain per app. Older builds (not currently pointed
/// at by either channel) are GC'd from the DB and S3 after each publish.
const KEEP_BUILDS: usize = 10;

pub struct PublishInput {
    /// Org identity — accepts either a slug (`"acme"`) or a UUID
    /// (`"550e8400-e29b-41d4-a716-446655440000"`). UUIDs are useful
    /// when the slug has drifted between envs (e.g. an admin renamed
    /// the org in prod but not staging) and the publisher wants a
    /// stable handle. `resolve_org` looks at both columns. `None` means the
    /// publisher pinned only a workspace (`--project`) — the org is inferred
    /// from it, since a workspace belongs to exactly one org.
    pub org_ref: Option<OrgRef>,
    pub app_slug: String,
    pub project_id: Uuid,
    pub branch: Option<String>,
    pub build_id: String,
    pub name: Option<String>,
    pub promote: bool,
    pub tarball: Vec<u8>,
    pub manifest: Option<serde_json::Value>,
    /// Git remote URL of the app source at publish time (best-effort).
    pub source_repo: Option<String>,
    /// Commit sha the build was published from (best-effort).
    pub commit_sha: Option<String>,
    /// Authenticated publisher (app-admin). Recorded on the build for the
    /// "who deployed" audit in the admin UI.
    pub published_by: Option<Uuid>,
    /// Email of the publisher — needed to resolve partner / staff authority for
    /// the third-party publish path (a partner uploading into a client).
    pub published_by_email: Option<String>,
    /// Set iff the request authenticated via an **app-scoped** publish token —
    /// OIDC-minted (no human) or partner-minted (a real `created_by`, design §7).
    /// Either way the `app_id` confines the token: authorization is strictly
    /// "this token's app == the target app AND the client consents", and it can
    /// publish to that one app and nowhere else — not the user's broader gates.
    pub machine_app_id: Option<Uuid>,
}

/// How the publisher referred to the target org. Accepting both lets
/// CLI users pass whichever they have at hand: `--org acme` (slug,
/// stable for humans) or `--org 550e8400-...` (UUID, stable across
/// rename/env-drift). The server tries them in order.
#[derive(Debug)]
pub enum OrgRef {
    /// Looked up against `organizations.slug`.
    Slug(String),
    /// Looked up against `organizations.id`. Skips a slug round-trip
    /// when the publisher already knows the row id.
    Id(Uuid),
}

impl OrgRef {
    /// Auto-detect: if the input parses as a UUID, treat as `Id`;
    /// otherwise treat as `Slug`. Lets a single `--org <value>` CLI
    /// arg accept either form without forcing the user to pick a
    /// different flag.
    pub fn from_str_auto(s: &str) -> Self {
        match Uuid::parse_str(s) {
            Ok(id) => OrgRef::Id(id),
            Err(_) => OrgRef::Slug(s.to_string()),
        }
    }

    fn describe(&self) -> String {
        match self {
            OrgRef::Slug(s) => s.clone(),
            OrgRef::Id(id) => id.to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PublishResult {
    pub app_id: Uuid,
    pub build_id: String,
    pub url: String,
    pub channel: String,
    /// Canonical org slug the app row landed on, after the server
    /// resolved whatever `OrgRef` the publisher sent. Echoing the
    /// server's view lets the CLI render `Registered new app
    /// acme/store-pulse` even when the engineer passed a UUID (which
    /// would otherwise echo back as `550e8400-…/store-pulse` —
    /// technically correct but jarring).
    pub org_slug: String,
    /// `true` when this publish created the app row, `false` when it
    /// updated an existing row. Lets the CLI tell the engineer whether
    /// they just registered a brand-new app or shipped a new version
    /// of one that was already in the system — surfaces accidental
    /// re-registration vs. intentional re-publish.
    pub is_new_app: bool,
    /// Non-fatal problems the operator should see even though the publish
    /// succeeded — e.g. a function shipped but its cron schedule failed to
    /// register, so it silently won't fire. Omitted from the JSON when empty, so
    /// this is an additive wire change: existing clients that don't read it are
    /// unaffected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("organization {0:?} not found")]
    UnknownOrg(String),
    #[error("project {0} is not part of org {1:?}")]
    UnknownProject(Uuid, String),
    #[error(
        "org {org:?} has not granted Oxy access for workspace {project} — an org owner must enable it (workspace settings → Oxy access)"
    )]
    OxyAccessDenied { org: String, project: Uuid },
    #[error("invalid bundle: {0}")]
    BadTarball(String),
    /// A build with this `build_id` already exists for the app. Surfaced as 409
    /// — see the check in [`publish`] for why this cannot be allowed to
    /// overwrite.
    #[error(
        "build {build_id:?} already exists for app {app_slug:?} — a build id must be unique per publish because its stored bytes are immutable and cached by id. Pass a different --build-id."
    )]
    DuplicateBuild { app_slug: String, build_id: String },
    /// The app already exists in a different workspace than this publish
    /// names. Surfaced as 409 — see `ensure_same_workspace` for why a
    /// publish, draft or not, never moves an app.
    #[error(
        "app {app_slug:?} belongs to workspace {existing_project}, but this publish targets workspace {requested_project}. A publish never moves an app to another workspace: publish with --project {existing_project}. To move the app, an app admin changes its workspace first (PATCH /api/customer-apps/{app_id} with {{\"project_id\": \"{requested_project}\"}}, e.g. `oxyc api -X PATCH /api/customer-apps/{app_id} -f project_id={requested_project}`), then publishes again."
    )]
    ProjectMismatch {
        app_slug: String,
        app_id: Uuid,
        existing_project: Uuid,
        requested_project: Uuid,
    },
    /// The `build_id` cannot safely become a store key / path segment.
    /// Surfaced as 422 — see `custom_apps_build_store::is_valid_build_id`.
    #[error(
        "invalid build id {0:?} — use only letters, digits, `.`, `_` and `-` (no leading dot, max 200 chars)"
    )]
    InvalidBuildId(String),
    /// The `app_slug` cannot safely become a schema/role name, a `repo_path`
    /// segment, or the served `/customer-apps/{org}/{slug}/` base path. Surfaced
    /// as 422 — see `admin::apps::is_valid_slug`. Crucially, an `_` in a slug
    /// would alias a hyphenated sibling onto one OLTP writer (see
    /// `oxy_oltp::schema::app_writer_name`), which `is_valid_slug` forbids.
    #[error(
        "invalid app slug {0:?} — use 1–63 lowercase letters, digits and single hyphens (no leading/trailing/double hyphen, no underscore)"
    )]
    InvalidSlug(String),
    /// A fast bundle-validation check failed (design doc §8, gate 1). Carries an
    /// actionable check/message/remediation; surfaced as 422.
    #[error("{0}")]
    Invalid(crate::server::api::custom_apps_validate::BundleValidation),
    /// A declared schema migration could not be shipped. Status is delegated to
    /// the error itself (`is_author_fault` / `is_retryable`): an edited
    /// migration is a 422 the publisher must fix, a contended apply is a 409
    /// worth retrying, and our own database trouble is a 500.
    #[error("{0}")]
    Migration(migrations::MigrationError),
    #[error("database error: {0}")]
    Db(String),
    #[error("storage error: {0}")]
    S3(String),
}

impl From<migrations::MigrationError> for PublishError {
    fn from(e: migrations::MigrationError) -> Self {
        PublishError::Migration(e)
    }
}

impl From<store::BuildStoreError> for PublishError {
    fn from(e: store::BuildStoreError) -> Self {
        PublishError::S3(e.to_string())
    }
}

impl PublishError {
    fn status(&self) -> StatusCode {
        match self {
            PublishError::UnknownOrg(_) | PublishError::UnknownProject(..) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            PublishError::OxyAccessDenied { .. } => StatusCode::FORBIDDEN,
            PublishError::BadTarball(_) | PublishError::Invalid(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            PublishError::DuplicateBuild { .. } | PublishError::ProjectMismatch { .. } => {
                StatusCode::CONFLICT
            }
            PublishError::InvalidBuildId(_) | PublishError::InvalidSlug(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            PublishError::Migration(e) => {
                if e.is_retryable() {
                    StatusCode::CONFLICT
                } else if e.is_author_fault() {
                    StatusCode::UNPROCESSABLE_ENTITY
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
            PublishError::Db(_) | PublishError::S3(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A tar entry path is safe iff it's relative and contains no `..`
/// component — guards against an archive member escaping the build
/// prefix (`../../etc/...`) when its files are later keyed into S3.
fn is_safe_relative_path(path: &std::path::Path) -> bool {
    !path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Decompression ceilings. The request body is capped at 64 MiB **compressed**
/// (`router/global.rs`), but gzip ratios turn that into tens of GB decompressed,
/// so without these a single publish — reachable by any org Admin or partner
/// since publish stopped being staff-only — can OOM the serve process and take
/// down every tenant on the replica. Generous for a real JS/CSS/asset bundle.
const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024; // total across the bundle
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024; // any single file
const MAX_FILES: usize = 20_000;

/// Parse the bundle's `oxy-app.json` if it ships one. A present-but-unparseable
/// manifest is a hard error rather than a silent `None`: it is the canonical
/// manifest `oxy publish` uploads, so dropping it strips the whole `functions`
/// block and publishes a function-less app. Absent → `Ok(None)` (the caller
/// falls back to the explicit multipart `manifest` field).
fn parse_embedded_manifest(
    files: &[(String, Vec<u8>)],
) -> Result<Option<serde_json::Value>, PublishError> {
    files
        .iter()
        .find(|(p, _)| p == "oxy-app.json")
        .map(|(_, b)| {
            serde_json::from_slice::<serde_json::Value>(b).map_err(|e| {
                PublishError::BadTarball(format!(
                    "oxy-app.json in the bundle is not valid JSON: {e}"
                ))
            })
        })
        .transpose()
}

/// Decompress a gzipped tar into `(relative_path, bytes)` pairs, rejecting
/// absolute paths and `..` traversal. Directories are skipped. Bounded in total
/// bytes, per-entry bytes, and file count so a decompression bomb can't OOM the
/// process (see the limit constants above).
pub fn unpack_tar_gz(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, PublishError> {
    unpack_tar_gz_bounded(bytes, MAX_DECOMPRESSED_BYTES, MAX_ENTRY_BYTES, MAX_FILES)
}

/// The enforcement core, parameterized on its limits so the bomb-rejection paths
/// are testable without allocating the production-sized ceilings.
fn unpack_tar_gz_bounded(
    bytes: &[u8],
    max_total: u64,
    max_entry: u64,
    max_files: usize,
) -> Result<Vec<(String, Vec<u8>)>, PublishError> {
    let mut archive = Archive::new(GzDecoder::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| PublishError::BadTarball(e.to_string()))?;
    let mut out = Vec::new();
    let mut total_bytes: u64 = 0;
    for entry in entries {
        let entry = entry.map_err(|e| PublishError::BadTarball(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| PublishError::BadTarball(e.to_string()))?;
        if !is_safe_relative_path(&path) {
            return Err(PublishError::BadTarball(format!(
                "unsafe path in tarball: {}",
                path.display()
            )));
        }
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let rel = path.to_string_lossy().trim_start_matches("./").to_string();
        if rel.is_empty() {
            continue;
        }
        if out.len() >= max_files {
            return Err(PublishError::BadTarball(format!(
                "bundle exceeds the {max_files}-file limit"
            )));
        }
        // `.take(cap + 1)` bounds the read itself — a header can lie about size,
        // so we never trust it and never let one entry allocate unbounded.
        let mut buf = Vec::new();
        entry
            .take(max_entry + 1)
            .read_to_end(&mut buf)
            .map_err(|e| PublishError::BadTarball(e.to_string()))?;
        if buf.len() as u64 > max_entry {
            return Err(PublishError::BadTarball(format!(
                "file '{rel}' exceeds the per-file {max_entry}-byte limit"
            )));
        }
        total_bytes = total_bytes.saturating_add(buf.len() as u64);
        if total_bytes > max_total {
            return Err(PublishError::BadTarball(format!(
                "bundle exceeds the {max_total}-byte decompressed limit"
            )));
        }
        out.push((rel, buf));
    }
    if !out.iter().any(|(p, _)| p == "index.html") {
        return Err(PublishError::BadTarball(
            "bundle has no index.html at its root".to_string(),
        ));
    }
    Ok(out)
}

async fn resolve_org(
    db: &DatabaseConnection,
    org_ref: &OrgRef,
) -> Result<organizations::Model, PublishError> {
    let row = match org_ref {
        OrgRef::Slug(slug) => {
            organizations::Entity::find()
                .filter(organizations::Column::Slug.eq(slug))
                .one(db)
                .await
        }
        OrgRef::Id(id) => organizations::Entity::find_by_id(*id).one(db).await,
    };
    row.map_err(|e| PublishError::Db(e.to_string()))?
        .ok_or_else(|| PublishError::UnknownOrg(org_ref.describe()))
}

/// Infer the org from the target workspace when the publisher didn't send one
/// (`--project` without `--org`). A workspace belongs to exactly one org, so
/// the project id alone determines it. Errors if the workspace is unknown or
/// somehow orphaned (no `org_id`).
async fn org_for_project(
    db: &DatabaseConnection,
    project_id: Uuid,
) -> Result<organizations::Model, PublishError> {
    let ws = workspaces::Entity::find_by_id(project_id)
        .one(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?
        .ok_or_else(|| PublishError::UnknownProject(project_id, "(inferred org)".to_string()))?;
    let org_id = ws.org_id.ok_or_else(|| {
        PublishError::UnknownProject(project_id, "(orphaned workspace)".to_string())
    })?;
    resolve_org(db, &OrgRef::Id(org_id)).await
}

/// Best-effort cross-org guard: if the project id resolves to a workspace,
/// its org must match. Unknown ids are rejected so a typo can't silently
/// create a row pointing at nothing.
async fn validate_project(
    db: &DatabaseConnection,
    project_id: Uuid,
    org_id: Uuid,
    org_slug: &str,
) -> Result<(), PublishError> {
    let ws = workspaces::Entity::find_by_id(project_id)
        .one(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?;
    match ws {
        Some(w) if w.org_id == Some(org_id) => Ok(()),
        _ => Err(PublishError::UnknownProject(
            project_id,
            org_slug.to_string(),
        )),
    }
}

/// The publish authorization gate. This route is **not** staff-gated at the router
/// (partners and CI publish through it), so this function is the whole decision —
/// it must never fail open. It resolves the caller to a
/// [`custom_apps_publish_authz::PublishActor`] and defers to the pure, tested
/// decision: an outsider or a plain Member is denied, an org **Admin+** may publish
/// their own app, a **partner** needs all three gates, and **staff** may publish
/// unless the workspace has locked Oxy out (`workspace_oxy_lockdown`). App-scoped
/// machine tokens take the confined `app_id`-match path above, before any of this.
async fn authorize_publish(
    db: &DatabaseConnection,
    org: &organizations::Model,
    input: &PublishInput,
) -> Result<(), PublishError> {
    // OIDC-minted machine token: authorize by "this token's app is the target app
    // AND the client consents". No user gates — the publisher registration the
    // exchange verified IS the assignment, and consent is re-checked here so a
    // revoke denies the next publish.
    if let Some(machine_app_id) = input.machine_app_id {
        let target =
            find_app(db, org.id, &input.app_slug)
                .await?
                .ok_or(PublishError::OxyAccessDenied {
                    org: org.slug.clone(),
                    project: input.project_id,
                })?;
        let consent =
            crate::server::api::custom_apps_publish_authz::consent_enabled(db, org.id).await;
        if target.id == machine_app_id && consent {
            return Ok(());
        }
        return Err(PublishError::OxyAccessDenied {
            org: org.slug.clone(),
            project: input.project_id,
        });
    }

    // Every non-machine publish routes through the ONE pure, tested decision
    // (`publish_decision`): it denies an outsider and a plain Member, allows an org
    // Admin+ and a partner with all three gates, and lets staff publish unless the
    // workspace locked Oxy out. Conflating "staff" with "not a member" here is
    // exactly how an outsider could publish into another tenant — so resolve the
    // actor once and let the policy decide, never a bare `!locked ⇒ Ok`.
    use crate::server::api::custom_apps_publish_authz as authz;
    let deny = || PublishError::OxyAccessDenied {
        org: org.slug.clone(),
        project: input.project_id,
    };
    let (Some(uid), Some(email)) = (input.published_by, input.published_by_email.as_deref()) else {
        return Err(deny());
    };
    let actor = authz::resolve_actor(db, uid, email, org.id).await;

    // Read each side-fact only for the actor it applies to: the lockdown for staff,
    // consent for a partner. Everyone else pays no extra query.
    let staff_locked_out = if matches!(actor, authz::PublishActor::Staff) {
        custom_apps_auth::is_oxy_locked_down(db, input.project_id)
            .await
            .map_err(|e| PublishError::Db(e.to_string()))?
    } else {
        false
    };
    let consent = matches!(actor, authz::PublishActor::Partner { .. })
        && authz::consent_enabled(db, org.id).await;

    authz::publish_decision(&actor, staff_locked_out, consent).map_err(|_| deny())
}

async fn find_app(
    db: &DatabaseConnection,
    org_id: Uuid,
    slug: &str,
) -> Result<Option<apps::Model>, PublishError> {
    apps::Entity::find()
        .filter(apps::Column::OrgId.eq(org_id))
        .filter(apps::Column::Slug.eq(slug))
        .one(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))
}

/// Delete the `app_builds` row a failed publish left behind, loudly.
///
/// Both callers are rollback paths that already know something went wrong, and
/// the failure here is usually the same transient DB problem that triggered the
/// rollback. That matters more than it used to: while the row survives, its
/// `build_id` is *taken*, so the operator's natural next move — retry the same
/// publish — now 409s where before it just worked. `gc_builds` reaps the row
/// eventually, but only on a later *successful* publish, which is exactly what
/// the operator can't do yet.
///
/// So the errors are still swallowed (a rollback must not mask the original
/// error with its own), but never silently: the warning is what lets an
/// operator tell "that id is taken" from "that id is taken *because cleanup
/// failed*", which have different remedies.
async fn discard_build_row(db: &DatabaseConnection, build_pk: Uuid, build_id: &str) {
    match app_builds::Entity::find_by_id(build_pk).one(db).await {
        Ok(Some(row)) => {
            if let Err(e) = row.delete(db).await {
                tracing::warn!(
                    "publish rollback: could not delete build row {build_pk} ({build_id:?}): {e} \
                     — retrying this publish with the same --build-id will now 409"
                );
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(
            "publish rollback: could not load build row {build_pk} ({build_id:?}) to delete it: \
             {e} — retrying this publish with the same --build-id may now 409"
        ),
    }
}

/// Undo a publish whose bytes were already stored AND whose build row was already
/// recorded, but that failed while wiring the build up (function registration or
/// pointer move). Runs the full teardown in the order the invariants require —
/// drop the stored bytes (best-effort; a leaked prefix is a GC concern, not a
/// publish failure), delete the orphan build row so a same-id retry doesn't 409,
/// then undo the `apps`-row mutation so a live app is never left repointed.
///
/// Consumes `rollback` (its `apply` is single-use). Extracted so the two
/// post-`record_build` failure sites share one sequence and cannot drift apart —
/// they were byte-identical copies.
async fn rollback_stored_build(
    db: &DatabaseConnection,
    app_id: Uuid,
    build_id: &str,
    build_pk: Uuid,
    rollback: AppMutationRollback,
) {
    if let Err(cleanup) = store::delete_build(app_id, build_id).await {
        tracing::warn!("publish rollback: orphan prefix left for {app_id}: {cleanup}");
    }
    discard_build_row(db, build_pk, build_id).await;
    rollback.apply(db, app_id).await;
}

/// True when this app already has a build under `build_id`.
///
/// There is no unique index on `(app_id, build_id)` to lean on, so this is the
/// enforcement point for the uniqueness the storage layer and the bundle cache
/// both assume. It races against a concurrent publish of the same id — two
/// publishes could both read `false` — but that window is orders of magnitude
/// narrower than the "CI re-run three days later" case this closes, and the
/// honest fix for the rest is a DB constraint, not a lock here.
async fn build_id_taken(
    db: &DatabaseConnection,
    app_id: Uuid,
    build_id: &str,
) -> Result<bool, PublishError> {
    app_builds::Entity::find()
        .filter(app_builds::Column::AppId.eq(app_id))
        .filter(app_builds::Column::BuildId.eq(build_id))
        .one(db)
        .await
        .map(|row| row.is_some())
        .map_err(|e| PublishError::Db(e.to_string()))
}

/// Insert (first publish) or update the `apps` row. Returns the app id
/// and `is_new = true` iff this call inserted a fresh row — the CLI uses
/// that to print "Registered new app" vs "Published new version of …"
/// so engineers spot accidental re-registration and intentional updates
/// without scanning the diff.
/// How to undo the app-row mutation `upsert_app` made, if a later step fails
/// before the build is durable. The row is mutated up front (a brand-new app
/// needs its id minted, and an existing one gets the new branch/name), so a
/// failure in `put_build` must not leave a half-published row behind.
///
/// An existing app's `project_id` is written only to re-home an app whose
/// workspace is deleted, orphaned or in another org — any other publish that
/// names another workspace is refused by `ensure_same_workspace`. Restoring it
/// here undoes that re-home on a failed publish, and is otherwise a no-op kept
/// as defence in depth:
/// `window.__OXY_APP__.projectId` is read from this row, and a wrong value
/// redirects a working bundle at another workspace's data plane.
enum AppMutationRollback {
    /// Newly created — undo by deleting the row.
    Created,
    /// Pre-existing — undo by restoring the values from before this publish.
    Updated {
        project_id: Uuid,
        branch: String,
        name: String,
        last_synced_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    },
}

impl AppMutationRollback {
    async fn apply(self, db: &DatabaseConnection, app_id: Uuid) {
        match self {
            AppMutationRollback::Created => {
                if let Err(e) = apps::Entity::delete_by_id(app_id).exec(db).await {
                    tracing::warn!(
                        "publish rollback: could not delete newly-created app {app_id}: {e}"
                    );
                }
            }
            AppMutationRollback::Updated {
                project_id,
                branch,
                name,
                last_synced_at,
            } => {
                let active = apps::ActiveModel {
                    id: ActiveValue::Set(app_id),
                    project_id: ActiveValue::Set(project_id),
                    branch: ActiveValue::Set(branch),
                    name: ActiveValue::Set(name),
                    // Restore too, so a rolled-back row doesn't claim a sync that
                    // never durably happened.
                    last_synced_at: ActiveValue::Set(last_synced_at),
                    updated_at: ActiveValue::Set(Utc::now().fixed_offset()),
                    ..Default::default()
                };
                if let Err(e) = active.update(db).await {
                    tracing::warn!(
                        "publish rollback: could not restore prior state for app {app_id}: {e}"
                    );
                }
            }
        }
        // Restoring the row is only half the undo. `upsert_app` mutates it up
        // front and the success-path invalidation is at the very end of
        // `publish`, so the whole of `put_build` — seconds to minutes on a
        // real bundle — sits in between. A serve request landing in that
        // window caches the *mutated* row, and without this the cache keeps
        // serving the rolled-back values for up to `CACHE_TTL`: an `Updated`
        // app hands out a `window.__OXY_APP__.projectId` pointing at another
        // workspace while the old bytes still serve (the exact hazard this
        // type's doc comment above says it exists to prevent), and a
        // `Created` one keeps resolving after its row is deleted.
        //
        // Unconditional, and after the restore: a failed restore leaves the
        // cache just as wrong, and invalidating first would let a concurrent
        // request re-cache the bad row before the write lands.
        //
        // `crates/app/tests/custom_apps_cache_invalidation.rs` cannot catch
        // this — its detector is file-level and this file already contains a
        // call on the success path.
        //
        // This drops the whole map, not one app's entry, so every failed
        // publish costs a re-resolution storm across every app this replica
        // serves — the storm the cache exists to eliminate. Accepted for the
        // same reason renames take the wholesale drop: a minute of extra
        // queries beats a minute of a wrong `project_id`. Note the cost is
        // now reachable from a *failing* request, and a `put_build` failure
        // is guaranteed on a multi-replica deploy with no bucket configured
        // — so a publisher retrying into a bad bucket flushes the map on
        // every attempt.
        super::custom_apps_cache::invalidate_app_resolution_cache();
    }
}

/// Refuse a publish that names a different workspace than the app lives in.
///
/// `apps.project_id` is what `window.__OXY_APP__.projectId` is injected from,
/// and what the app's connectors, secrets and schedules resolve against. A
/// publish used to overwrite it — drafts included, since the draft and live
/// channels share the one row — so a draft publish with a different
/// `--project` silently moved the LIVE app onto another workspace's data.
/// Moving an app is an explicit admin action (`PATCH /api/customer-apps/{id}`),
/// never a side effect of shipping a build.
///
/// `branch` and `name` stay overwritable on publish: they label the app and
/// move no data.
///
/// The one exception: the app's workspace is not a live workspace of the app's
/// org. That covers three cases:
///
/// - **Deleted.** `apps.project_id` has no foreign key, and deleting a
///   workspace leaves `apps` alone.
/// - **Orphaned.** The workspace exists but its `org_id` is NULL.
/// - **In another org.** The admin `PATCH /api/customer-apps/{id}` wrote
///   `project_id` without checking the workspace's org until #3174, so rows
///   written before that fix can name another org's workspace.
///
/// Refusing any of these is a dead end, because the `--project` the 409 names
/// cannot repair the app. A deleted workspace is gone, and an orphaned one
/// belongs to no org. Another org's workspace fails `validate_project` when the
/// publisher names this org, and otherwise resolves to that org, where
/// `find_app` misses this app and the publish registers a second one. Moving
/// the app by PATCH is app-admin only. Returns `Ok(true)` for the re-home,
/// which the caller writes, and `Ok(false)` when the workspaces already match.
///
/// A re-home never moves an app onto a workspace outside its own org, because
/// of what `publish` resolved first: `org` contains `input.project_id`
/// (`validate_project` when the publisher named an org, `org_for_project`
/// derives the org from the workspace when it did not), and `existing` was
/// found by `find_app` under that same `org.id`.
async fn ensure_same_workspace(
    db: &DatabaseConnection,
    existing: &apps::Model,
    input: &PublishInput,
) -> Result<bool, PublishError> {
    if existing.project_id == input.project_id {
        return Ok(false);
    }
    let current = workspaces::Entity::find_by_id(existing.project_id)
        .one(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?;
    if current
        .as_ref()
        .is_none_or(|w| w.org_id != Some(existing.org_id))
    {
        return Ok(true);
    }
    Err(PublishError::ProjectMismatch {
        app_slug: existing.slug.clone(),
        app_id: existing.id,
        existing_project: existing.project_id,
        requested_project: input.project_id,
    })
}

async fn upsert_app(
    db: &DatabaseConnection,
    org: &organizations::Model,
    input: &PublishInput,
) -> Result<(Uuid, bool, AppMutationRollback), PublishError> {
    let now = Utc::now().fixed_offset();
    let existing = find_app(db, org.id, &input.app_slug).await?;
    if let Some(row) = existing {
        // Authoritative check, before the row is touched. The fast path in
        // `publish` catches this before the bundle is unpacked, but the row can
        // change between the two reads.
        let rehome = ensure_same_workspace(db, &row, input).await?;
        let id = row.id;
        let slug = row.slug.clone();
        // Snapshot the fields this update overwrites, so a later failure can
        // restore them rather than stranding the app on a half-publish.
        let prior = AppMutationRollback::Updated {
            project_id: row.project_id,
            branch: row.branch.clone(),
            name: row.name.clone(),
            last_synced_at: row.last_synced_at,
        };
        let stray_workspace = row.project_id;
        let mut active: apps::ActiveModel = row.into();
        // `project_id` is written only for a re-home: otherwise
        // `ensure_same_workspace` above has already proved it equal.
        if rehome {
            active.project_id = ActiveValue::Set(input.project_id);
        }
        if let Some(b) = &input.branch {
            active.branch = ActiveValue::Set(b.clone());
        }
        if let Some(name) = &input.name {
            active.name = ActiveValue::Set(name.clone());
        }
        active.last_synced_at = ActiveValue::Set(Some(now));
        active.updated_at = ActiveValue::Set(now);
        active
            .update(db)
            .await
            .map_err(|e| PublishError::Db(e.to_string()))?;
        if rehome {
            tracing::warn!(
                "publish re-homed app {}/{slug} ({id}) from workspace {stray_workspace} (deleted, orphaned, or in another org) to workspace {}",
                org.slug,
                input.project_id
            );
        }
        return Ok((id, false, prior));
    }

    let id = Uuid::new_v4();
    let name = input
        .name
        .clone()
        .unwrap_or_else(|| humanize_slug(&input.app_slug));
    let model = apps::ActiveModel {
        // Leave to the DB default ('org'): a new app is org-visible unless
        // explicitly restricted later.
        visibility: sea_orm::ActiveValue::NotSet,
        id: ActiveValue::Set(id),
        slug: ActiveValue::Set(input.app_slug.clone()),
        name: ActiveValue::Set(name),
        org_id: ActiveValue::Set(org.id),
        project_id: ActiveValue::Set(input.project_id),
        branch: ActiveValue::Set(input.branch.clone().unwrap_or_else(|| "main".to_string())),
        source_repo: ActiveValue::Set("oxy-hq/customer-apps".to_string()),
        status: ActiveValue::Set("created".to_string()),
        source_type: ActiveValue::Set("s3".to_string()),
        source_config: ActiveValue::Set(serde_json::json!({})),
        last_synced_at: ActiveValue::Set(Some(now)),
        manifest_override: ActiveValue::NotSet,
        bootstrap_pr_url: ActiveValue::NotSet,
        // Stays a draft until promoted (here or via /admin/apps).
        published_at: ActiveValue::NotSet,
        repo_path: ActiveValue::Set(Some(format!("{}/{}", org.slug, input.app_slug))),
        draft_build_id: ActiveValue::NotSet,
        published_build_id: ActiveValue::NotSet,
        last_promoted_by: ActiveValue::NotSet,
        last_promoted_at: ActiveValue::NotSet,
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    };
    model
        .insert(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?;
    Ok((id, true, AppMutationRollback::Created))
}

fn humanize_slug(slug: &str) -> String {
    slug.split('-')
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn record_build(
    db: &DatabaseConnection,
    app_id: Uuid,
    input: &PublishInput,
    s3_prefix: String,
    manifest_json: Option<serde_json::Value>,
) -> Result<Uuid, PublishError> {
    let build_pk = Uuid::new_v4();
    let model = app_builds::ActiveModel {
        id: ActiveValue::Set(build_pk),
        app_id: ActiveValue::Set(app_id),
        build_id: ActiveValue::Set(input.build_id.clone()),
        s3_prefix: ActiveValue::Set(s3_prefix),
        manifest_json: ActiveValue::Set(manifest_json),
        created_at: ActiveValue::Set(Utc::now().fixed_offset()),
        published_by: ActiveValue::Set(input.published_by),
        source_repo: ActiveValue::Set(input.source_repo.clone()),
        commit_sha: ActiveValue::Set(input.commit_sha.clone()),
        source_branch: ActiveValue::Set(input.branch.clone()),
        // Gate 1 (byte-level validation) already ran and passed to reach here —
        // publish 422s otherwise, before storing. Record it so promotion can be
        // gated on a persisted status. A future deploy-time probe (gate 2) may
        // downgrade this to `failed`; nothing sets `pending` yet.
        validation_status: ActiveValue::Set("passed".to_string()),
        validation_detail: ActiveValue::NotSet,
    };
    model
        .insert(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?;
    Ok(build_pk)
}

/// Extract `(name, per-function manifest JSON)` pairs from the bundle
/// manifest's `functions` block. Empty when the manifest declares none
/// (today's static-bundle default), so this is a no-op for function-less apps.
fn function_specs(manifest_json: Option<&serde_json::Value>) -> Vec<(String, serde_json::Value)> {
    manifest_json
        .and_then(|m| m.get("functions"))
        .and_then(|f| f.as_object())
        .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Whether `name` is safe as a `/fn/<name>` route key and an artifact-key path
/// segment. THE canonical function-name rule (`^[a-z][a-z0-9-]{0,63}$`): the CLI
/// (`oxy publish`), the vite-plugin build gate, and the SDK manifest loader all
/// mirror it, and `custom_apps_serve::sources::s3_object_key` relies on every
/// stored name matching it to prove a rewritten request path can never resolve
/// to a function artifact. Enforced at publish by [`record_functions`] so a
/// bundle POSTed straight to the API — bypassing the CLI — can't store a name
/// that breaks the invariant.
pub(crate) fn is_valid_function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Reject any declared function name that isn't [`is_valid_function_name`],
/// as one `PublishError` before any row is written. Split out so it is
/// unit-testable without a database.
fn validate_function_names(specs: &[(String, serde_json::Value)]) -> Result<(), PublishError> {
    for (name, _) in specs {
        if !is_valid_function_name(name) {
            return Err(PublishError::BadTarball(format!(
                "invalid function name {name:?} — must match ^[a-z][a-z0-9-]{{0,63}}$ \
                 (a lowercase letter, then up to 63 lowercase letters, digits or hyphens)"
            )));
        }
    }
    Ok(())
}

/// Build-store key of a function's bundled JS artifact, matching the layout
/// `oxy publish` uploads (`<build_prefix>functions/<name>.js`) and the
/// `app_functions.artifact_key` contract.
fn function_artifact_key(build_prefix: &str, name: &str) -> String {
    format!("{build_prefix}functions/{name}.js")
}

/// Record one `app_functions` row per declared function, keyed to this build,
/// so the `/fn/<name>` route can resolve them. Without this the bundled JS
/// ships in the build store but the runtime can never find it (→ 404).
async fn record_functions(
    db: &DatabaseConnection,
    app_id: Uuid,
    build_pk: Uuid,
    build_prefix: &str,
    specs: &[(String, serde_json::Value)],
) -> Result<(), PublishError> {
    // Names become artifact-key path segments + `/fn/<name>` route keys; a bundle
    // published straight through the API skips the CLI's check, so enforce the
    // shape here — the load-bearing spot for `sources::s3_object_key`. Reject the
    // whole publish (the caller rolls the stored build back out).
    validate_function_names(specs)?;
    for (name, spec) in specs {
        let model = app_functions::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            app_id: ActiveValue::Set(app_id),
            build_id: ActiveValue::Set(build_pk),
            name: ActiveValue::Set(name.clone()),
            manifest_json: ActiveValue::Set(Some(spec.clone())),
            artifact_key: ActiveValue::Set(function_artifact_key(build_prefix, name)),
            created_at: ActiveValue::Set(Utc::now().fixed_offset()),
        };
        model
            .insert(db)
            .await
            .map_err(|e| PublishError::Db(e.to_string()))?;
    }
    Ok(())
}

/// Register/update `agentic_schedules` for functions that declare a `schedule`
/// in their manifest, so a scheduled Oxy Function fires without a manual
/// schedule (`target_kind="function"`, `target_ref="<app_id>/<name>"`). Idempotent
/// per publish: upserts by `target_ref` so a re-publish updates cadence rather
/// than duplicating, and **reconciles** — any of this app's function schedules
/// whose function no longer declares a `schedule` (dropped, renamed, or deleted)
/// is removed, so a stale row can't keep firing `run_scheduled_function` against
/// a function that no longer exists. Best-effort — a schedule failure never
/// fails the publish (the function stays route-invocable).
async fn register_function_schedules(
    db: &DatabaseConnection,
    app_id: Uuid,
    workspace_id: Uuid,
    specs: &[(String, serde_json::Value)],
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    let existing = agentic_pipeline::scheduler::list_schedules(db, workspace_id)
        .await
        .unwrap_or_default();
    // The `target_ref`s that SHOULD be scheduled after this publish — every
    // function that currently declares a `schedule`. Drives both the upsert
    // below and the reconcile at the end.
    let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, spec) in specs {
        let Some(cron) = spec.get("schedule").and_then(|v| v.as_str()) else {
            continue;
        };
        let timezone = spec
            .get("timezone")
            .and_then(|v| v.as_str())
            .unwrap_or("UTC")
            .to_string();
        let target_ref = format!("{app_id}/{name}");
        live.insert(target_ref.clone());
        // Carry the function's retry policy on the schedule so the entity-free
        // fire arm can attach it to the queued task without reaching into the
        // manifest. Built here (host) from the same helper the manual trigger
        // uses, so the two paths can't drift.
        let variables = crate::server::api::custom_apps_functions::function_task_policy(spec)
            .and_then(|p| serde_json::to_value(&p).ok())
            .map(|p| serde_json::json!({ "task_policy": p }));
        let input = agentic_pipeline::scheduler::ScheduleInput {
            name: format!("fn:{app_id}/{name}"),
            target_kind: "function".to_string(),
            target_ref: target_ref.clone(),
            question: None,
            variables,
            cron_expr: cron.to_string(),
            timezone,
            enabled: true,
        };
        let found = existing
            .iter()
            .find(|s| s.target_kind == "function" && s.target_ref == target_ref);
        let result = match found {
            Some(s) => agentic_pipeline::scheduler::update_schedule(db, workspace_id, &s.id, input)
                .await
                .map(|_| ()),
            None => agentic_pipeline::scheduler::create_schedule(db, workspace_id, input)
                .await
                .map(|_| ()),
        };
        if let Err(e) = result {
            tracing::warn!("publish: failed to register schedule for {app_id}/{name}: {e}");
            warnings.push(format!(
                "function '{name}' was published but its schedule could not be registered ({e}); \
                 it will not fire on its cron until the next successful publish"
            ));
        }
    }
    // Reconcile: retire this app's function schedules whose function no longer
    // declares a cadence. `existing` predates the upserts above, so a
    // just-updated schedule is also in `live` and survives; only genuinely
    // orphaned rows (function dropped `schedule`, was renamed, or deleted) are
    // removed — otherwise every tick would `run_scheduled_function` a missing
    // function and log a failed run forever.
    let prefix = format!("{app_id}/");
    for s in &existing {
        if s.target_kind == "function"
            && s.target_ref.starts_with(&prefix)
            && !live.contains(&s.target_ref)
            && let Err(e) =
                agentic_pipeline::scheduler::delete_schedule(db, workspace_id, &s.id).await
        {
            tracing::warn!(
                "publish: failed to retire stale schedule {}: {e}",
                s.target_ref
            );
            warnings.push(format!(
                "a stale schedule for '{}' could not be retired ({e}); it may keep firing against \
                 a function that no longer exists",
                s.target_ref
            ));
        }
    }
    warnings
}

/// Point the channel(s) at the new build. Draft always; published +
/// `published_at` when promoting.
async fn set_pointers(
    db: &DatabaseConnection,
    app_id: Uuid,
    build_pk: Uuid,
    promote: bool,
) -> Result<(), PublishError> {
    let row = apps::Entity::find_by_id(app_id)
        .one(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?
        .ok_or_else(|| PublishError::Db(format!("app {app_id} vanished mid-publish")))?;
    let mut active: apps::ActiveModel = row.into();
    active.draft_build_id = ActiveValue::Set(Some(build_pk));
    if promote {
        // Promotion gate (validator-can't-be-bypassed): only a build whose
        // validation is recorded `passed` may go live. Redundant on this path —
        // the build was just gate-1 validated — but keeps every promotion point
        // honest, so a build that hasn't passed can never reach the live channel.
        gate_promotion(db, build_pk).await?;
        active.published_build_id = ActiveValue::Set(Some(build_pk));
        active.published_at = ActiveValue::Set(Some(Utc::now().fixed_offset()));
    }
    active
        .update(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?;
    Ok(())
}

/// Promotion gate (the enforcement half of "the validator can't be bypassed"):
/// a build may reach the published/live channel only if its recorded validation
/// status is `passed`. Gate 1 stamps `passed` at publish; a future deploy-time
/// probe (gate 2) may downgrade to `failed`. Checked at every promotion point so
/// no build can go live without a recorded pass. Today every stored build is
/// `passed`, so this is dormant — but the boundary now exists and is honest.
async fn gate_promotion(db: &DatabaseConnection, build_pk: Uuid) -> Result<(), PublishError> {
    let build = app_builds::Entity::find_by_id(build_pk)
        .one(db)
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?
        .ok_or_else(|| PublishError::Db(format!("build {build_pk} vanished mid-publish")))?;
    if build.validation_status != "passed" {
        return Err(PublishError::Invalid(
            crate::server::api::custom_apps_validate::BundleValidation::new(
                "validation_not_passed",
                format!(
                    "build validation status is '{}', not 'passed' — it cannot be promoted to live",
                    build.validation_status
                ),
                "Re-publish after the bundle passes validation.",
            ),
        ));
    }
    Ok(())
}

/// Delete builds beyond `KEEP_BUILDS`, never touching the rows the two
/// channel pointers currently reference. Best-effort on the S3 side.
async fn gc_builds(db: &DatabaseConnection, app_id: Uuid, protect: &[Uuid]) {
    // Always protect the builds the live channels point at, regardless of what
    // the caller passed. GC that reaps the currently-served build is a silent
    // outage: the app 404s (`custom_apps_serve/sources.rs`) with no publish
    // having touched the live channel. Read the pointers here so the guarantee
    // can't be lost by a caller forgetting to thread them through.
    let mut protect: Vec<Uuid> = protect.to_vec();
    match apps::Entity::find_by_id(app_id).one(db).await {
        Ok(Some(app)) => {
            protect.extend(app.published_build_id);
            protect.extend(app.draft_build_id);
        }
        Ok(None) => {
            tracing::warn!(
                "gc_builds: app {app_id} not found; skipping GC so a live build can't be reaped"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                "gc_builds: could not load channel pointers for app {app_id} ({e}); skipping GC"
            );
            return;
        }
    }
    let builds = match app_builds::Entity::find()
        .filter(app_builds::Column::AppId.eq(app_id))
        .order_by_desc(app_builds::Column::CreatedAt)
        .all(db)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("gc_builds list failed for app {app_id}: {e}");
            return;
        }
    };
    for build in builds.into_iter().skip(KEEP_BUILDS) {
        if protect.contains(&build.id) {
            continue;
        }
        if let Err(e) = store::delete_build(app_id, &build.build_id).await {
            tracing::warn!("gc_builds S3 delete failed ({}): {e}", build.build_id);
        }
        let build_label = build.build_id.clone();
        if let Err(e) = build.delete(db).await {
            tracing::warn!("gc_builds row delete failed ({build_label}): {e}");
        }
    }
}

pub async fn publish(mut input: PublishInput) -> Result<PublishResult, PublishError> {
    let db = oxy::database::client::establish_connection()
        .await
        .map_err(|e| PublishError::Db(e.to_string()))?;

    // Resolve the org. When the publisher pinned only a workspace
    // (`--project` without `--org`), infer the org from it — a workspace
    // belongs to exactly one org. When an org IS given, keep the cross-org
    // guard: it must match the workspace's org.
    let org = match &input.org_ref {
        Some(org_ref) => {
            let org = resolve_org(&db, org_ref).await?;
            validate_project(&db, input.project_id, org.id, &org.slug).await?;
            org
        }
        None => org_for_project(&db, input.project_id).await?,
    };
    authorize_publish(&db, &org, &input).await?;

    // Reject a malformed or already-used id BEFORE the expensive work. The
    // pipeline below inflates up to `MAX_DECOMPRESSED_BYTES`, validates, and
    // brotli-compresses the whole bundle; making a duplicate pay all of that
    // only to 409 is wasteful now that a reused id is an ordinary mistake
    // rather than an exotic one (a copied-out `deploy.yml` that still pins
    // `--build-id ${{ github.sha }}` hits it on every promote).
    //
    // Only possible when the app already exists — on a first publish there is
    // no `app_id` yet, and no builds to collide with either. The check after
    // `upsert_app` stays as the authoritative one; this is a fast path, not a
    // replacement, and the two cannot disagree in a way that admits a bad id.
    if !store::is_valid_build_id(&input.build_id) {
        return Err(PublishError::InvalidBuildId(input.build_id.clone()));
    }
    // The slug is a raw multipart field on this route and had NO validation —
    // it feeds the OLTP writer derivation (`app_writer_name`), a `repo_path`
    // segment and the served base path. `app_writer_name` already refuses an `_`
    // (which would alias a hyphenated sibling onto one writer) rather than pass
    // it through, but reject the whole invalid-slug set here too, so a bad slug
    // never reaches the row, the path or the store in the first place.
    if !crate::server::api::admin::apps::is_valid_slug(&input.app_slug) {
        return Err(PublishError::InvalidSlug(input.app_slug.clone()));
    }
    if let Some(app) = find_app(&db, org.id, &input.app_slug).await? {
        // Same fast-path reasoning for a publish that names another workspace:
        // refuse it before inflating the bundle. `upsert_app` re-checks, and is
        // the one that writes a re-home.
        ensure_same_workspace(&db, &app, &input).await?;
        if build_id_taken(&db, app.id, &input.build_id).await? {
            return Err(PublishError::DuplicateBuild {
                app_slug: input.app_slug.clone(),
                build_id: input.build_id.clone(),
            });
        }
    }

    // Decompress off the async runtime: even bounded at 256 MiB, the inflate +
    // allocation is CPU-bound and would tie up a Tokio worker for the duration
    // (a crafted body that expands to the full budget parks a worker). `take`
    // moves the bytes into the blocking task — `tarball` isn't used afterwards.
    let tarball = std::mem::take(&mut input.tarball);
    let files = tokio::task::spawn_blocking(move || unpack_tar_gz(&tarball))
        .await
        .map_err(|e| {
            PublishError::BadTarball(format!("bundle decompression task failed: {e}"))
        })??;
    // Fast deploy validation (design doc §8, gate 1): catch the known
    // blank-screen causes (missing head, baked-vs-registered base-path
    // mismatch) as an actionable 422 BEFORE storing the build.
    crate::server::api::custom_apps_validate::validate_bundle(&files, &org.slug, &input.app_slug)
        .map_err(PublishError::Invalid)?;
    let index_bytes = files
        .iter()
        .find(|(p, _)| p == "index.html")
        .map(|(_, b)| axum::body::Bytes::from(b.clone()));
    // Capture the bundle's oxy-app.json into the build row so the manifest
    // resolver (debug endpoint) reads it from the DB, not a local file.
    // Falls back to an explicit `manifest` multipart field if the bundle
    // didn't ship one.
    //
    // A present-but-unparseable oxy-app.json is a hard error, NOT a silent
    // fall-through: `oxy-app.json` is the canonical manifest `oxy publish` ships,
    // so swallowing its parse error strips the whole `functions` block and
    // publishes a function-less app with a 200 — then `useFunction` 404s and the
    // author is misdiagnosed. (The multipart `manifest` field is hardened the
    // same way in the handler.)
    let manifest_json = parse_embedded_manifest(&files)?.or_else(|| input.manifest.clone());

    // Lift the bundle's declared `*.sql` migrations out NOW, while `files` is
    // still in hand and before a single byte has been stored. Three things this
    // ordering buys, none of which survive doing it later:
    //
    //  - `files` is moved into `put_build` below, so the SQL has to be copied
    //    out before then. It is kilobytes.
    //  - A malformed `migrations` block, or one naming a directory the bundle
    //    doesn't carry, fails the publish with no orphan build and no row to
    //    roll back.
    //  - A **draft** publish validates the block too. The author learns the
    //    directory name is wrong on the publish that carries it, rather than on
    //    a promote weeks later.
    //
    // Empty for the overwhelmingly common app, which declares no migrations.
    let declared_migrations = migrations::declare(manifest_json.as_ref(), &files)?;

    // Reserve the bundle's `__oxy/` namespace and write the platform's asset
    // manifest into it — the document that gives the serve path its preload
    // hints and the service worker its precache list. Before pre-compression so
    // the manifest gets a `.br` sibling like any other JSON, and before
    // `put_build` so it lands in the same immutable prefix as the files it
    // describes.
    let mut files = files;
    let asset_manifest =
        asset_manifest::install_into(&mut files, &input.build_id, manifest_json.as_ref());
    // The entry bytes, kept aside so the cache can be warmed once the build is
    // live. Cloned rather than referenced because `files` is about to be moved
    // into the upload; it is a few hundred KiB and it buys the first visitor
    // after a publish a warm critical path instead of one object-store
    // round-trip per entry chunk.
    let seed_entries: Vec<(String, axum::body::Bytes)> = asset_manifest
        .entries
        .iter()
        .filter_map(|entry| {
            files
                .iter()
                .find(|(path, _)| path.trim_start_matches('/') == entry.path)
                .map(|(path, bytes)| (path.clone(), axum::body::Bytes::from(bytes.clone())))
        })
        .collect();

    // Emit `<asset>.br` siblings so the serve path never re-compresses an
    // immutable content-hashed asset (see `custom_apps_precompress`). Runs on
    // the blocking pool for the same reason the tar inflate does: brotli over a
    // multi-MB bundle is CPU-bound and would park a Tokio worker. Additive —
    // the store simply receives more files, and a build with no siblings
    // (published before this existed) still serves correctly.
    //
    // NOTE ON THE SIZE CAP: `MAX_DECOMPRESSED_BYTES` bounds what
    // `unpack_tar_gz` produces, and these variants are appended *after* that
    // check — so peak resident bytes for a publish is the unpacked bundle plus
    // its compressible subset in brotli form (plus rayon's per-thread output
    // buffers). The effective ceiling is therefore ~1.3x the stated cap, not
    // the cap. Kept out of the cap deliberately: the cap exists to bound what
    // an *uploader* can make us hold, and these bytes are ours, derived from
    // already-admitted input. Count them here if that ratio ever gets tighter.
    let files = tokio::task::spawn_blocking(move || {
        let mut files = files;
        let mut variants = precompress::precompressed_variants(&files);
        files.append(&mut variants);
        files
    })
    .await
    .map_err(|e| PublishError::BadTarball(format!("bundle pre-compression task failed: {e}")))?;

    let (app_id, is_new_app, rollback) = upsert_app(&db, &org, &input).await?;
    // A build id must be unique per app. Everything downstream treats a build's
    // stored bytes as immutable and addressable by id:
    //
    // - `put_build` writes key by key into `build_prefix(app_id, build_id)` and
    //   never wipes the prefix first, so a re-publish under a reused id merges
    //   into the old build rather than replacing it.
    // - `custom_apps_bundle_cache` caches per-`build_id` *absences* on the
    //   strength of "a build's file set is fixed once `put_build` returns". A
    //   file the second publish adds would read as permanently missing on every
    //   replica that already probed for it — and for a `.js` request that means
    //   the SPA `index.html` fallback at 200, i.e. a broken app that does not
    //   self-heal without a process restart.
    // - `gc_builds` protects by row PK but deletes by `build_id` *prefix*, so
    //   reaping either of two rows sharing an id deletes the bytes the other
    //   one still points at.
    //
    // Rejecting is the only option that keeps those three honest; overwriting
    // silently corrupts a build replicas have already cached. The CLI's default
    // id is unique per run (`cli/commands/publish.rs`), so in practice this
    // fires only on an explicitly reused `--build-id`.
    match build_id_taken(&db, app_id, &input.build_id).await {
        Ok(false) => {}
        Ok(true) => {
            rollback.apply(&db, app_id).await;
            return Err(PublishError::DuplicateBuild {
                app_slug: input.app_slug.clone(),
                build_id: input.build_id.clone(),
            });
        }
        Err(e) => {
            rollback.apply(&db, app_id).await;
            return Err(e);
        }
    }
    // Bytes must land before the row mutation is allowed to stand. If `put_build`
    // fails (guaranteed on a multi-replica deploy with no bucket configured — it
    // refuses outright), undo the app-row change so a live app is never left
    // repointed at a different workspace while still serving the old build.
    let s3_prefix = match store::put_build(app_id, &input.build_id, files).await {
        Ok(prefix) => prefix,
        Err(e) => {
            rollback.apply(&db, app_id).await;
            return Err(e.into());
        }
    };
    // Capture the function specs + build prefix before `record_build` consumes
    // `manifest_json` and `s3_prefix`.
    let fn_specs = function_specs(manifest_json.as_ref());
    let build_prefix = s3_prefix.clone();
    // Bytes are now stored. If recording the row or moving the pointer fails,
    // roll the orphaned build back out so a partial publish leaves no
    // half-state (leaked storage prefix, or a row no channel points at).
    let build_pk = match record_build(&db, app_id, &input, s3_prefix, manifest_json).await {
        Ok(pk) => pk,
        Err(e) => {
            if let Err(cleanup) = store::delete_build(app_id, &input.build_id).await {
                tracing::warn!("publish rollback: orphan prefix left for {app_id}: {cleanup}");
            }
            rollback.apply(&db, app_id).await;
            return Err(e);
        }
    };
    // Register the bundle's functions against this build so `/fn/<name>`
    // resolves. On failure roll the orphan build back out (the app_functions
    // FK cascades when the build row is deleted).
    if let Err(e) = record_functions(&db, app_id, build_pk, &build_prefix, &fn_specs).await {
        rollback_stored_build(&db, app_id, &input.build_id, build_pk, rollback).await;
        return Err(e);
    }
    // Schema ships with the code that needs it — but only on a PROMOTING
    // publish, and only BEFORE the published pointer moves.
    //
    // Before, because a half-migrated app whose code already went live is worse
    // than a promote that did not happen: the app 500s on a table that isn't
    // there while the previous build, which worked, has already been replaced.
    // On failure we roll the orphan build back out and return, so the live
    // channel is untouched and the author sees the failure on the publish that
    // caused it.
    //
    // Only on promote, because the draft channel and the published channel share
    // ONE `app_<writer>` schema. Applying a draft's DDL would put unreviewed
    // schema in front of the live build.
    //
    // A partial apply is not rolled back and must not be: files that succeeded
    // are in the ledger, so the re-publish after the author fixes the broken one
    // resumes rather than restarts. That is the whole reason the ledger exists.
    if input.promote {
        match migrations::apply_on_promote(
            &db,
            app_id,
            &input.app_slug,
            org.id,
            build_pk,
            &declared_migrations,
        )
        .await
        {
            Ok(applied) => {
                let summary = applied.summary();
                if !summary.is_empty() {
                    tracing::info!("publish: {summary} for app {app_id}");
                }
            }
            Err(e) => {
                tracing::warn!("publish: schema migrations failed for app {app_id}: {e}");
                rollback_stored_build(&db, app_id, &input.build_id, build_pk, rollback).await;
                return Err(PublishError::Migration(e));
            }
        }
    }
    if let Err(e) = set_pointers(&db, app_id, build_pk, input.promote).await {
        rollback_stored_build(&db, app_id, &input.build_id, build_pk, rollback).await;
        return Err(e);
    }
    // Schedules track the LIVE build, so (re)register + reconcile function
    // schedules only on a PROMOTING publish — a draft-only publish shouldn't
    // start firing background runs — and after `set_pointers` so a fire resolves
    // the just-set `published_build_id`. Best-effort: a schedule failure never
    // fails the publish (functions stay route-invocable).
    let warnings = if input.promote {
        register_function_schedules(&db, app_id, input.project_id, &fn_specs).await
    } else {
        Vec::new()
    };
    gc_builds(&db, app_id, &[build_pk]).await;

    // The serve path caches the `apps` row — including the channel pointers
    // `set_pointers` just moved — so a publish must drop that cache or the new
    // build stays invisible for up to the cache TTL. `oxy publish` is
    // interactive and the engineer reloads immediately; a stale minute reads
    // as "my publish did nothing".
    //
    // Per-process, like every other cache here: on a multi-replica fleet only
    // the replica that took the publish drops it, and the others age out
    // within `CACHE_TTL`.
    super::custom_apps_cache::invalidate_app_resolution_cache();

    // Warm the bundle LRU for everything on the critical path — the shell, the
    // asset manifest the serve path reads to build preload hints, and the entry
    // chunks themselves. `oxy publish` is interactive: the engineer reloads
    // within seconds, and without this every one of those is a cold
    // object-store read on the very request they are watching.
    if let Some(bytes) = index_bytes {
        cache::seed(app_id, &input.build_id, "index.html", bytes);
    }
    if let Ok(bytes) = serde_json::to_vec(&asset_manifest) {
        cache::seed(
            app_id,
            &input.build_id,
            asset_manifest::ASSET_MANIFEST_PATH,
            axum::body::Bytes::from(bytes),
        );
    }
    for (path, bytes) in seed_entries {
        cache::seed(app_id, &input.build_id, &path, bytes);
    }

    Ok(PublishResult {
        app_id,
        build_id: input.build_id,
        url: format!("/customer-apps/{}/{}/", org.slug, input.app_slug),
        channel: if input.promote { "published" } else { "draft" }.to_string(),
        org_slug: org.slug.clone(),
        is_new_app,
        warnings,
    })
}

/// `POST /api/customer-apps/publish` — thin multipart shim over [`publish`].
///
/// The app-admin guard middleware has already authenticated the caller; we
/// pull the user via `AuthenticatedUserExtractor` (before `Multipart`, which
/// consumes the body) to stamp `published_by` on the build.
pub async fn publish_handler(
    oxy_auth::extractor::AuthenticatedUserExtractor(user): oxy_auth::extractor::AuthenticatedUserExtractor,
    // Present iff authenticated via an app publish token; its `app_id` is set only
    // for OIDC-minted machine tokens.
    marker: Option<axum::Extension<oxy_auth::types::AppPublishTokenAuth>>,
    mut multipart: Multipart,
) -> Result<Json<PublishResult>, (StatusCode, String)> {
    let machine_app_id = marker.and_then(|axum::Extension(m)| m.app_id);
    let mut org: Option<String> = None;
    let mut org_id: Option<Uuid> = None;
    // Tracks whether the publisher sent a non-empty `org_id` that
    // didn't parse — we surface that as a 400 instead of silently
    // falling back to `org`, so a typo'd UUID doesn't end up landing
    // the publish on a different org's row that happens to match an
    // unrelated `org=` slug.
    let mut org_id_invalid: Option<String> = None;
    let mut app = None;
    let mut project_id = None;
    let mut branch = None;
    let mut build_id = None;
    let mut name = None;
    let mut promote = false;
    let mut manifest = None;
    let mut tarball: Option<Vec<u8>> = None;
    let mut source_repo = None;
    let mut commit_sha = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("multipart error: {e}")))?
    {
        match field.name().unwrap_or_default() {
            "bundle" => {
                tarball = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| (StatusCode::BAD_REQUEST, format!("bundle read: {e}")))?
                        .to_vec(),
                );
            }
            "manifest" => {
                let raw = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("manifest read: {e}")))?;
                if !raw.trim().is_empty() {
                    // A present-but-unparseable manifest is a hard 400, not a
                    // silent drop. Degrading to `None` strips the bundle's whole
                    // `functions` block and publishes a function-less app with a
                    // 200 — then `useFunction` 404s and the author is misdiagnosed
                    // as never having declared the function.
                    manifest = Some(serde_json::from_str(&raw).map_err(|e| {
                        (
                            StatusCode::BAD_REQUEST,
                            format!("manifest field is not valid JSON: {e}"),
                        )
                    })?);
                }
            }
            field_name => {
                let key = field_name.to_string();
                // Propagate a field read error rather than silently coercing it to
                // "" — an empty `promote` degrades a `--promote` publish to draft
                // with no signal.
                let val = field
                    .text()
                    .await
                    .map_err(|e| (StatusCode::BAD_REQUEST, format!("field '{key}' read: {e}")))?;
                match key.as_str() {
                    // `org` accepts either a slug or a UUID — auto-detected
                    // by `OrgRef::from_str_auto` below. `org_id` is the
                    // explicit alias for the UUID form; if both are present
                    // `org_id` wins (older clients that send just `org=`
                    // still work unchanged).
                    "org" => org = Some(val),
                    "org_id" => {
                        let trimmed = val.trim();
                        if trimmed.is_empty() {
                            // Empty string is treated as "field omitted"
                            // (consistent with how other multipart fields
                            // are handled here).
                        } else {
                            match Uuid::parse_str(trimmed) {
                                Ok(id) => org_id = Some(id),
                                Err(_) => org_id_invalid = Some(trimmed.to_string()),
                            }
                        }
                    }
                    "app" => app = Some(val),
                    "project" | "project_id" => project_id = Uuid::parse_str(val.trim()).ok(),
                    "branch" => branch = Some(val).filter(|s| !s.is_empty()),
                    "build_id" => build_id = Some(val).filter(|s| !s.is_empty()),
                    "name" => name = Some(val).filter(|s| !s.is_empty()),
                    "source_repo" => source_repo = Some(val).filter(|s| !s.is_empty()),
                    "commit_sha" => commit_sha = Some(val).filter(|s| !s.is_empty()),
                    "channel" => promote = val == "published",
                    "promote" => promote = val == "true" || val == "1",
                    _ => {}
                }
            }
        }
    }

    // Reject a bundle-supplied display name with control/JSON-breaking chars
    // before it lands in the apps row (defense against injection via the
    // uploaded manifest / multipart field).
    if let Some(n) = &name {
        crate::server::api::admin::apps::handlers::validate_display_name(n)
            .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    }

    if let Some(bad) = org_id_invalid {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("org_id is not a valid UUID: {bad:?}"),
        ));
    }
    // No org sent — infer it from the pinned workspace server-side (a
    // workspace belongs to exactly one org). `project_id` is validated as
    // present below, so `publish()` always has something to infer from.
    let org_ref = match (org_id, org) {
        (Some(id), _) => Some(OrgRef::Id(id)),
        (None, Some(s)) => Some(OrgRef::from_str_auto(&s)),
        (None, None) => None,
    };

    let input = PublishInput {
        org_ref,
        app_slug: app.ok_or((StatusCode::BAD_REQUEST, "missing app".into()))?,
        project_id: project_id
            .ok_or((StatusCode::BAD_REQUEST, "missing/invalid project".into()))?,
        branch,
        build_id: build_id.unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
        name,
        promote,
        tarball: tarball.ok_or((StatusCode::BAD_REQUEST, "missing bundle".into()))?,
        manifest,
        source_repo,
        commit_sha,
        published_by: Some(user.id),
        // `user.email`, never a display label. This value is not decoration:
        // `publish_authz::resolve_actor` feeds it to `platform_reaches`, so a
        // label that fell back to the non-unique `name` would let a user named
        // after a staff address publish with STAFF authority into any org that
        // grant reaches. `publish` already denies a `None` here, which is the
        // right answer — publishing is a developer action and a worker
        // enrolled without a mailbox has no path to it.
        published_by_email: user.email.clone(),
        machine_app_id,
    };

    publish(input)
        .await
        .map(Json)
        .map_err(|e| (e.status(), e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    fn make_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn unpack_roundtrips_and_strips_dot_slash() {
        let gz = make_tar_gz(&[
            ("./index.html", b"<html>"),
            ("assets/app.js", b"console.log(1)"),
        ]);
        let files = unpack_tar_gz(&gz).expect("unpack");
        assert!(
            files
                .iter()
                .any(|(p, b)| p == "index.html" && b == b"<html>")
        );
        assert!(files.iter().any(|(p, _)| p == "assets/app.js"));
    }

    #[test]
    fn unpack_rejects_missing_index() {
        let gz = make_tar_gz(&[("assets/app.js", b"x")]);
        let err = unpack_tar_gz(&gz).unwrap_err();
        assert!(matches!(err, PublishError::BadTarball(_)));
    }

    #[test]
    fn unpack_rejects_oversized_single_entry() {
        // 20 bytes with a 10-byte per-entry cap → rejected, and the read is
        // bounded by `.take(cap+1)` so a lying header can't OOM us first.
        let gz = make_tar_gz(&[("index.html", &[b'a'; 20])]);
        let err = unpack_tar_gz_bounded(&gz, 1_000, 10, 100).unwrap_err();
        assert!(
            matches!(&err, PublishError::BadTarball(m) if m.contains("per-file")),
            "expected per-file limit error, got {err:?}"
        );
    }

    #[test]
    fn unpack_rejects_oversized_total() {
        // Each file is under the per-entry cap, but together they blow the total.
        let gz = make_tar_gz(&[
            ("index.html", &[b'a'; 8]),
            ("a.js", &[b'b'; 8]),
            ("b.js", &[b'c'; 8]),
        ]);
        let err = unpack_tar_gz_bounded(&gz, 20, 100, 100).unwrap_err();
        assert!(
            matches!(&err, PublishError::BadTarball(m) if m.contains("decompressed limit")),
            "expected total-bytes limit error, got {err:?}"
        );
    }

    #[test]
    fn unpack_rejects_too_many_files() {
        let gz = make_tar_gz(&[("index.html", b"x"), ("a.js", b"x"), ("b.js", b"x")]);
        let err = unpack_tar_gz_bounded(&gz, 1_000, 100, 2).unwrap_err();
        assert!(
            matches!(&err, PublishError::BadTarball(m) if m.contains("file limit")),
            "expected file-count limit error, got {err:?}"
        );
    }

    #[test]
    fn embedded_manifest_present_but_malformed_is_rejected() {
        // The canonical `oxy publish` path: a malformed oxy-app.json must 4xx,
        // not silently strip the functions block and publish function-less.
        let files = vec![("oxy-app.json".to_string(), b"{ not: json".to_vec())];
        let err = parse_embedded_manifest(&files).unwrap_err();
        assert!(
            matches!(&err, PublishError::BadTarball(m) if m.contains("oxy-app.json")),
            "expected a BadTarball naming oxy-app.json, got {err:?}"
        );
    }

    #[test]
    fn embedded_manifest_absent_falls_through() {
        let files = vec![("index.html".to_string(), b"<html>".to_vec())];
        assert!(parse_embedded_manifest(&files).unwrap().is_none());
    }

    #[test]
    fn embedded_manifest_valid_parses() {
        let files = vec![(
            "oxy-app.json".to_string(),
            br#"{"slug":"x","functions":{}}"#.to_vec(),
        )];
        assert!(parse_embedded_manifest(&files).unwrap().is_some());
    }

    #[test]
    fn unpack_accepts_a_normal_bundle_within_limits() {
        let gz = make_tar_gz(&[("index.html", b"<html>"), ("app.js", b"x")]);
        let files = unpack_tar_gz_bounded(&gz, 1_000, 100, 100).expect("within limits");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn function_specs_extracts_declared_functions() {
        let manifest = serde_json::json!({
            "slug": "hello-oxy",
            "functions": {
                "top-stores": { "route": true, "timeoutSeconds": 15 },
            },
        });
        let specs = function_specs(Some(&manifest));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].0, "top-stores");
        assert_eq!(specs[0].1["route"], serde_json::json!(true));
        assert_eq!(specs[0].1["timeoutSeconds"], serde_json::json!(15));
    }

    #[test]
    fn function_specs_empty_when_no_functions_block() {
        // A static-bundle manifest (today's default) records no functions.
        assert!(function_specs(Some(&serde_json::json!({ "slug": "x" }))).is_empty());
        assert!(function_specs(None).is_empty());
        // A present-but-empty block is also a no-op.
        assert!(function_specs(Some(&serde_json::json!({ "functions": {} }))).is_empty());
    }

    #[test]
    fn function_artifact_key_matches_build_store_layout() {
        assert_eq!(
            function_artifact_key("customer-apps/abc/builds/v1/", "top-stores"),
            "customer-apps/abc/builds/v1/functions/top-stores.js"
        );
    }

    #[test]
    fn validate_function_names_rejects_traversal() {
        let bad = vec![("../../evil".to_string(), serde_json::json!({}))];
        let err = validate_function_names(&bad).unwrap_err();
        assert!(
            matches!(&err, PublishError::BadTarball(m) if m.contains("function name")),
            "expected a BadTarball naming the function, got {err:?}"
        );
    }

    #[test]
    fn validate_function_names_rejects_bad_shapes() {
        // Uppercase, underscore, leading hyphen, empty, and a path separator all
        // fail — these are exactly the names the sources.rs rewrite argument and
        // the artifact-key path segment must never see.
        for name in ["Notify", "send_email", "-lead", "", "a/b", "a.b"] {
            let specs = vec![(name.to_string(), serde_json::json!({}))];
            assert!(
                validate_function_names(&specs).is_err(),
                "expected {name:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_function_names_accepts_valid() {
        let ok = vec![
            ("notify".to_string(), serde_json::json!({})),
            ("top-stores".to_string(), serde_json::json!({})),
            ("f0".to_string(), serde_json::json!({})),
        ];
        assert!(validate_function_names(&ok).is_ok());
    }

    #[test]
    fn unpack_path_guard_rejects_unsafe() {
        // The tar *writer* already refuses to emit `..`, so we exercise the
        // guard predicate directly — it must reject traversal and absolute
        // paths from archives built by other tools, and allow normal ones.
        use std::path::Path;
        assert!(!is_safe_relative_path(Path::new("../escape.html")));
        assert!(!is_safe_relative_path(Path::new("a/../../b")));
        assert!(!is_safe_relative_path(Path::new("/abs/secret")));
        assert!(is_safe_relative_path(Path::new("index.html")));
        assert!(is_safe_relative_path(Path::new("assets/app.js")));
    }

    #[test]
    fn humanize_slug_titlecases_words() {
        assert_eq!(humanize_slug("weekly-sales"), "Weekly Sales");
        assert_eq!(humanize_slug("dashboard"), "Dashboard");
    }

    #[test]
    fn oxy_access_denied_is_forbidden() {
        let e = PublishError::OxyAccessDenied {
            org: "acme".to_string(),
            project: Uuid::nil(),
        };
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert!(e.to_string().contains("has not granted Oxy access"));
    }

    /// A malformed id is the bundle-is-fine-but-your-input-isn't case: 422,
    /// and the message has to say what the accepted shape is.
    #[test]
    fn invalid_build_id_is_unprocessable_and_states_the_rule() {
        let e = PublishError::InvalidBuildId("../../etc".to_string());
        assert_eq!(e.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let msg = e.to_string();
        assert!(msg.contains("../../etc"));
        assert!(msg.contains("letters, digits"), "{msg}");
    }

    /// A reused build id is a conflict, not a bad request — the bundle is
    /// fine, the id is taken. The message has to name the remedy, because the
    /// CLI surfaces the raw body (`cli/commands/publish.rs`).
    #[test]
    fn duplicate_build_is_a_conflict_and_names_the_remedy() {
        let e = PublishError::DuplicateBuild {
            app_slug: "store-pulse".to_string(),
            build_id: "abc123".to_string(),
        };
        assert_eq!(e.status(), StatusCode::CONFLICT);
        let msg = e.to_string();
        assert!(msg.contains("store-pulse") && msg.contains("abc123"));
        assert!(
            msg.contains("--build-id"),
            "the error must tell the publisher how to proceed: {msg}"
        );
    }

    /// Naming another workspace conflicts with the existing row; the bundle is
    /// fine. The message names both workspaces and the deliberate way to move
    /// an app, because the CLI surfaces the raw body.
    #[test]
    fn project_mismatch_is_a_conflict_and_names_both_workspaces_and_the_move() {
        let (app_id, existing, requested) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let e = PublishError::ProjectMismatch {
            app_slug: "bookkeeping".to_string(),
            app_id,
            existing_project: existing,
            requested_project: requested,
        };
        assert_eq!(e.status(), StatusCode::CONFLICT);
        let msg = e.to_string();
        assert!(msg.contains("bookkeeping"), "{msg}");
        assert!(msg.contains(&format!("--project {existing}")), "{msg}");
        assert!(msg.contains(&requested.to_string()), "{msg}");
        assert!(
            msg.contains(&format!("PATCH /api/customer-apps/{app_id}")),
            "the error must name the admin path that moves an app: {msg}"
        );
    }
}
