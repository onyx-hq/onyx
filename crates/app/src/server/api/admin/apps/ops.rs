//! Internal helpers + the shared error type for the customer-apps admin
//! endpoints. Pure logic behind the HTTP handlers in `handlers.rs`; the serde
//! DTOs they operate on live in `dto.rs`.

use axum::Json;
use axum::http::StatusCode;
use chrono::Utc;
use entity::apps;
use entity::organizations;
use entity::prelude::{AppBuilds, Apps, Organizations};
use oxy_shared::utils::slugify;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue;
use sea_orm::ColumnTrait;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
use sea_orm::ModelTrait;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use uuid::Uuid;

use crate::server::api::workspace_org::{WorkspaceOrgMatch, workspace_in_org};

use super::dto::{ApiErr, AppResponse, ErrorBody};

/// Build an [`ApiErr`] with a custom message. Prefer this for paths
/// the user can fix (slug collision, missing org, malformed input);
/// use `internal()` for unexpected failures the operator can't act on.
pub(super) fn api_err(status: StatusCode, message: impl Into<String>) -> ApiErr {
    (
        status,
        Json(ErrorBody {
            message: message.into(),
        }),
    )
}

/// 500-equivalent for `.map_err`. Keeps call sites short while still
/// producing a body the frontend can show consistently.
pub(super) fn internal(_: impl std::fmt::Display) -> ApiErr {
    api_err(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error.")
}

/// Resolve and validate a caller-supplied `template_id` against the
/// registry. Returns `"vite"` when `id` is `None` (back-compat). Returns
/// an error string when `id` is `Some` but not registered.
///
/// Extracted as a pure helper so unit tests can exercise the validation
/// logic without spinning up an HTTP server or a database connection.
pub(crate) fn validate_template_id(id: Option<&str>) -> Result<&str, String> {
    let id = id.unwrap_or("vite");
    if crate::custom_app_template::registry::get_template(id).is_none() {
        return Err(format!("unknown template_id: {id}"));
    }
    Ok(id)
}

/// Validate a human-facing display name before it gets substituted
/// into JSON (`oxy-app.json`, `package.json`) and JSX (`App.tsx`)
/// templates. Rejects characters that would break the host language —
/// quotes, backslashes, angle brackets (HTML/JSX), curly braces (JSX
/// expressions), and ASCII control chars including newlines.
///
/// The substituter is intentionally a dumb string-replace so it can
/// be reused by both Rust and TypeScript code paths; pushing
/// validation to the boundary keeps the substituter simple and stops
/// the whole class of injection (JSON-shape, JSX-shape, HTML
/// fragment) at one place. Names that need richer characters can be
/// updated post-scaffold by editing the generated files directly.
pub(crate) fn validate_display_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name must not be empty".into());
    }
    // Count unicode scalar values, not bytes — the error message says
    // "characters" and a CJK app name of 43 characters is otherwise
    // rejected at 129 bytes (3 bytes per char in UTF-8).
    if trimmed.chars().count() > 128 {
        return Err("name must be at most 128 characters".into());
    }
    for c in trimmed.chars() {
        if c.is_control() {
            return Err("name must not contain control characters or newlines".into());
        }
        if matches!(c, '"' | '\\' | '<' | '>' | '{' | '}') {
            return Err(format!(
                "name must not contain the character {c:?} (would break the scaffolded JSON/JSX)"
            ));
        }
    }
    Ok(())
}

/// Validate a LocalFolder app's configured path. Returns operator-
/// facing warning strings — nothing here is an error (the row is
/// already persisted); these are hints the UI surfaces as toasts so
/// the operator catches a misconfigured path BEFORE they click
/// Preview and stare at a broken iframe.
///
/// Three things make a path "wrong" in different ways:
///   1. Empty / unset — path was never configured
///   2. Path exists but isn't a directory
///   3. Path is a directory but has no `index.html`
///
/// All three produce a single combined message because they share a
/// fix (set / correct the path to point at the build output).
pub(super) fn validate_local_source(
    source_type: &str,
    source_config: &serde_json::Value,
    org_slug: &str,
    app_slug: &str,
) -> Vec<String> {
    if source_type != "local" {
        return Vec::new();
    }
    let raw = source_config
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if raw.trim().is_empty() {
        return vec![
            "Local source has no path configured. Set it from Settings → Local bundle path."
                .to_string(),
        ];
    }
    let path = std::path::Path::new(raw);
    match path.metadata() {
        Ok(meta) if !meta.is_dir() => {
            vec![format!(
                "Local bundle path {raw:?} exists but isn't a directory. \
                 Point at the folder that holds index.html (Next.js export → out/, Vite → dist/)."
            )]
        }
        Ok(_) => {
            if path.join("index.html").exists() {
                check_baked_base_path(path, org_slug, app_slug)
            } else {
                vec![format!(
                    "Local bundle path {raw:?} has no index.html. \
                     Did you run `pnpm build`? Or point at the build output dir \
                     (Next.js export → out/, Vite → dist/)."
                )]
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            vec![format!(
                "Local bundle path {raw:?} doesn't exist on the oxy host. \
                 Check the path and try again."
            )]
        }
        Err(e) => {
            // Permission denied or other transient — surface verbatim
            // so the operator can act on it.
            vec![format!("Local bundle path {raw:?} can't be read: {e}.")]
        }
    }
}

/// Read the bundle's `index.html`, extract the baked
/// `/customer-apps/<org>/<slug>/` prefix, and warn if it doesn't
/// match the chosen `<org_slug>/<app_slug>`. This is the warning we
/// most need to surface at link time — the serve-time rewrite patches
/// `index.html` but cannot reach into the bundle's JS chunks, so a
/// slug-vs-baked mismatch means every data fetch from the bundle 404s
/// and the dashboard sits forever at "Loading…".
fn check_baked_base_path(
    bundle_dir: &std::path::Path,
    org_slug: &str,
    app_slug: &str,
) -> Vec<String> {
    let Ok(bytes) = std::fs::read(bundle_dir.join("index.html")) else {
        return Vec::new();
    };
    let Ok(html) = std::str::from_utf8(&bytes) else {
        return Vec::new();
    };
    let Some(baked) = crate::server::api::custom_apps_serve::first_custom_apps_prefix(html) else {
        // Bundle doesn't reference any /customer-apps/* prefix —
        // probably built without OXY_APP_BASE_PATH. The serve-time
        // path rewrite handles this case by injecting the expected
        // prefix, so it's not actionable here.
        return Vec::new();
    };
    let expected = format!("/customer-apps/{org_slug}/{app_slug}/");
    if baked == expected {
        return Vec::new();
    }
    vec![format!(
        "Bundle was built with base path {baked:?} baked in, but this app is \
         registered as {expected:?}. The JS chunks fetch from the baked path and \
         will 404 every data product (the dashboard will sit at 'Loading…' \
         forever). Fix by either rebuilding with OXY_APP_BASE_PATH={expected} \
         or changing the app slug to match the baked path."
    )]
}

/// Create `$OXY_STATE_DIR/customer-apps/<id>/source/` for a freshly
/// inserted local-source app. Returns the canonical path on success.
/// Failures are typed back as the HTTP status the caller should
/// surface so the rollback path stays simple.
pub(super) async fn provision_local_dir_for(id: Uuid) -> Result<std::path::PathBuf, ApiErr> {
    let state_root = std::env::var("OXY_STATE_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            api_err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Cannot provision a local bundle dir: OXY_STATE_DIR is not set.",
            )
        })?;

    let dir = std::path::PathBuf::from(state_root)
        .join("customer-apps")
        .join(id.to_string())
        .join("source");

    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        tracing::error!("create_dir_all({}): {e}", dir.display());
        api_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't create bundle directory at {}.", dir.display()),
        )
    })?;

    Ok(dir)
}

/// Bulk-resolve org_id → org_slug for a batch of apps. One query regardless
/// of how many apps; missing orgs (deleted out from under us — should be
/// impossible thanks to the FK cascade, but just in case) get a fallback
/// "unknown-org" slug so the response still serialises.
pub(super) async fn org_slugs_for(
    db: &DatabaseConnection,
    apps: &[apps::Model],
) -> Result<std::collections::HashMap<Uuid, String>, sea_orm::DbErr> {
    use std::collections::HashSet;
    let ids: HashSet<Uuid> = apps.iter().map(|a| a.org_id).collect();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = Organizations::find()
        .filter(organizations::Column::Id.is_in(ids.iter().copied()))
        .all(db)
        .await?;
    Ok(rows.into_iter().map(|o| (o.id, o.slug)).collect())
}

/// Resolve a set of user ids to their emails in one query. Deduped by the
/// `IN` clause; missing/legacy ids simply don't appear in the map.
pub(super) async fn emails_by_user_id(
    db: &DatabaseConnection,
    ids: Vec<Uuid>,
) -> Result<std::collections::HashMap<Uuid, String>, sea_orm::DbErr> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(entity::prelude::Users::find()
        .filter(entity::users::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|u| (u.id, u.label().to_string()))
        .collect())
}

/// Which apps on this page have a build running that records no usable git
/// source.
///
/// "Running" = the published build, falling back to draft — the same pointer
/// preference the serve path uses, because the question the operator is asking
/// is "if this app breaks, can anyone find its code?" and the answer is about
/// the bundle in front of users. Apps with no build at all are absent from the
/// set: nothing is deployed, so there is nothing orphaned yet.
///
/// Traceability itself is [`custom_app_provenance::classify`] — the rule
/// `oxyc publish` mirrors (`provenanceGaps`), so the warning an engineer sees
/// at publish time and the flag an operator sees in the list agree about what
/// counts.
///
/// One `IN` query for the whole page, like every other batched extra here,
/// and `select_only` because `app_builds` carries the `manifest_json` blob
/// that `icon_art_by_app` has already fetched for these same rows. A failed
/// lookup is the caller's to swallow — a missing warning must never 500 the
/// list.
pub(super) async fn unsourced_active_build_apps(
    db: &DatabaseConnection,
    rows: &[apps::Model],
) -> Result<std::collections::HashSet<Uuid>, sea_orm::DbErr> {
    use entity::app_builds::Column as BuildCol;

    // app_builds.id → the app whose active pointer names it.
    let build_to_app: std::collections::HashMap<Uuid, Uuid> = rows
        .iter()
        .filter_map(|r| r.published_build_id.or(r.draft_build_id).map(|b| (b, r.id)))
        .collect();
    if build_to_app.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let builds: Vec<(Uuid, Option<String>, Option<String>)> = AppBuilds::find()
        .select_only()
        .column(BuildCol::Id)
        .column(BuildCol::SourceRepo)
        .column(BuildCol::CommitSha)
        .filter(BuildCol::Id.is_in(build_to_app.keys().copied().collect::<Vec<_>>()))
        .into_tuple()
        .all(db)
        .await?;
    Ok(builds
        .into_iter()
        .filter(|(_, repo, commit)| {
            !crate::custom_app_provenance::classify(repo.as_deref(), commit.as_deref())
                .is_traceable()
        })
        .filter_map(|(id, _, _)| build_to_app.get(&id).copied())
        .collect())
}

/// Build the per-app `(icon_url, art_url)` map for a page, resolving every
/// manifest in ONE batched `app_builds` query (no N+1) and turning them into
/// URLs with the same helper the homepage launcher uses — so admin + launcher
/// agree on the picture. Published build preferred, draft as fallback (handled
/// in the batch resolver). Metadata: unresolved apps get `(None, None)`. See
/// the `oxy-app-visual-identity` skill.
pub(super) async fn icon_art_by_app(
    db: &sea_orm::DatabaseConnection,
    rows: &[apps::Model],
    org_slugs: &std::collections::HashMap<Uuid, String>,
) -> std::collections::HashMap<Uuid, (Option<String>, Option<String>)> {
    let manifests =
        crate::server::api::custom_apps_manifest::resolve_manifests_batch(db, rows).await;
    rows.iter()
        .filter_map(|app| {
            let slug = org_slugs.get(&app.org_id)?;
            Some((
                app.id,
                crate::server::api::workspace_custom_apps::icon_art_urls(
                    manifests.get(&app.id),
                    slug,
                    &app.slug,
                ),
            ))
        })
        .collect()
}

pub(super) fn rows_to_responses(
    rows: Vec<apps::Model>,
    org_slugs: &std::collections::HashMap<Uuid, String>,
) -> Vec<AppResponse> {
    rows.into_iter()
        .map(|row| {
            let org_slug = org_slugs
                .get(&row.org_id)
                .cloned()
                .unwrap_or_else(|| "unknown-org".to_string());
            AppResponse::from_model_with_org(row, &org_slug)
        })
        .collect()
}

// Shared single-app mutations + batch endpoints
//
// The publish/unpublish/delete handlers above and the batch endpoints below
// share one core mutation each (`*_one`) so a bulk action can never drift from
// its single-app counterpart. Batch endpoints are best-effort: every id is
// attempted independently and its outcome recorded, so one failure never
// aborts the rest.

/// Shared failure type for the single-app and batch mutation paths. `status`
/// drives the one-shot routes' HTTP code; `message` names the failure in a
/// batch result row.
pub struct AppOpError {
    pub status: StatusCode,
    pub(super) message: String,
}

impl AppOpError {
    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: "App not found.".into(),
        }
    }

    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Internal server error.".into(),
        }
    }

    /// A build can't be promoted because its recorded validation status is not
    /// `passed` (the validator-can't-be-bypassed gate).
    fn validation_failed(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: detail.into(),
        }
    }

    /// The app can't be deleted because it has a provisioned OLTP store. Deleting
    /// would leave `app_<slug>` as residue a NEW app reusing the freed slug would
    /// adopt — the same cross-app reach the rename guard blocks. Deprovision
    /// first; the message reaches the operator (batch surfaces it per-id, and the
    /// single-delete handler now returns it as a body too).
    fn has_oltp_store(org_id: Uuid, writer: &oxy_oltp::schema::WriterRef) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: oltp_store_blocks_message("deleted", org_id, writer),
        }
    }
}

/// Shared message for the app-rename and app-delete guards, so the two error
/// types that carry it — `ApiErr` in the update handler, [`AppOpError`] here —
/// cannot drift apart. `action` is the past participle ("renamed" / "deleted").
///
/// Names the concrete escape hatch: the per-writer deprovision, NOT the
/// whole-tenant one. The writer named is the DERIVED writer
/// (`writer.to_string()` → `app:oltp_bookings`), never the raw slug, because
/// the slug's hyphens are illegal in a writer name and `parse_writer` would
/// reject them.
///
/// It also names the STANDING the route needs, because the two doors differ:
/// this console is gated on `Action::PlatformApps` and that route on
/// `Action::PlatformOltp`, and an App Operator holds the first and explicitly
/// not the second (`oltp_provisioning_is_operator_work_not_app_work`). So the
/// population most likely to be deleting a custom app would otherwise be handed
/// a route that answers 403 — a narrower recurrence of the same defect this
/// message exists to fix.
///
/// It names the HTTP route FIRST and the CLI second, and that order is the
/// point. `oxy oltp deprovision --writer` has no `--env`: it reads
/// `OXY_DATABASE_URL` and talks to the local control plane. So for every
/// operator who hits this guard on a cloud environment — which is every
/// operator who hits it in production — the CLI command cannot reach the
/// environment that refused them, and the only route that could was the
/// whole-tenant `DELETE /oltp`. Naming a remedy that cannot be performed is
/// worse than naming none: it costs the reader the time to find out.
pub(super) fn oltp_store_blocks_message(
    action: &str,
    org_id: Uuid,
    writer: &oxy_oltp::schema::WriterRef,
) -> String {
    format!(
        "This app has a provisioned OLTP store, so it cannot be {action}. The store's schema is \
         bound to the app's slug; changing or removing the app while it exists would orphan the \
         data — or expose it to a different app that later reused the slug. Deprovision just this \
         app's store first — it drops that one schema, not the org's database:\n\
         \n\
         POST /api/admin/orgs/{org_id}/oltp/deprovision-writer {{\"writer\": \"{writer}\"}}\n\
         \n\
         That route needs platform-OLTP standing, which this console does not imply: an App \
         Operator can delete apps and cannot deprovision a store, so ask someone holding it. \
         Against a remote environment that route is still the ONLY way — \
         `oxy oltp deprovision --org {org_id} --writer {writer} --yes` has no `--env` and acts \
         on the local control plane."
    )
}

/// Promotion gate (validator-can't-be-bypassed): a build may reach the live
/// (published) channel only if its recorded validation status is `passed`.
/// Shared by EVERY AppOpError promotion path — publish, promote-latest — so no
/// path can quietly re-open the invariant. A missing build is refused (you can't
/// validate what isn't there), matching `custom_apps_publish::gate_promotion`.
/// Dormant today (every stored build is `passed`); load-bearing once gate 2 can
/// record `failed`.
async fn gate_build_promotion(db: &DatabaseConnection, build_pk: Uuid) -> Result<(), AppOpError> {
    let build = AppBuilds::find_by_id(build_pk)
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("promotion gate: build {build_pk} load failed: {e}");
            AppOpError::internal()
        })?
        .ok_or_else(|| {
            AppOpError::validation_failed(
                "build not found — cannot promote a missing build to live".to_string(),
            )
        })?;
    if build.validation_status != "passed" {
        return Err(AppOpError::validation_failed(format!(
            "build validation status is '{}', not 'passed' — cannot promote to live",
            build.validation_status
        )));
    }
    Ok(())
}

/// Load an app's org — needed to build [`AppResponse`]. A missing org is a
/// broken FK, surfaced as an internal error.
pub(super) async fn load_org(
    db: &DatabaseConnection,
    org_id: Uuid,
) -> Result<organizations::Model, AppOpError> {
    Organizations::find_by_id(org_id)
        .one(db)
        .await
        .map_err(|_| AppOpError::internal())?
        .ok_or_else(AppOpError::internal)
}

/// Core publish mutation shared by [`publish_app`] and [`batch_publish_apps`].
/// Pure pointer move: stamp `published_at`/promoter, repoint the published
/// channel at the current draft build, and drop the canonical-dir cache so the
/// serve path resolves the freshly-published channel instead of a stale entry.
pub async fn publish_one(
    db: &DatabaseConnection,
    id: Uuid,
    actor: Uuid,
) -> Result<apps::Model, AppOpError> {
    let row = Apps::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("publish_one {id} load failed: {e}");
            AppOpError::internal()
        })?
        .ok_or_else(AppOpError::not_found)?;

    let draft_ptr = row.draft_build_id;
    // Promotion gate (validator-can't-be-bypassed): a draft goes live only if its
    // recorded validation status is `passed`. Gate 1 stamps that at publish; a
    // future deploy-time probe (gate 2) may downgrade it. Held at draft otherwise.
    if let Some(ptr) = draft_ptr {
        gate_build_promotion(db, ptr).await?;
    }
    let now = Utc::now().fixed_offset();
    let mut active: apps::ActiveModel = row.into();
    active.published_at = ActiveValue::Set(Some(now));
    active.last_promoted_by = ActiveValue::Set(Some(actor));
    active.last_promoted_at = ActiveValue::Set(Some(now));
    if let Some(ptr) = draft_ptr {
        active.published_build_id = ActiveValue::Set(Some(ptr));
    }
    active.updated_at = ActiveValue::Set(now);
    let updated = active.update(db).await.map_err(|e| {
        tracing::error!("publish_one {id} update failed: {e}");
        AppOpError::internal()
    })?;

    // Per-app cache only — the global access cache is invalidated ONCE by the
    // caller (a batch would otherwise do N full global invalidations).
    crate::server::api::custom_apps_cache::invalidate_cached_canonical_dir_all_channels(id);
    // The serve path caches the `apps` row itself (channel pointers,
    // `published_at`), so it must be dropped here too or this mutation takes
    // up to the cache TTL to appear.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();
    Ok(updated)
}

/// Core unpublish mutation shared by [`unpublish_app`] and
/// [`batch_unpublish_apps`]. Nulls `published_at` + the published channel
/// pointer; the bundle bytes stay untouched.
pub async fn unpublish_one(db: &DatabaseConnection, id: Uuid) -> Result<apps::Model, AppOpError> {
    let row = Apps::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("unpublish_one {id} load failed: {e}");
            AppOpError::internal()
        })?
        .ok_or_else(AppOpError::not_found)?;

    let now = Utc::now().fixed_offset();
    let mut active: apps::ActiveModel = row.into();
    active.published_at = ActiveValue::Set(None);
    active.published_build_id = ActiveValue::Set(None);
    active.updated_at = ActiveValue::Set(now);
    let updated = active.update(db).await.map_err(|e| {
        tracing::error!("unpublish_one {id} update failed: {e}");
        AppOpError::internal()
    })?;

    // Per-app cache only — the global access cache is invalidated ONCE by the
    // caller (a batch would otherwise do N full global invalidations).
    crate::server::api::custom_apps_cache::invalidate_cached_canonical_dir_all_channels(id);
    // The serve path caches the `apps` row itself (channel pointers,
    // `published_at`), so it must be dropped here too or this mutation takes
    // up to the cache TTL to appear.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();
    Ok(updated)
}

/// Core delete shared by [`delete_app`] and [`batch_delete_apps`]. Removes the
/// bundle bytes AND the app's asset silo before the DB row so a partial failure
/// leaves a recoverable orphan row rather than orphan S3 prefixes; either
/// storage failure is logged, never fatal.
pub(super) async fn delete_one(db: &DatabaseConnection, id: Uuid) -> Result<(), AppOpError> {
    let row = Apps::find_by_id(id)
        .one(db)
        .await
        .map_err(|_| AppOpError::internal())?
        .ok_or_else(AppOpError::not_found)?;

    // Refuse BEFORE reclaiming anything if the app has a provisioned OLTP store —
    // symmetric with the rename guard. `ctx.oltp` binds the writer/schema to the
    // slug, so deleting the app would leave `app_<slug>` as residue that a new app
    // reusing the freed slug would adopt (the asset silo below is reclaimed on
    // delete; OLTP rows are the tenant's live data, so dropping them on an app
    // delete is the wrong default — deprovision explicitly). Checked regardless of
    // the kill-switch, since the schema exists either way.
    if let Some(writer) = oxy_oltp::schema::app_writer_name(&row.slug)
        .and_then(|w| oxy_oltp::schema::WriterRef::app(w).ok())
        && oxy_oltp::resolver::writer_is_provisioned(db, row.org_id, &writer)
            .await
            .map_err(|_| AppOpError::internal())?
    {
        tracing::warn!(
            app_id = %id, org_id = %row.org_id, slug = %row.slug,
            "refused app delete: a provisioned OLTP store would be orphaned and its slug freed"
        );
        return Err(AppOpError::has_oltp_store(row.org_id, &writer));
    }

    if let Err(e) = crate::server::api::custom_apps_build_store::delete_app(id).await {
        tracing::warn!(
            "delete_one {id}: bundle bytes could not be removed from build store: {e} \
             — proceeding with DB row delete; reclaim manually if needed"
        );
    }

    // The bundle store holds the app's published JS; the asset store
    // (`ctx.storage`) holds its uploaded/generated files under a separate prefix.
    // Reclaim both, or a deleted app's customer uploads outlive it in S3 (cost +
    // a data-retention concern). Best-effort, same as the build store above.
    if let Err(e) = crate::server::api::custom_apps_storage::delete_app_assets(id).await {
        tracing::warn!(
            "delete_one {id}: asset silo could not be removed from storage: {e} \
             — proceeding with DB row delete; reclaim manually if needed"
        );
    }

    row.delete(db).await.map_err(|_| AppOpError::internal())?;
    // A cached slug→row resolution would keep serving a deleted app until the
    // TTL expired. Drop it before returning.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();
    Ok(())
}

/// Core "promote to latest" shared by [`batch_promote_latest_apps`]: point the
/// published channel at the app's newest build and stamp `published_at`. This
/// is the bulk "roll everyone forward to their latest version" primitive —
/// distinct from `publish_one` (which promotes the *draft* pointer): it always
/// targets the most recently created build regardless of channel. An app with
/// no builds is a per-item failure, not a fatal one.
pub(super) async fn promote_latest_one(
    db: &DatabaseConnection,
    id: Uuid,
    actor: Uuid,
) -> Result<apps::Model, AppOpError> {
    let row = Apps::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("promote_latest_one {id} load failed: {e}");
            AppOpError::internal()
        })?
        .ok_or_else(AppOpError::not_found)?;

    let latest = AppBuilds::find()
        .filter(entity::app_builds::Column::AppId.eq(id))
        .order_by_desc(entity::app_builds::Column::CreatedAt)
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("promote_latest_one {id} build lookup failed: {e}");
            AppOpError::internal()
        })?
        .ok_or_else(|| AppOpError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: "No builds to promote.".into(),
        })?;

    // Promotion gate (validator-can't-be-bypassed): the newest build goes live
    // only if its recorded validation is `passed`. This is exactly where a
    // freshly-uploaded-then-failed build would sit once gate 2 can downgrade a
    // stored build, so promote-latest must not skip the check.
    gate_build_promotion(db, latest.id).await?;

    let now = Utc::now().fixed_offset();
    let mut active: apps::ActiveModel = row.into();
    active.published_build_id = ActiveValue::Set(Some(latest.id));
    active.published_at = ActiveValue::Set(Some(now));
    active.last_promoted_by = ActiveValue::Set(Some(actor));
    active.last_promoted_at = ActiveValue::Set(Some(now));
    active.updated_at = ActiveValue::Set(now);
    let updated = active.update(db).await.map_err(|e| {
        tracing::error!("promote_latest_one {id} update failed: {e}");
        AppOpError::internal()
    })?;

    // Per-app cache only — the global access cache is invalidated ONCE by the
    // caller (a batch would otherwise do N full global invalidations).
    crate::server::api::custom_apps_cache::invalidate_cached_canonical_dir_all_channels(id);
    // The serve path caches the `apps` row itself (channel pointers,
    // `published_at`), so it must be dropped here too or this mutation takes
    // up to the cache TTL to appear.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();
    Ok(updated)
}

/// Upper bound on ids accepted by a batch endpoint. The admin surface is
/// small-scale; this only rejects a pathological request, it is not a paging
/// limit.
pub(super) const MAX_BATCH_IDS: usize = 500;

/// Reject empty or oversized batches before touching the DB.
pub(super) fn validate_batch(ids: &[Uuid]) -> Result<(), ApiErr> {
    if ids.is_empty() {
        return Err(api_err(StatusCode::BAD_REQUEST, "No apps selected."));
    }
    if ids.len() > MAX_BATCH_IDS {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            format!("Too many apps in one request (max {MAX_BATCH_IDS})."),
        ));
    }
    Ok(())
}

/// Resolve `<org_slug>/<app_slug>` → (org row, app row). Returns `Ok(None)`
/// if either lookup misses; `Err` only on a real DB failure.
/// Resolve `<org>/<app>` → (org row, app row). The `org` segment can be
/// either a slug or a UUID — auto-detected on parse, mirroring the
/// publish-side `OrgRef::from_str_auto`. Lets `oxyc publish --org <uuid>`
/// reach `build-config` without falling over on the slug-only lookup
/// the route used to do; the customer-facing `/customer-apps/<org>/<app>/`
/// URLs are unaffected because they only ever carry slugs (UUIDs in
/// browser URLs would be ugly and the serve dispatcher passes slugs
/// straight through).
pub(crate) async fn lookup_by_pretty_path(
    db: &DatabaseConnection,
    org_segment: &str,
    app_slug: &str,
) -> Result<Option<(organizations::Model, apps::Model)>, StatusCode> {
    let org = match Uuid::parse_str(org_segment) {
        Ok(id) => Organizations::find_by_id(id).one(db).await,
        Err(_) => {
            Organizations::find()
                .filter(organizations::Column::Slug.eq(org_segment))
                .one(db)
                .await
        }
    }
    .map_err(|e| {
        tracing::error!("Org lookup failed for {org_segment}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let Some(org) = org else {
        return Ok(None);
    };
    let app = Apps::find()
        .filter(apps::Column::OrgId.eq(org.id))
        .filter(apps::Column::Slug.eq(app_slug))
        .one(db)
        .await
        .map_err(|e| {
            tracing::error!("App lookup failed for {org_segment}/{app_slug}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(app.map(|a| (org, a)))
}

/// Validate that a caller-supplied slug fits the URL-safe shape we
/// auto-derive in [`slugify`]: lowercase ASCII letters/digits/dashes,
/// 1–63 chars, no leading/trailing dash, no consecutive dashes.
pub(crate) fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub(super) async fn slug_taken_in_org(
    db: &DatabaseConnection,
    org_id: Uuid,
    slug: &str,
) -> Result<bool, ApiErr> {
    Apps::find()
        .filter(apps::Column::OrgId.eq(org_id))
        .filter(apps::Column::Slug.eq(slug))
        .one(db)
        .await
        .map(|opt| opt.is_some())
        .map_err(|e| {
            tracing::error!("Slug uniqueness check failed for {org_id}/{slug}: {e}");
            internal(e)
        })
}

/// Refuse to point an app at a workspace outside its own org — on create
/// (`create_app_unscoped`) and on a move (`update_app`).
///
/// `apps.project_id` is what `window.__OXY_APP__.projectId` is injected from, and
/// the connectors, secrets and data plane the bundle reaches all resolve against
/// it — so a row naming another org's workspace runs this org's app on that
/// tenant's data. This is a data-integrity rule, not an access decision: no
/// caller, however privileged, may write that row. Same rule as
/// `custom_apps_publish::validate_project`, and literally the same predicate:
/// both go through [`crate::server::api::workspace_org`], so the two cannot
/// drift apart on what counts as in-org.
///
/// A missing workspace, an orphaned one (no `org_id`) and one in another org all
/// get the same **422** and the same body. Telling them apart would tell a
/// platform grant bounded to this org that an id exists in some other tenant —
/// the probe `app_scope_guard` answers indistinguishably everywhere else. The
/// log line records which it was.
pub(super) async fn ensure_workspace_in_org(
    db: &DatabaseConnection,
    workspace_id: Uuid,
    org_id: Uuid,
) -> Result<(), ApiErr> {
    let matched = workspace_in_org(db, workspace_id, org_id)
        .await
        .map_err(|e| {
            tracing::error!("Workspace lookup failed for {workspace_id}: {e}");
            internal(e)
        })?;
    let reason = match matched {
        WorkspaceOrgMatch::InOrg => return Ok(()),
        WorkspaceOrgMatch::Orphaned => "workspace has no org",
        WorkspaceOrgMatch::OtherOrg => "workspace belongs to another org",
        WorkspaceOrgMatch::Missing => "workspace does not exist",
    };
    tracing::warn!(
        workspace_id = %workspace_id, org_id = %org_id, reason,
        "refused to point an app at a workspace outside its org"
    );
    Err(api_err(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!(
            "Workspace {workspace_id} is not part of this app's organization. \
             An app can only use a workspace in its own organization."
        ),
    ))
}

/// Derive a slug from a human name and dedupe by appending `-2`, `-3`, …
/// until unique within the org. Tries up to 50 suffixes before giving up;
/// the realistic ceiling for any one org is far below that.
pub(super) async fn unique_slug_for_name(
    db: &DatabaseConnection,
    org_id: Uuid,
    name: &str,
) -> Result<String, ApiErr> {
    let base = slugify(name);
    // Fetch all candidate-shape slugs in this org with one query, then
    // dedupe in-process. The previous implementation walked up to 50
    // sequential `SELECT 1 FROM apps WHERE slug = ?` round trips for
    // densely-populated orgs; this collapses it to one. We use LIKE
    // 'base%' on the indexed `slug` column so even a large `apps`
    // table only scans the per-base subset.
    //
    // `like_escape` defends against a base whose slug happens to
    // contain `%` or `_`. Slugs are normally restricted to lowercase
    // ASCII letters + digits + `-` (see `is_valid_slug`), so the
    // escape rarely fires — but `slugify` doesn't guarantee shape,
    // and a slug like `a_b` would otherwise match `a*b` and prune
    // valid free numbers from the candidate set.
    let pattern = format!("{}%", like_escape(&base));
    let taken: Vec<String> = Apps::find()
        .filter(apps::Column::OrgId.eq(org_id))
        .filter(apps::Column::Slug.like(&pattern))
        .select_only()
        .column(apps::Column::Slug)
        .into_tuple()
        .all(db)
        .await
        .map_err(|e| {
            tracing::error!("Slug-candidate scan failed for {org_id} base={base:?}: {e}");
            internal(e)
        })?;
    let taken: std::collections::HashSet<&str> = taken.iter().map(|s| s.as_str()).collect();
    if !taken.contains(base.as_str()) {
        return Ok(base);
    }
    for n in 2..=50 {
        let candidate = format!("{base}-{n}");
        if !taken.contains(candidate.as_str()) {
            return Ok(candidate);
        }
    }
    tracing::error!("Could not find unique slug for {name:?} in org {org_id}");
    Err(api_err(
        StatusCode::CONFLICT,
        format!(
            "Couldn't find a free slug for {name:?} in this org \
             (tried '{base}', '{base}-2' through '{base}-50'). \
             Provide a slug explicitly or delete some unused apps."
        ),
    ))
}

/// Escape SQL `LIKE`-pattern metacharacters in a literal value. Slugs
/// almost never need it, but `slugify` doesn't guarantee shape and
/// we'd rather over-match nothing than under-match a candidate.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod oltp_guard_message_tests {
    use super::oltp_store_blocks_message;
    use oxy_oltp::schema::WriterRef;
    use uuid::Uuid;

    /// The guard must name a remedy the reader can actually perform.
    ///
    /// This started as a CLI-only message, and `oxy oltp deprovision --writer`
    /// has no `--env`: it reads `OXY_DATABASE_URL` and acts on the local control
    /// plane. So every operator who hit this guard against a cloud environment
    /// was handed a command that could not reach the environment that refused
    /// them, and the only route that could was the whole-tenant `DELETE /oltp`.
    ///
    /// Asserted rather than reviewed because the failure is invisible: the
    /// message still reads as helpful, and you only discover it is not by
    /// running it and watching it talk to the wrong database.
    #[test]
    fn names_the_http_route_before_the_cli() {
        let org = Uuid::nil();
        let writer = WriterRef::app("store_ops").expect("valid writer");
        let msg = oltp_store_blocks_message("deleted", org, &writer);

        let route = msg
            .find("/oltp/deprovision-writer")
            .expect("must name the per-writer route — it is the only remote-capable remedy");
        assert!(
            msg.contains("app:store_ops"),
            "must name the DERIVED writer, not the raw slug: {msg}"
        );

        // Order matters, not just presence. The CLI is mentioned to say it is
        // local-only; leading with it is what sent people down the dead end.
        let cli = msg
            .find("oxy oltp deprovision")
            .expect("should still mention the CLI");
        assert!(
            route < cli,
            "the route must come before the CLI mention: {msg}"
        );
        assert!(
            msg.contains("no `--env`"),
            "must say WHY the CLI is not the answer remotely: {msg}"
        );
        // The two doors differ: this console is `Action::PlatformApps`, the
        // route is `Action::PlatformOltp`, and an App Operator holds the first
        // and not the second. Without this clause the reader most likely to hit
        // the guard is sent to a route that 403s them.
        assert!(
            msg.contains("platform-OLTP standing"),
            "must name the standing the route requires: {msg}"
        );

        // It must never name the whole-tenant escape hatch: that destroys every
        // other app's schema in the org, which is the opposite of this guard.
        assert!(
            !msg.contains("DELETE /api/admin/orgs"),
            "must not point at the whole-tenant deprovision: {msg}"
        );
    }
}
