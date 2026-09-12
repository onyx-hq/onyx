//! Example custom-app seed — deploys the checked-in `oxy-starter` bundle so a
//! fresh `oxy seed` lands on a launcher with a real, clickable app instead of an
//! empty grid.
//!
//! **It deploys through the real publish path**, not a shortcut: bytes go to the
//! build store, then an `app_builds` row, then the channel pointers — the same
//! order (and the same code) `POST /api/customer-apps/publish` uses. What it
//! skips is only the transport: no tarball, no HTTP, no bundle validation. That
//! matters because it means the seeded app exercises the serve path a real
//! `oxyc publish` produces, so a test against it tests the shipping code.
//!
//! The bundle is checked into the repo (`examples/customer_apps/oxy-starter/`)
//! rather than generated here: it's reviewable in a diff, and `oxy seed` stays a
//! tool that reads the workspace instead of writing to it.
//!
//! No S3, no Node. The build store falls back to the filesystem when
//! `OXY_CUSTOMER_APPS_S3_BUCKET` is unset, and the bundle is hand-written HTML
//! with no build step — so this works on a fresh clone with no toolchain.
//!
//! **Superseded builds are left behind on purpose.** Editing the bundle changes
//! its content hash, so a re-seed writes a new build (row + bytes) and repoints
//! the channels at it; the previous one stays until `oxy seed --clear`. That's
//! not a leak to fix here — it's how a rollback stays possible, it mirrors what
//! the real publish pipeline keeps (which has `gc_builds` for the hosted case),
//! and the cost is a few KB per edit on a dev machine.

use std::path::{Path, PathBuf};

use chrono::Utc;
use entity::prelude::{AppBuilds, AppTeamGrants, Apps, OrgTeams, Organizations, Workspaces};
use entity::{app_builds, app_team_grants, apps, organizations, workspaces};
use oxy::theme::StyledText;
use oxy_shared::errors::OxyError;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
    QueryOrder,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::server::api::custom_apps_asset_manifest as asset_manifest;
use crate::server::api::custom_apps_build_store as store;

type Conn = sea_orm::DatabaseConnection;

/// Bundle directory, relative to the seeded workspace path.
const BUNDLE_REL_PATH: &str = "customer_apps/oxy-starter";

/// The app slug, shared by every deployment. `apps` is unique on
/// `(org_id, slug)`, not on slug alone — so the same bundle can serve
/// `local/oxy-starter` and `acme/oxy-starter` as two independent deployments,
/// which is exactly the multi-tenant shape the platform is built for.
///
/// Deliberately NOT `hello-oxy`: that slug belongs to the canonical worked
/// example in the customer-apps repo. Sharing it would mean a developer who
/// publishes the real one locally collides with the seed on `(org_id, slug)` —
/// and `oxy seed --clear` would then delete their work.
const APP_SLUG: &str = "oxy-starter";

/// The slug of the second, RESTRICTED deployment seeded alongside the open one.
///
/// A separate app rather than restricting the only one: an org whose sole app is
/// invisible to most of its people reads as a broken seed, not as a demonstrated
/// feature. With two, the launcher shows one card to everyone and the second only to
/// the granted team — which is the contrast the feature is actually about.
const RESTRICTED_APP_SLUG: &str = "oxy-starter-private";

/// Files that document the example but aren't part of the deployed bundle.
/// A real `oxyc publish` ships a build output directory, which wouldn't
/// contain these.
const NOT_BUNDLE_FILES: &[&str] = &["README.md"];

/// One deployment of the bundle: an org and the workspace whose launcher
/// should show it.
pub(crate) struct AppTarget {
    pub org_id: Uuid,
    pub org_slug: String,
    /// `apps.project_id` — the column is named `project_id` but holds a
    /// `workspaces.id`. The launcher filters on it verbatim.
    pub workspace_id: Uuid,
    /// The app's slug. Usually [`APP_SLUG`]; a second deployment into the same org
    /// uses a different one so the two don't collide on `(org_id, slug)`.
    pub slug: String,
    pub name: String,
    /// When set, the app is restricted to this team instead of being org-visible.
    ///
    /// Deliberately a per-target field rather than a per-ORG rule: an org's ONLY app
    /// must never be the restricted one, or the seed hides the app from most of that
    /// org's people and the launcher looks broken. Orgs that demonstrate the
    /// restricted state get a SECOND app for it.
    pub restrict_to_team: Option<Uuid>,
}

impl AppTarget {
    /// The default, org-visible deployment.
    pub fn open(org_id: Uuid, org_slug: String, workspace_id: Uuid) -> Self {
        Self {
            org_id,
            org_slug,
            workspace_id,
            slug: APP_SLUG.to_string(),
            name: "Oxy Starter".to_string(),
            restrict_to_team: None,
        }
    }

    /// A second deployment in the same org, restricted to `team_id` — the seeded
    /// example of `visibility = 'members'`.
    pub fn restricted(&self, team_id: Uuid) -> Self {
        Self {
            org_id: self.org_id,
            org_slug: self.org_slug.clone(),
            workspace_id: self.workspace_id,
            slug: RESTRICTED_APP_SLUG.to_string(),
            name: "Oxy Starter (Private)".to_string(),
            restrict_to_team: Some(team_id),
        }
    }
}

/// Deterministic per-(org, slug) app id, so re-seeding updates one row rather than
/// racing the `(org_id, slug)` unique index.
fn app_id_for(org_id: Uuid, slug: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!("oxy.app-seed.{org_id}.{slug}").as_bytes(),
    )
}

/// Content-addressed build id: hash the bundle, use the first 16 hex chars.
///
/// A stable id would let edited bytes land on the prefix a running server has
/// already cached, and the reader would keep serving the old page
/// (`custom_apps_bundle_cache` keys on `(app_id, build_id, path)`). Hashing
/// means changed bytes are a different build, so an edit can't be shadowed by
/// a warm cache. It also mirrors what a real publish does with a commit sha.
fn build_id_for(files: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    for (rel, bytes) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Read the bundle into `(relative_path, bytes)` pairs, sorted so the content
/// hash is stable across filesystems (readdir order is not).
async fn read_bundle(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, OxyError> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files).await?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    if !files.iter().any(|(rel, _)| rel == "index.html") {
        return Err(OxyError::RuntimeError(format!(
            "example app bundle at {} has no index.html",
            dir.display()
        )));
    }
    Ok(files)
}

/// Recursive walk. Boxed because `async fn` recursion needs an indirection.
fn collect_files<'a>(
    root: &'a Path,
    dir: &'a Path,
    out: &'a mut Vec<(String, Vec<u8>)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), OxyError>> + Send + 'a>> {
    Box::pin(async move {
        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| OxyError::RuntimeError(format!("read {}: {e}", dir.display())))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| OxyError::RuntimeError(format!("walk {}: {e}", dir.display())))?
        {
            let path = entry.path();
            if path.is_dir() {
                collect_files(root, &path, out).await?;
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|e| OxyError::RuntimeError(format!("relativize {path:?}: {e}")))?
                .to_string_lossy()
                .replace('\\', "/");
            if NOT_BUNDLE_FILES.contains(&rel.as_str()) {
                continue;
            }
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| OxyError::RuntimeError(format!("read {}: {e}", path.display())))?;
            out.push((rel, bytes));
        }
        Ok(())
    })
}

/// The bundle's own `oxy-app.json`, for the `app_builds.manifest_json` column
/// the launcher reads card metadata from.
fn manifest_of(files: &[(String, Vec<u8>)]) -> Option<serde_json::Value> {
    let (_, bytes) = files.iter().find(|(rel, _)| rel == "oxy-app.json")?;
    serde_json::from_slice(bytes).ok()
}

/// Where the bundle lives for a given seeded workspace path.
fn bundle_dir(workspace_path: &str) -> PathBuf {
    Path::new(workspace_path).join(BUNDLE_REL_PATH)
}

/// Deploy the example bundle into every target org.
///
/// Returns the number of apps deployed. Skips (returns `Ok(0)`) when the bundle
/// isn't present — `--workspace-path` can point at any directory, and a seed
/// shouldn't fail because the developer's own workspace has no example app in it.
pub(crate) async fn seed_example_apps(
    conn: &Conn,
    workspace_path: &str,
    targets: &[AppTarget],
) -> Result<usize, OxyError> {
    let dir = bundle_dir(workspace_path);
    if !dir.is_dir() {
        return Ok(0);
    }
    let files = read_bundle(&dir).await?;
    let build_id = build_id_for(&files);
    let manifest = manifest_of(&files);

    println!(
        "{} deploying example app ({} file{}, build {build_id})",
        "📦".info(),
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );

    let mut deployed = 0;
    for target in targets {
        deploy(conn, target, &files, &build_id, manifest.clone()).await?;
        // An org's home page is its SUBDOMAIN, and those are opt-in: without an
        // enabled `org_subdomains` row the host 302s to the app root, so a seeded
        // org had no home for its seeded app to appear on. Enable it here, pointed
        // at the workspace the app was just deployed into, so `<slug>.<zone>/` lands
        // on a launcher that actually lists it.
        ensure_org_subdomain(conn, target).await?;
        let restricted = apply_seed_visibility(conn, target).await?;
        println!(
            "  {} /customer-apps/{}/{}/{}",
            "✓".success(),
            target.org_slug,
            target.slug,
            if restricted { "  (restricted)" } else { "" }
        );
        deployed += 1;
    }
    Ok(deployed)
}

/// Enable the org's subdomain and point it at the workspace holding the seeded app.
///
/// Idempotent, and deliberately **non-destructive on a default project someone
/// chose**: an existing row has `enabled` forced, and its `default_workspace_id`
/// backfilled ONLY when empty — a developer who re-pointed an org at a different
/// default workspace keeps it across a re-seed, but an enabled row with no project
/// to scope to (which `set_subdomain` permits, and the `ON DELETE SET NULL` FK can
/// produce) is repaired rather than left as the one state this function exists to
/// prevent.
///
/// Note the zone itself is deployment config (`OXY_ORG_SUBDOMAIN_ZONE`, or derived
/// from `OXY_API_URL` when the admin host's first label is `app`). The row only says
/// "this org has one"; whether `<slug>.<zone>` resolves in a given environment is a
/// DNS/config question the seed can't answer.
async fn ensure_org_subdomain(conn: &Conn, target: &AppTarget) -> Result<(), OxyError> {
    use entity::org_subdomains;

    // The admin UI refuses to create a subdomain on a reserved label, and dispatch
    // bounces one before `resolve()` ever runs — so writing an enabled row for such
    // a slug would produce a host that looks configured and never works. No seeded
    // slug collides today; this keeps that true if one is ever added. Cosmetic loss,
    // so warn and carry on, like the rest of the example-app seed.
    if oxy_app_core::org_host_dispatch::is_reserved_label(&target.org_slug) {
        println!(
            "{} skipping org subdomain for reserved label '{}'",
            "⚠️".warning(),
            target.org_slug
        );
        return Ok(());
    }

    let existing = entity::prelude::OrgSubdomains::find()
        .filter(org_subdomains::Column::OrgId.eq(target.org_id))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("find org subdomain: {e}")))?;

    match existing {
        Some(row) => {
            // Backfill an EMPTY default workspace, but never overwrite a set one.
            // `set_subdomain` accepts `default_workspace_id: None`, and the FK is
            // `ON DELETE SET NULL`, so an enabled row with no default project is
            // reachable — and that is precisely the state this function exists to
            // prevent, since the org root would then have nothing to scope to.
            let needs_default = row.default_workspace_id.is_none();
            // A default pointing elsewhere is a deliberate re-point (the admin UI can
            // set one), so leave it — but say so. Deploying the example app to a
            // workspace the org root never scopes to produces exactly the empty grid
            // this function exists to prevent, and it is the half that leaves no
            // trace: the app is published, nothing errors, the home page is blank.
            if let Some(current) = row.default_workspace_id
                && current != target.workspace_id
            {
                println!(
                    "{} org '{}' scopes its subdomain to workspace {current}, but the example app went to {} — it will not show on the org root",
                    "⚠️".warning(),
                    target.org_slug,
                    target.workspace_id
                );
            }
            if !row.enabled || needs_default {
                let mut active = row.into_active_model();
                active.enabled = ActiveValue::Set(true);
                if needs_default {
                    active.default_workspace_id = ActiveValue::Set(Some(target.workspace_id));
                }
                active.updated_at = ActiveValue::Set(Utc::now().fixed_offset());
                active
                    .update(conn)
                    .await
                    .map_err(|e| OxyError::DBError(format!("enable org subdomain: {e}")))?;
            }
        }
        None => {
            org_subdomains::ActiveModel {
                id: ActiveValue::Set(Uuid::new_v5(
                    &Uuid::NAMESPACE_DNS,
                    format!("oxy.subdomain-seed.{}", target.org_id).as_bytes(),
                )),
                org_id: ActiveValue::Set(target.org_id),
                default_workspace_id: ActiveValue::Set(Some(target.workspace_id)),
                enabled: ActiveValue::Set(true),
                created_by: ActiveValue::Set(None),
                created_at: ActiveValue::NotSet,
                updated_at: ActiveValue::NotSet,
            }
            .insert(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("insert org subdomain: {e}")))?;
        }
    }
    Ok(())
}

/// Put the seeded app into the `visibility = 'members'` state for targets the seed
/// defines a team for, so a fresh `oxy seed` shows the restricted case as well as
/// the default one — otherwise the whole visibility feature looks unbuilt until
/// someone restricts an app by hand.
///
/// Returns whether the app was restricted. Idempotent: re-running rewrites the same
/// grant. A missing team is treated as "nothing to restrict" rather than an error —
/// the team seed is skipped on a non-local DB, so its absence is expected.
async fn apply_seed_visibility(conn: &Conn, target: &AppTarget) -> Result<bool, OxyError> {
    let Some(team_id) = target.restrict_to_team else {
        return Ok(false);
    };
    // The team is seeded by `seed_partner_tenants`, which is skipped on a non-local
    // DB — so its absence is expected, not an error.
    if OrgTeams::find_by_id(team_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("find seed team: {e}")))?
        .is_none()
    {
        return Ok(false);
    }

    let app_id = app_id_for(target.org_id, &target.slug);
    apps::ActiveModel {
        id: ActiveValue::Unchanged(app_id),
        visibility: ActiveValue::Set("members".to_string()),
        ..Default::default()
    }
    .update(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("restrict seeded app: {e}")))?;

    let grant_id = Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!("oxy.app-seed.grant.{app_id}.{team_id}").as_bytes(),
    );
    if AppTeamGrants::find_by_id(grant_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("find seed grant: {e}")))?
        .is_none()
    {
        app_team_grants::ActiveModel {
            id: ActiveValue::Set(grant_id),
            app_id: ActiveValue::Set(app_id),
            team_id: ActiveValue::Set(team_id),
            role: ActiveValue::Set(entity::app_members::ROLE_MEMBER.to_string()),
            created_at: ActiveValue::NotSet,
            created_by: ActiveValue::Set(None),
        }
        .insert(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("grant seeded app to team: {e}")))?;
    }
    Ok(true)
}

/// One deployment, in the same order `publish()` uses:
///
/// 1. the `apps` row — `app_builds.app_id` carries a foreign key to it, so a
///    build row can't exist first;
/// 2. the bytes;
/// 3. the `app_builds` row;
/// 4. the channel pointers.
///
/// Pointers last is what makes the sequence safe to interrupt: a row only ever
/// names a build whose bytes are already stored, so a half-finished seed leaves
/// the previous build live rather than a 404.
async fn deploy(
    conn: &Conn,
    target: &AppTarget,
    files: &[(String, Vec<u8>)],
    build_id: &str,
    manifest: Option<serde_json::Value>,
) -> Result<(), OxyError> {
    let app_id = app_id_for(target.org_id, &target.slug);
    ensure_app(conn, target, app_id).await?;

    // Same `__oxy/` namespace reservation + asset manifest a real `oxyc publish`
    // writes. The seeded app is the first bundle every new workspace opens, so
    // it is the last one that should be missing its preload hints and its
    // service-worker precache list — and running the shared installer here is
    // also what keeps the seed honest as that document's shape evolves.
    let mut files = files.to_vec();
    asset_manifest::install_into(&mut files, build_id, manifest.as_ref());

    // Unconditional, even when the rows already exist: this is what heals a
    // wiped state dir (`oxy clean`, a new machine, a pruned Docker volume).
    // The DB would still name the build, but the bytes would be gone and the
    // app would 404 — so re-running `oxy seed` is the repair.
    let prefix = store::put_build(app_id, build_id, files)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("store example app bundle: {e}")))?;

    let build_pk = upsert_build(conn, app_id, build_id, &prefix, manifest).await?;
    point_app_at(conn, app_id, build_pk).await?;
    Ok(())
}

/// The `app_builds` row. Keyed on `(app_id, build_id)` like the real publish,
/// which the schema enforces as unique.
async fn upsert_build(
    conn: &Conn,
    app_id: Uuid,
    build_id: &str,
    prefix: &str,
    manifest: Option<serde_json::Value>,
) -> Result<Uuid, OxyError> {
    let existing = AppBuilds::find()
        .filter(app_builds::Column::AppId.eq(app_id))
        .filter(app_builds::Column::BuildId.eq(build_id))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query app_build {build_id}: {e}")))?;
    if let Some(row) = existing {
        return Ok(row.id);
    }
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!("oxy.app-seed.build.{app_id}.{build_id}").as_bytes(),
    );
    app_builds::ActiveModel {
        id: ActiveValue::Set(id),
        app_id: ActiveValue::Set(app_id),
        build_id: ActiveValue::Set(build_id.to_string()),
        s3_prefix: ActiveValue::Set(prefix.to_string()),
        manifest_json: ActiveValue::Set(manifest),
        created_at: ActiveValue::Set(Utc::now().fixed_offset()),
        published_by: ActiveValue::Set(None),
        source_repo: ActiveValue::Set(Some("oxy-hq/oxygen".to_string())),
        commit_sha: ActiveValue::Set(None),
        source_branch: ActiveValue::Set(Some("main".to_string())),
        // The promote gate refuses to make a build live unless this is
        // "passed" — the seed bypasses the validator, so it must assert the
        // verdict the validator would have reached for a bundle this simple.
        validation_status: ActiveValue::Set("passed".to_string()),
        validation_detail: ActiveValue::Set(None),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert app_build {build_id}: {e}")))?;
    Ok(id)
}

/// The `apps` row, without channel pointers — those are set by
/// [`point_app_at`] once the build they name actually exists.
async fn ensure_app(conn: &Conn, target: &AppTarget, app_id: Uuid) -> Result<(), OxyError> {
    let now = Utc::now().fixed_offset();
    let existing = Apps::find_by_id(app_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query app {app_id}: {e}")))?;

    if let Some(row) = existing {
        // Re-point an existing row at the workspace, in case the demo
        // workspace was re-created with a different id.
        if row.project_id != target.workspace_id {
            let mut active = row.into_active_model();
            active.project_id = ActiveValue::Set(target.workspace_id);
            active.updated_at = ActiveValue::Set(now);
            active
                .update(conn)
                .await
                .map_err(|e| OxyError::DBError(format!("re-point app {app_id}: {e}")))?;
        }
        return Ok(());
    }

    apps::ActiveModel {
        // Leave to the DB default ('org'): a new app is org-visible unless
        // explicitly restricted later.
        visibility: sea_orm::ActiveValue::NotSet,
        id: ActiveValue::Set(app_id),
        slug: ActiveValue::Set(target.slug.clone()),
        name: ActiveValue::Set(target.name.clone()),
        org_id: ActiveValue::Set(target.org_id),
        project_id: ActiveValue::Set(target.workspace_id),
        branch: ActiveValue::Set("main".to_string()),
        source_repo: ActiveValue::Set("oxy-hq/oxygen".to_string()),
        status: ActiveValue::Set("created".to_string()),
        // "s3" is the tag for the build-store pipeline; the store itself picks
        // S3 or the filesystem from OXY_CUSTOMER_APPS_S3_BUCKET. "local" would
        // mean something else entirely — serve the dev's directory directly,
        // bypassing builds and channels — and would not exercise the real path.
        source_type: ActiveValue::Set("s3".to_string()),
        // NOT NULL with no default: NotSet fails the insert.
        source_config: ActiveValue::Set(serde_json::json!({})),
        last_synced_at: ActiveValue::Set(None),
        manifest_override: ActiveValue::Set(None),
        bootstrap_pr_url: ActiveValue::Set(None),
        // Pointers and publication are set by point_app_at() once the build
        // exists. Publishing here would name a build_pk that doesn't exist yet.
        published_at: ActiveValue::Set(None),
        repo_path: ActiveValue::Set(Some(format!("examples/{APP_SLUG}"))),
        draft_build_id: ActiveValue::Set(None),
        published_build_id: ActiveValue::Set(None),
        last_promoted_by: ActiveValue::Set(None),
        last_promoted_at: ActiveValue::Set(None),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert app {app_id}: {e}")))?;
    Ok(())
}

/// Make `build_pk` live on both channels and publish the app.
///
/// `published_at` is what the launcher filters on. Without it the app still
/// serves on a direct URL — `resolve_channel` falls back to the draft channel —
/// but never appears on the home grid. That half-state reads as "the launcher is
/// broken" rather than "the app isn't published", so the seed always sets it.
async fn point_app_at(conn: &Conn, app_id: Uuid, build_pk: Uuid) -> Result<(), OxyError> {
    let now = Utc::now().fixed_offset();
    let row = Apps::find_by_id(app_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query app {app_id}: {e}")))?
        .ok_or_else(|| OxyError::RuntimeError(format!("app {app_id} vanished mid-deploy")))?;

    let mut active = row.into_active_model();
    active.draft_build_id = ActiveValue::Set(Some(build_pk));
    active.published_build_id = ActiveValue::Set(Some(build_pk));
    active.published_at = ActiveValue::Set(Some(now));
    active.last_promoted_at = ActiveValue::Set(Some(now));
    active.updated_at = ActiveValue::Set(now);
    active
        .update(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("point app {app_id} at build: {e}")))?;
    Ok(())
}

/// Every app **this seed** created: one of the two seeded slugs, each carrying
/// the deterministic id derived from its `(org, slug)`.
///
/// The id is what proves provenance — a developer's own app that happens to share
/// a slug has a random id and is excluded. Any seed path that writes to or
/// deletes app-scoped data must select through here rather than `Apps::find()`,
/// or it will reach real tenant rows.
///
/// **Ordered by id**, because callers assign per-app fixture data by position.
/// Postgres returns unordered rows in whatever order it likes, and re-seeding
/// updates these rows, which can move them — so without this the app that is the
/// runaway on one run is the flat one on the next, and any test asserting which
/// app tops the growth column flakes.
pub(crate) async fn seeded_apps(conn: &Conn) -> Result<Vec<apps::Model>, OxyError> {
    Ok(Apps::find()
        .filter(apps::Column::Slug.is_in([APP_SLUG, RESTRICTED_APP_SLUG]))
        .order_by_asc(apps::Column::Id)
        .all(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query seeded apps: {e}")))?
        .into_iter()
        .filter(|row| row.id == app_id_for(row.org_id, &row.slug))
        .collect())
}

/// Tear down every app this seed deployed — the stored bytes as well as the
/// rows.
///
/// Scoped through [`seeded_apps`], so a developer's own app that happens to be
/// called `hello-oxy` is left alone. `apps.project_id` has no foreign key, so
/// dropping the workspace would otherwise leave these rows dangling — and the
/// bytes on disk have no owner at all.
pub(crate) async fn clear_example_apps(conn: &Conn) -> Result<u64, OxyError> {
    let rows = seeded_apps(conn).await?;

    let mut removed = 0;
    for row in rows {
        // Bytes first: a failure here leaves the row in place, so a re-run
        // still knows what to clean. The reverse would orphan the files.
        store::delete_app(row.id)
            .await
            .map_err(|e| OxyError::RuntimeError(format!("delete app bundle {}: {e}", row.id)))?;
        removed += Apps::delete_by_id(row.id)
            .exec(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("delete app {}: {e}", row.id)))?
            .rows_affected;
    }
    Ok(removed)
}

/// The workspace an org's example app belongs in — the one the partner seed
/// created, resolved by its DERIVED id rather than by sort order.
///
/// Sorting by name was only correct while the org had exactly the workspaces the
/// seed made. Anyone adding one that sorts earlier (`"AAA"` beats `"Acme Internal
/// Analytics"`) would have the next re-seed move the apps onto it, off the workspace
/// the org subdomain names as default — and the org's home page would render an
/// empty grid with nothing logged.
///
/// For an org the seed **does** define, the seeded workspace is the only answer:
/// absent, this returns `None` rather than guessing. The absence means the partner
/// seed never ran against this database — and an org slugged `acme` on the other
/// end of a remote `OXY_DATABASE_URL` is then somebody's real tenant, whose
/// workspace would get a demo app published into it and an enabled `org_subdomains`
/// row written over it.
///
/// First-by-name survives only for an org `ORGS` doesn't define, where there is no
/// derived id to prefer and no seed identity to contradict. `None` is also the
/// answer when the org itself is absent, so callers must tolerate it either way.
pub(crate) async fn seeded_app_workspace_of(
    conn: &Conn,
    org_slug: &str,
) -> Result<Option<AppTarget>, OxyError> {
    let Some(org) = Organizations::find()
        .filter(organizations::Column::Slug.eq(org_slug))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("lookup org {org_slug}: {e}")))?
    else {
        return Ok(None);
    };

    if let Some(seeded) = super::seed_partners::seeded_workspace_id(org_slug, org.id) {
        let exists = Workspaces::find_by_id(seeded)
            .one(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("lookup seeded workspace for {org_slug}: {e}")))?
            .is_some();
        // Return either way — never fall through. See the doc above: for a
        // seed-defined slug, a missing seeded workspace means this is not the seed's
        // database, and the fallback would write into whoever's org is really there.
        return Ok(exists.then(|| AppTarget::open(org.id, org.slug, seeded)));
    }

    let Some(ws) = Workspaces::find()
        .filter(workspaces::Column::OrgId.eq(org.id))
        .order_by_asc(workspaces::Column::Name)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("lookup workspace for {org_slug}: {e}")))?
    else {
        return Ok(None);
    };
    Ok(Some(AppTarget::open(org.id, org.slug, ws.id)))
}

#[cfg(test)]
#[path = "seed_apps_tests.rs"]
mod tests;
