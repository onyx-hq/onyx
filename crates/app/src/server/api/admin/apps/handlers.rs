//! Admin endpoints for the customer-apps registry. Gated by oxy_owner_guard
//! at the router layer (mounted under /admin in router/global.rs).

use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use chrono::Utc;
use entity::apps;
use entity::org_members;
use entity::prelude::{AppBuilds, Apps, OrgMembers, Organizations, Workspaces};
use oxy::database::client::establish_connection;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue;
use sea_orm::ColumnTrait;
use sea_orm::EntityTrait;
use sea_orm::ModelTrait;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use uuid::Uuid;

use super::dto::*;
use super::ops::*;

pub(crate) use super::dto::{CreateAppRequest, ListAppsQuery};
pub(crate) use super::ops::validate_display_name;
// `pub`: the extracted `oxy-api-partner-console` surface publishes/unpublishes a
// client's app through these.
pub use super::ops::{publish_one, unpublish_one};

/// The org ids a **bounded** platform grant reaches — `None` for unbounded.
///
/// **Fallible on purpose.** An unreadable grant is not "unbounded"; it is *unknown*, and
/// the two callers of this need opposite things from that:
///
/// * a READ (`list_apps`, `oxy-access`) prefers to show rows — the capability gate
///   already admitted this caller, and a blip shouldn't present an empty console as
///   though it were the truth. [`scope_org_filter`] collapses `Err` that way.
/// * a WRITE (`create_app`, the `batch/*` endpoints) must refuse. Treating unknown as
///   unbounded there means one transient `DbErr` turns `batch/delete` into a
///   cross-tenant delete — which the module docs correctly call the worst leak this
///   model could have.
///
/// `app_scope_guard` already fails closed on `Err`. Having the two halves of one system
/// disagree about what an unreadable grant means is how this drifts, so the difference is
/// stated here once rather than re-decided at each call site.
pub(crate) async fn scope_org_filter_checked(
    db: &sea_orm::DatabaseConnection,
    user: &oxy_auth::types::AuthenticatedUser,
) -> Result<Option<Vec<Uuid>>, sea_orm::DbErr> {
    use oxy_authz::Scope;
    // A Global Owner is unbounded by definition and holds no grant row — short-circuit
    // rather than reading one, so an owner who ALSO carries a bounded row (possible when
    // OXY_OWNER and OXY_GLOBAL_ADMINS overlap) isn't narrowed here while every other
    // path says they reach everything. Mirrors `platform_reaches` / `platform_holds`.
    if crate::server::authz::globals::is_global_owner(user.email.as_deref().unwrap_or("")) {
        return Ok(None);
    }
    match crate::server::authz::globals::platform_grant_checked(
        db,
        user.email.as_deref().unwrap_or(""),
    )
    .await?
    {
        Some(grant) => Ok(match &grant.scope {
            Scope::All => None,
            Scope::Orgs(orgs) => Some(orgs.clone()),
        }),
        // No grant row and not an owner: nothing to narrow by. The capability gate
        // decides whether they belong here at all.
        None => Ok(None),
    }
}

/// The lenient read-path filter — see [`scope_org_filter_checked`] for why `Err`
/// collapses to "don't filter" here and nowhere else.
///
/// `Some(vec![])` is a real answer — a grant bounded to nothing — and correctly yields
/// an empty list.
pub(crate) async fn scope_org_filter(
    db: &sea_orm::DatabaseConnection,
    user: &oxy_auth::types::AuthenticatedUser,
) -> Option<Vec<Uuid>> {
    scope_org_filter_checked(db, user).await.unwrap_or_else(|e| {
        tracing::warn!(
            target: "authz",
            error = %e,
            "platform grant unreadable — listing unfiltered rather than showing an empty registry"
        );
        None
    })
}

/// Scope exception #4 (see `app_scope_guard`): the **storage** routes name their target
/// with something other than `{id}` — a `?appId=` query param, an `{org_id}` path segment,
/// or nothing at all (the fleet rollup). The path-based guard sees none of those, so each
/// of those handlers has to say out loud that it checked, which is what this is for.
///
/// Fails CLOSED, unlike [`scope_org_filter`]: these are targeted reads of one named org,
/// so collapsing `Err` to "unbounded" would hand a bounded grant another tenant's figures
/// on a transient `DbErr`. The lenient variant is for *list* endpoints, where the same
/// collapse only over-shows rows the caller could already enumerate.
///
/// `Ok(false)` means out of scope; every caller turns that into **404, not 403** — the
/// rule `app_scope_guard` sets so ids cannot be probed for existence.
pub(crate) async fn org_in_scope(
    db: &sea_orm::DatabaseConnection,
    user: &oxy_auth::types::AuthenticatedUser,
    org_id: Uuid,
) -> Result<bool, StatusCode> {
    match scope_org_filter_checked(db, user).await {
        Ok(None) => Ok(true), // unbounded grant
        Ok(Some(orgs)) => Ok(orgs.contains(&org_id)),
        Err(e) => {
            tracing::error!(
                target: "authz",
                error = %e,
                "platform grant unreadable on a scoped storage read — refusing"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Scope exception #2 (see `app_scope_guard`): **batch** ids travel in the request body,
/// where the path-based guard cannot see them. Without this, `batch/delete` would happily
/// delete apps in every org a bounded grant has no reach into — the single worst leak the
/// scope model could have, because it needs no discovery step: the caller just posts ids.
///
/// Splits the requested ids into the ones this grant reaches and per-item failures for
/// the rest. Out-of-scope ids report the same "not found" an out-of-scope single read
/// gets, so batching cannot be used to probe the registry.
async fn split_by_scope(
    db: &sea_orm::DatabaseConnection,
    user: &oxy_auth::types::AuthenticatedUser,
    ids: Vec<Uuid>,
) -> Result<(Vec<Uuid>, Vec<BatchItemResult>), StatusCode> {
    // Fail CLOSED on an unreadable grant. Collapsing `Err` to "unbounded" here would let
    // a single transient `DbErr` turn `batch/delete` into a cross-tenant delete.
    let orgs = match scope_org_filter_checked(db, user).await {
        Ok(None) => return Ok((ids, Vec::new())), // unbounded grant — nothing to fence
        Ok(Some(orgs)) => orgs,
        Err(e) => {
            tracing::error!(
                target: "authz",
                error = %e,
                "platform grant unreadable on a batch WRITE — refusing"
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // One query for the whole batch: id → org. A failure here must NOT degrade to an
    // empty map — that would read as "no id has a known org", passing every id through
    // the `_ => allowed` arm below and defeating the fence entirely.
    let owning_org: std::collections::HashMap<Uuid, Uuid> = Apps::find()
        .filter(apps::Column::Id.is_in(ids.clone()))
        .all(db)
        .await
        .map_err(|e| {
            tracing::error!("batch scope lookup failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|a| (a.id, a.org_id))
        .collect();

    let mut allowed = Vec::with_capacity(ids.len());
    let mut denied = Vec::new();
    for id in ids {
        // An id with no row is passed THROUGH, not denied: the per-id op returns its own
        // "not found", and answering differently here would tell a bounded operator
        // which unknown ids are real.
        match owning_org.get(&id) {
            Some(org_id) if !orgs.contains(org_id) => {
                denied.push(BatchItemResult::failed(id, "App not found.".to_string()));
            }
            _ => allowed.push(id),
        }
    }
    Ok((allowed, denied))
}

/// Public endpoint — returns the build-time config for an app by pretty
/// path. Read by the customer-apps CI workflow (and `just build`) before
/// running `pnpm build` so no per-app env config has to live in the
/// customer-apps repo.
///
/// Public on purpose: project_id, branch, and the slugs themselves are
/// not secrets — they're already in the URL. The data behind them is
/// gated by org membership at `/api/*`.
pub async fn get_build_config(
    Path((org_slug, app_slug)): Path<(String, String)>,
) -> Result<Json<BuildConfigResponse>, StatusCode> {
    let db = establish_connection()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (org, row) = lookup_by_pretty_path(&db, &org_slug, &app_slug)
        .await?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(BuildConfigResponse {
        project_id: row.project_id,
        branch: row.branch,
        org_slug: org.slug,
        app_slug: row.slug,
    }))
}

/// Public endpoint — resolve the org slug for a workspace (project) id. Lets
/// `oxyc publish --project <uuid>` build from source without a hardcoded
/// `orgSlug`: a workspace belongs to exactly one org, so the org — and thus the
/// `/customer-apps/<org>/<app>/` base path — is inferred from the pinned
/// project. Public for the same reason as `get_build_config`: a project UUID and
/// its org slug already appear in URLs; the data behind them is gated by org
/// membership at `/api/*`.
pub async fn get_org_for_project(
    Path(project_id): Path<Uuid>,
) -> Result<Json<OrgForProjectResponse>, StatusCode> {
    let db = establish_connection()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let ws = Workspaces::find_by_id(project_id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let org_id = ws.org_id.ok_or(StatusCode::NOT_FOUND)?;
    let org = Organizations::find_by_id(org_id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(OrgForProjectResponse {
        project_id,
        org_slug: org.slug,
    }))
}

pub async fn create_app(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(req): Json<CreateAppRequest>,
) -> Result<Json<AppResponse>, ApiErr> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        internal(e)
    })?;

    // Scope exception #1 (see `app_scope_guard`): the target org arrives in the BODY, so
    // the path-based guard can't see it. Without this a grant bounded to org A could
    // register an app in org B and then reach it legitimately ever after — scope would
    // be bypassable by creating your way in.
    crate::server::api::admin::scope::deny_out_of_scope(&db, &user, req.org_id)
        .await
        .map_err(|s| api_err(s, "Organization not found."))?;

    create_app_unscoped(Json(req)).await
}

/// Registration, once [`create_app`] has checked scope. Private so the handler
/// is the only way in — it was `pub` for the removed `oxy apps create`, which made
/// it a second, unchecked door.
async fn create_app_unscoped(
    Json(req): Json<CreateAppRequest>,
) -> Result<Json<AppResponse>, ApiErr> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        internal(e)
    })?;

    // Validate the template_id up-front before any side effects so we
    // return 400 immediately for unknown ids without touching the DB.
    let template_id = validate_template_id(req.template_id.as_deref())
        .map_err(|msg| api_err(StatusCode::BAD_REQUEST, msg))?;

    // Validate the display name: rejects chars that would break the
    // scaffolded JSON / JSX so a poisonous name can't produce a
    // bundle that fails the probe (or worse) after we open the PR.
    validate_display_name(&req.name).map_err(|msg| api_err(StatusCode::BAD_REQUEST, msg))?;

    // Resolve org first — we need org.slug for the URL we return.
    let org = Organizations::find_by_id(req.org_id)
        .one(&db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to look up org {}: {e}", req.org_id);
            internal(e)
        })?
        .ok_or_else(|| {
            api_err(
                StatusCode::NOT_FOUND,
                format!("Organization {} not found.", req.org_id),
            )
        })?;

    // The workspace must belong to the org the row is created in: the bundle runs
    // against whatever workspace `project_id` names. Same rule `update_app` holds a
    // move to, checked with the rest of the validation so a refusal inserts no row
    // and opens no scaffold PR.
    ensure_workspace_in_org(&db, req.project_id, req.org_id).await?;

    // Slug: caller-supplied wins, but we still validate + collision-check.
    // No caller slug? Auto-derive from name and dedupe with `-2`, `-3`, …
    // until unique within this org.
    let slug = match req.slug.as_deref() {
        Some(s) => {
            let s = s.trim();
            if !is_valid_slug(s) {
                return Err(api_err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "Slug {s:?} isn't valid. Use lowercase letters, digits, and \
                         single hyphens; 1–63 chars; no leading/trailing/double hyphens."
                    ),
                ));
            }
            if slug_taken_in_org(&db, req.org_id, s).await? {
                return Err(api_err(
                    StatusCode::CONFLICT,
                    format!(
                        "An app with slug {s:?} already exists in org {:?}. \
                         The slug is locked to the bundle's baked OXY_APP_BASE_PATH, \
                         so the existing app needs to be deleted (or renamed) before \
                         you can link this bundle.",
                        org.slug
                    ),
                ));
            }
            s.to_string()
        }
        None => unique_slug_for_name(&db, req.org_id, &req.name).await?,
    };

    let id = Uuid::new_v4();
    let now = Utc::now().fixed_offset();
    let (source_type, source_config) = req.source.into_columns();

    // `repo_path` is the stable cross-env identifier for S3-sourced
    // bundles. Operator-overridable; defaults to the row's
    // `<org_slug>/<slug>` pair so the common case (admin row name
    // matches the repo path) requires no extra input. For non-S3
    // sources we record None — those source types don't read from S3.
    let repo_path: Option<String> = match source_type.as_str() {
        "s3" => Some(
            req.repo_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.trim_matches('/').to_string())
                .unwrap_or_else(|| format!("{}/{slug}", org.slug)),
        ),
        _ => None,
    };

    // Only S3 apps have a draft/published bundle separation worth
    // gating on — Local and V0 are essentially "the engineer already
    // controls the bundle", so we auto-publish them on create. That
    // way they show up in the customer sidebar immediately, and the
    // explicit Publish button is a no-op for them (a sidebar
    // visibility toggle, not a deploy step).
    let initial_published_at = match source_type.as_str() {
        "s3" => ActiveValue::NotSet,
        _ => ActiveValue::Set(Some(now)),
    };

    let model = apps::ActiveModel {
        // Leave to the DB default ('org'): a new app is org-visible unless
        // explicitly restricted later.
        visibility: sea_orm::ActiveValue::NotSet,
        id: ActiveValue::Set(id),
        slug: ActiveValue::Set(slug),
        name: ActiveValue::Set(req.name),
        org_id: ActiveValue::Set(req.org_id),
        project_id: ActiveValue::Set(req.project_id),
        branch: ActiveValue::Set(req.branch),
        source_repo: ActiveValue::Set("oxy-hq/customer-apps".to_string()),
        status: ActiveValue::Set("created".to_string()),
        source_type: ActiveValue::Set(source_type),
        source_config: ActiveValue::Set(source_config),
        bootstrap_pr_url: ActiveValue::NotSet,
        last_synced_at: ActiveValue::NotSet,
        // No per-deployment override on create — defaults to "use the
        // bundle's bundled oxy-app.json."
        manifest_override: ActiveValue::NotSet,
        published_at: initial_published_at,
        repo_path: ActiveValue::Set(repo_path),
        draft_build_id: ActiveValue::NotSet,
        published_build_id: ActiveValue::NotSet,
        last_promoted_by: ActiveValue::NotSet,
        last_promoted_at: ActiveValue::NotSet,
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    };

    let inserted = model.insert(&db).await.map_err(|e| {
        // Defensive 409 — the in-process dedup above should have caught any
        // collision, but a race between two concurrent admins could still
        // hit the unique index.
        if oxy::database::errors::is_unique_violation(&e) {
            return api_err(
                StatusCode::CONFLICT,
                "Another admin just created an app with this slug. Retry.",
            );
        }
        tracing::error!("Failed to insert app: {e}");
        internal(e)
    })?;

    let mut row = inserted;

    // Local-source provisioning: mkdir the engineer's empty folder
    // under the state dir and update the row's source_config.path so
    // the serve handler can find it. Failure rolls back the row so
    // we never leave behind an app whose configured path doesn't
    // exist on disk (the silent-loading-iframe class of bug).
    if req.provision_local_source && row.source_type == "local" {
        match provision_local_dir_for(row.id).await {
            Ok(path) => {
                let active = apps::ActiveModel {
                    id: ActiveValue::Unchanged(row.id),
                    source_config: ActiveValue::Set(serde_json::json!({
                        "path": path.display().to_string(),
                    })),
                    updated_at: ActiveValue::Set(Utc::now().fixed_offset()),
                    ..Default::default()
                };
                match active.update(&db).await {
                    Ok(updated) => {
                        row = updated;
                    }
                    Err(e) => {
                        tracing::error!("Failed to persist provisioned local path: {e}");
                        // Best-effort cleanup of the freshly-created dir
                        // before we drop the row — leaves no debris.
                        let _ = tokio::fs::remove_dir_all(&path).await;
                        let _ = row.clone().delete(&db).await;
                        return Err(internal(e));
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    "Local-source provisioning failed for {}: {:?}; rolling back app row",
                    row.id,
                    err.1
                );
                let _ = row.clone().delete(&db).await;
                return Err(err);
            }
        }
    }

    // If the caller asked for a scaffold PR and the source is s3, open one
    // synchronously. Failure rolls the row back so we never persist an app
    // whose caller asked for a PR and didn't get one — the only state that
    // exists post-handler is the state the response reflects.
    if req.scaffold_pr && row.source_type == "s3" {
        match crate::server::api::custom_apps_scaffold::scaffold_pr(&db, &row, &org, template_id)
            .await
        {
            Ok(pr_url) => {
                let active = apps::ActiveModel {
                    id: ActiveValue::Unchanged(row.id),
                    bootstrap_pr_url: ActiveValue::Set(Some(pr_url.clone())),
                    updated_at: ActiveValue::Set(Utc::now().fixed_offset()),
                    ..Default::default()
                };
                row = active.update(&db).await.map_err(|e| {
                    tracing::error!("Failed to persist bootstrap_pr_url: {e}");
                    internal(e)
                })?;
            }
            Err(err) => {
                tracing::error!(
                    "PR scaffold failed for {}: {err}; rolling back app row",
                    row.id
                );
                let _ = row.clone().delete(&db).await;
                return Err(api_err(
                    StatusCode::BAD_GATEWAY,
                    format!("Couldn't open scaffold PR: {err}"),
                ));
            }
        }
    }

    let warnings =
        validate_local_source(&row.source_type, &row.source_config, &org.slug, &row.slug);
    let mut resp = AppResponse::from_model_with_org(row, &org.slug);
    resp.warnings = warnings;
    Ok(Json(resp))
}

/// Ceiling on `list_apps` page size — bounds the per-page batched lookups
/// (org slugs, promoter emails, last-active, manifests) so a caller-supplied
/// `?limit=` can't turn one admin request into an unbounded scan.
const MAX_LIST_LIMIT: u64 = 200;

pub async fn list_apps(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Query(q): Query<ListAppsQuery>,
) -> Result<Json<ListAppsResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let scope = scope_org_filter(&db, &user).await;
    list_apps_scoped(q, scope).await
}

/// The registry list, with an explicit org filter — `None` for a grant with no scope.
///
/// Split from [`list_apps`] so the filter can be exercised without an HTTP principal:
/// handing a test a synthetic `AuthenticatedUser` to satisfy the extractor would
/// fabricate a principal, and fabricated principals are how authorization models start
/// lying.
pub async fn list_apps_scoped(
    q: ListAppsQuery,
    scope: Option<Vec<Uuid>>,
) -> Result<Json<ListAppsResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // Clamp the caller-supplied page size — the per-page batched lookups below
    // scale with it, so an unbounded `?limit=` shouldn't be operator-tunable.
    let limit = q.limit.clamp(1, MAX_LIST_LIMIT);
    // Sort by `updated_at` DESC so the most recently touched apps
    // (whether that's a sync, a config edit, or a publish) sit at the
    // top of the page. Pairs with the frontend's `useInfiniteQuery`
    // so "load more" walks back in time from the most recent.
    //
    // **This is where scope lives.** `platform_cap_guard` proved the caller may use
    // this section; it deliberately did not check scope, because the platform resource
    // has no org to check against. A bounded grant is enforced here, as a row filter —
    // capabilities gate verbs, scope filters rows. Applied BEFORE the limit/offset so
    // paging walks the caller's own registry rather than paging a global list and
    // discarding most of it.
    let mut query = Apps::find().order_by_desc(apps::Column::UpdatedAt);
    if let Some(orgs) = scope {
        query = query.filter(apps::Column::OrgId.is_in(orgs));
    }
    let rows = query
        .limit(Some(limit))
        .offset(q.offset)
        .all(&db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to list apps: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let returned = rows.len() as u64;
    let org_slugs = org_slugs_for(&db, &rows).await.map_err(|e| {
        tracing::error!("Failed to load org slugs for apps list: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // Batch-load last-active timestamps for the page (single query;
    // N+1 would be brutal on an org with 100+ apps). Failures fall
    // back to `None` — the list page should never 500 because the
    // tracking table is unavailable.
    let app_ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let last_active =
        crate::server::api::custom_apps_activity::last_active_at_by_app(&db, &app_ids)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("last_active_at_by_app failed (filling Nones): {e}");
                Default::default()
            });
    // Map each app → its last promoter, then resolve those user ids to emails
    // in one query (same N+1-avoidance as last_active). A failed lookup falls
    // back to no attribution rather than 500ing the list.
    let promoter_by_app: std::collections::HashMap<Uuid, Uuid> = rows
        .iter()
        .filter_map(|r| r.last_promoted_by.map(|p| (r.id, p)))
        .collect();
    let promoter_emails = emails_by_user_id(&db, promoter_by_app.values().copied().collect())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("promoter email lookup failed (no attribution): {e}");
            Default::default()
        });
    // Which apps are serving a build with no traceable source. Same batched
    // shape, same fail-soft posture: an operator losing the warning is far
    // better than the list 500ing over it.
    let unsourced = unsourced_active_build_apps(&db, &rows)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("unsourced_active_build_apps failed (no warnings shown): {e}");
            Default::default()
        });
    // Manifest-derived icon/art for the whole page in ONE batched query — same
    // N+1-avoidance as the promoter/last-active lookups above.
    let mut icon_art = icon_art_by_app(&db, &rows, &org_slugs).await;
    let mut items = rows_to_responses(rows, &org_slugs);
    for item in items.iter_mut() {
        item.source_unrecorded = unsourced.contains(&item.id);
        if let Some(ts) = last_active.get(&item.id) {
            item.last_active_at = Some(ts.to_rfc3339());
        }
        item.last_promoted_by_email = promoter_by_app
            .get(&item.id)
            .and_then(|pid| promoter_emails.get(pid).cloned());
        if let Some((icon, art)) = icon_art.remove(&item.id) {
            item.icon_url = icon;
            item.art_url = art;
        }
    }
    // `next_offset` only exists when the page came back full — short
    // pages are the tail of the dataset and signal "stop fetching" to
    // the client without a separate `total` count query.
    let next_offset = if returned >= limit {
        Some(q.offset + returned)
    } else {
        None
    };
    Ok(Json(ListAppsResponse { items, next_offset }))
}

pub async fn list_my_apps(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
) -> Result<Json<Vec<AppResponse>>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let memberships = OrgMembers::find()
        .filter(org_members::Column::UserId.eq(user.id))
        .all(&db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query memberships: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let org_ids: Vec<Uuid> = memberships.iter().map(|m| m.org_id).collect();
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }

    // Customer-facing endpoint: only show published apps. Drafts live
    // in /admin/apps for staff.
    let rows = Apps::find()
        .filter(apps::Column::OrgId.is_in(org_ids))
        .filter(apps::Column::PublishedAt.is_not_null())
        .all(&db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query apps: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let org_slugs = org_slugs_for(&db, &rows).await.map_err(|e| {
        tracing::error!("Failed to load org slugs for apps/mine: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // Manifest-derived icon/art in ONE batched query (no N+1).
    let mut icon_art = icon_art_by_app(&db, &rows, &org_slugs).await;
    let mut items = rows_to_responses(rows, &org_slugs);
    for item in items.iter_mut() {
        if let Some((icon, art)) = icon_art.remove(&item.id) {
            item.icon_url = icon;
            item.art_url = art;
        }
    }
    Ok(Json(items))
}

pub async fn get_app(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
) -> Result<Json<AppResponse>, StatusCode> {
    let db = establish_connection()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let row = Apps::find_by_id(id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    crate::server::api::admin::scope::deny_out_of_scope(&db, &user, row.org_id).await?;
    let org = Organizations::find_by_id(row.org_id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let manifests = crate::server::api::custom_apps_manifest::resolve_manifests_batch(
        &db,
        std::slice::from_ref(&row),
    )
    .await;
    let (icon_url, art_url) = crate::server::api::workspace_custom_apps::icon_art_urls(
        manifests.get(&row.id),
        &org.slug,
        &row.slug,
        row.published_build_id,
    );
    let mut resp = AppResponse::from_model_with_org(row, &org.slug);
    resp.icon_url = icon_url;
    resp.art_url = art_url;
    Ok(Json(resp))
}

/// The error the rename guard returns when an app has a provisioned OLTP store.
/// Carries a body (via [`ApiErr`]) so the operator sees the actual instruction —
/// a bare 409 is indistinguishable from "slug already taken", and the action it
/// needs (per-writer deprovision) lives in another console. Shares its prose with
/// the delete guard via [`oltp_store_blocks_message`] so the two can't drift.
fn oltp_store_blocks(action: &str, org_id: Uuid, writer: &oxy_oltp::schema::WriterRef) -> ApiErr {
    api_err(
        StatusCode::CONFLICT,
        oltp_store_blocks_message(action, org_id, writer),
    )
}

pub async fn update_app(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateAppRequest>,
) -> Result<Json<AppResponse>, ApiErr> {
    let db = establish_connection().await.map_err(internal)?;
    let existing = Apps::find_by_id(id)
        .one(&db)
        .await
        .map_err(internal)?
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, "App not found."))?;
    let org_id = existing.org_id;
    crate::server::api::admin::scope::deny_out_of_scope(&db, &user, org_id)
        .await
        // Status-aware so a 500 (unreadable grant) doesn't read as "App not
        // found." The 404 IS the intended scope-hiding answer for out-of-scope.
        .map_err(|s| {
            let msg = if s == StatusCode::NOT_FOUND {
                "App not found."
            } else {
                "Could not verify access to this app."
            };
            api_err(s, msg)
        })?;

    if let Some(slug) = req.slug.as_deref() {
        let slug = slug.trim();
        if !is_valid_slug(slug) {
            return Err(api_err(
                StatusCode::BAD_REQUEST,
                "Invalid slug: use 1–63 lowercase letters, digits, or hyphens.",
            ));
        }
        if slug != existing.slug && slug_taken_in_org(&db, org_id, slug).await? {
            return Err(api_err(
                StatusCode::CONFLICT,
                "A different app in this org already uses that slug.",
            ));
        }
        // Refuse a rename that would strand a provisioned OLTP store. `ctx.oltp`
        // derives its writer from the app's slug (`app_writer_name`), so changing
        // the slug would (a) orphan the existing `app_<oldslug>` schema — its rows
        // become unreachable — and (b) free the old slug for another app to claim,
        // which would then resolve the SAME schema: the cross-app reach the
        // derivation exists to prevent, reopened by two ordinary admin edits. The
        // store must be deprovisioned before the app can be renamed. Checked
        // regardless of the kill-switch, since the schema exists either way. This
        // 409 shares its status with the slug-taken one above but carries a
        // distinct body, so the operator gets the deprovision instruction rather
        // than concluding the slug is taken.
        if slug != existing.slug
            && let Some(writer) = oxy_oltp::schema::app_writer_name(&existing.slug)
                .and_then(|w| oxy_oltp::schema::WriterRef::app(w).ok())
            && oxy_oltp::resolver::writer_is_provisioned(&db, org_id, &writer)
                .await
                .map_err(internal)?
        {
            tracing::warn!(
                app_id = %id, org_id = %org_id, old_slug = %existing.slug, new_slug = %slug,
                "refused app rename: a provisioned OLTP store would be orphaned and its slug freed"
            );
            return Err(oltp_store_blocks("renamed", org_id, &writer));
        }
    }

    // A move must stay inside the app's own org: the bundle runs against whatever
    // workspace this row names. Only an actual move is checked, so re-sending the
    // current `project_id` still saves the rest of the edit — including for an app
    // whose workspace has since been deleted.
    if let Some(pid) = req.project_id
        && pid != existing.project_id
    {
        ensure_workspace_in_org(&db, pid, org_id).await?;
    }

    let mut active: apps::ActiveModel = existing.into();
    if let Some(n) = req.name {
        validate_display_name(&n).map_err(|msg| api_err(StatusCode::BAD_REQUEST, msg))?;
        active.name = ActiveValue::Set(n);
    }
    if let Some(s) = req.slug {
        active.slug = ActiveValue::Set(s.trim().to_string());
    }
    if let Some(pid) = req.project_id {
        active.project_id = ActiveValue::Set(pid);
    }
    if let Some(b) = req.branch {
        active.branch = ActiveValue::Set(b);
    }
    if let Some(s) = req.status {
        active.status = ActiveValue::Set(s);
    }
    if let Some(source) = req.source {
        let (source_type, source_config) = source.into_columns();
        active.source_type = ActiveValue::Set(source_type);
        active.source_config = ActiveValue::Set(source_config);
    }
    active.updated_at = ActiveValue::Set(Utc::now().fixed_offset());
    let updated = active.update(&db).await.map_err(|e| {
        if oxy::database::errors::is_unique_violation(&e) {
            return api_err(
                StatusCode::CONFLICT,
                "A different app in this org already uses that slug.",
            );
        }
        tracing::error!("Failed to update app {id}: {e}");
        internal(e)
    })?;

    // This handler can change the **slug**, which is the serve path's cache
    // KEY, as well as source/branch/project. Without this the old slug keeps
    // resolving (and the new one 404s) until the TTL expires.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();

    let org = Organizations::find_by_id(org_id)
        .one(&db)
        .await
        .map_err(internal)?
        .ok_or_else(|| api_err(StatusCode::INTERNAL_SERVER_ERROR, "Organization not found."))?;
    let warnings = validate_local_source(
        &updated.source_type,
        &updated.source_config,
        &org.slug,
        &updated.slug,
    );
    let mut resp = AppResponse::from_model_with_org(updated, &org.slug);
    resp.warnings = warnings;
    Ok(Json(resp))
}

/// Publish:
///
/// - **S3 source**: server-side copy `apps/<org>/<slug>/draft/*` →
///   `apps/<org>/<slug>/published/*`, then sync the `published`
///   channel into the local state-dir. This is what isolates the
///   customer view from the engineer's draft.
/// - **Local / V0 source**: there's no bundle channel to promote —
///   just stamp `published_at`. Publishing is purely a sidebar
///   visibility toggle for these.
///
/// In all cases stamp `published_at = now()` so the customer-facing
/// auth gate flips and the workspace sidebar picks up the entry.
pub async fn publish_app(
    oxy_auth::extractor::AuthenticatedUserExtractor(user): oxy_auth::extractor::AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
) -> Result<Json<AppResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("publish_app DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let updated = publish_one(&db, id, user.id).await.map_err(|e| e.status)?;
    crate::server::api::custom_apps_auth::invalidate_access_cache();
    let org = load_org(&db, updated.org_id).await.map_err(|e| e.status)?;

    // The SAME action taken by a partner was audited and by Oxy staff was not, so
    // the trail recorded the delegated tier and was blind to the privileged one —
    // backwards. Publishing puts an app in front of a customer's users; who did it
    // is exactly the question an incident asks first.
    oxy_app_core::audit::record_best_effort(
        &db,
        oxy_app_core::audit::AuditEntry::new(user.label().to_string(), "app.published")
            .actor(user.id, oxy_app_core::audit::ActorType::User)
            .org(updated.org_id)
            .target("app", updated.id.to_string(), updated.name.clone()),
    )
    .await;

    Ok(Json(AppResponse::from_model_with_org(updated, &org.slug)))
}

/// Unpublish: null out `published_at`. Non-app-admins lose access on
/// next request; the bundle itself stays untouched.
pub async fn unpublish_app(
    oxy_auth::extractor::AuthenticatedUserExtractor(user): oxy_auth::extractor::AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
) -> Result<Json<AppResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("unpublish_app DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let updated = unpublish_one(&db, id).await.map_err(|e| e.status)?;
    crate::server::api::custom_apps_auth::invalidate_access_cache();
    let org = load_org(&db, updated.org_id).await.map_err(|e| e.status)?;

    // Taking a customer's app DOWN is at least as auditable as putting it up.
    oxy_app_core::audit::record_best_effort(
        &db,
        oxy_app_core::audit::AuditEntry::new(user.label().to_string(), "app.unpublished")
            .actor(user.id, oxy_app_core::audit::ActorType::User)
            .org(updated.org_id)
            .target("app", updated.id.to_string(), updated.name.clone()),
    )
    .await;

    Ok(Json(AppResponse::from_model_with_org(updated, &org.slug)))
}

/// `POST /admin/apps/{id}/functions/{name}/runs` — trigger a one-off background
/// run of a custom-app Oxy Function as a job (the manual "run now" that isn't
/// tied to a cron schedule). An optional JSON request body is handed to the
/// function as its `req` input params (same shape a route invocation receives);
/// an empty body runs it with no params. Enqueues a durable task on the global
/// fleet and returns its `run_id`; the caller watches it in the orchestrator
/// dashboard. Thin transport: parse input → enqueue → serialize (the work is in
/// `custom_apps_functions::trigger_function_job`).
pub async fn run_function_job(
    oxy_auth::extractor::AuthenticatedUserExtractor(_user): oxy_auth::extractor::AuthenticatedUserExtractor,
    Path((id, name)): Path<(Uuid, String)>,
    body: axum::body::Bytes,
) -> Result<Json<RunFunctionJobResponse>, StatusCode> {
    // Empty body → no params; a non-empty body must be valid JSON.
    let input = if body.is_empty() {
        None
    } else {
        Some(
            serde_json::from_slice::<serde_json::Value>(&body)
                .map_err(|_| StatusCode::BAD_REQUEST)?,
        )
    };
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("run_function_job DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let run_id = crate::server::api::custom_apps_functions::trigger_function_job(
        &db,
        id,
        &name,
        input,
        crate::server::api::custom_apps_functions::FunctionJobTrigger::Manual,
    )
    .await
    .map_err(|e| {
        tracing::warn!("run_function_job failed for {id}/{name}: {e}");
        StatusCode::BAD_REQUEST
    })?;
    Ok(Json(RunFunctionJobResponse { run_id }))
}

/// `GET /api/customer-apps/{id}/builds` — newest-first build history for
/// the new publish pipeline. Empty for legacy `s3`/local/v0 rows that
/// have never been published via `oxyc publish`.
pub async fn list_builds(Path(id): Path<Uuid>) -> Result<Json<BuildHistoryResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("list_builds DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let app = Apps::find_by_id(id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let builds = AppBuilds::find()
        .filter(entity::app_builds::Column::AppId.eq(id))
        .order_by_desc(entity::app_builds::Column::CreatedAt)
        .all(&db)
        .await
        .map_err(|e| {
            tracing::error!("list_builds query failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    // Resolve emails in a single query for the "who deployed" column: each
    // build's original publisher + whoever last promoted (made live). Missing
    // ids (legacy/NULL) simply render without an email.
    let mut publisher_ids: Vec<Uuid> = builds.iter().filter_map(|b| b.published_by).collect();
    if let Some(promoter) = app.last_promoted_by {
        publisher_ids.push(promoter);
    }
    let emails: std::collections::HashMap<Uuid, String> = if publisher_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        entity::prelude::Users::find()
            .filter(entity::users::Column::Id.is_in(publisher_ids))
            .all(&db)
            .await
            .map_err(|e| {
                tracing::error!("list_builds publisher lookup failed: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .into_iter()
            .map(|u| (u.id, u.label().to_string()))
            .collect()
    };
    let builds_out: Vec<BuildSummary> = builds
        .into_iter()
        .map(|b| BuildSummary {
            is_draft: app.draft_build_id == Some(b.id),
            is_published: app.published_build_id == Some(b.id),
            published_by_email: b.published_by.and_then(|uid| emails.get(&uid).cloned()),
            id: b.id,
            build_id: b.build_id,
            created_at: b.created_at.to_rfc3339(),
            source_repo: b.source_repo,
            commit_sha: b.commit_sha,
            source_branch: b.source_branch,
        })
        .collect();
    Ok(Json(BuildHistoryResponse {
        builds: builds_out,
        promoted_by_email: app
            .last_promoted_by
            .and_then(|uid| emails.get(&uid).cloned()),
        promoted_at: app.last_promoted_at.map(|t| t.to_rfc3339()),
    }))
}

/// `POST /api/customer-apps/{id}/rollback` — repoint the published channel
/// at any retained build. Pure pointer move; the build's bytes are already
/// in S3. Validates the build belongs to this app.
pub async fn rollback_app(
    oxy_auth::extractor::AuthenticatedUserExtractor(user): oxy_auth::extractor::AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
    Json(req): Json<RollbackRequest>,
) -> Result<Json<AppResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("rollback_app DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let row = Apps::find_by_id(id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let build = AppBuilds::find_by_id(req.build_id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if build.app_id != id {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Promotion gate (validator-can't-be-bypassed): a historical build may be
    // rolled back to the live channel only if its recorded validation is
    // `passed` — otherwise rollback would be a way to make a failed build live.
    if build.validation_status != "passed" {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let org = Organizations::find_by_id(row.org_id)
        .one(&db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let now = Utc::now().fixed_offset();
    let mut active: apps::ActiveModel = row.into();
    active.published_build_id = ActiveValue::Set(Some(req.build_id));
    active.published_at = ActiveValue::Set(Some(now));
    active.last_promoted_by = ActiveValue::Set(Some(user.id));
    active.last_promoted_at = ActiveValue::Set(Some(now));
    active.updated_at = ActiveValue::Set(now);
    let updated = active.update(&db).await.map_err(|e| {
        tracing::error!("rollback_app update failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    crate::server::api::custom_apps_auth::invalidate_access_cache();
    crate::server::api::custom_apps_cache::invalidate_cached_canonical_dir_all_channels(id);
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();
    Ok(Json(AppResponse::from_model_with_org(updated, &org.slug)))
}

pub async fn delete_app(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiErr> {
    let db = establish_connection().await.map_err(internal)?;
    // Resolve the app's org BEFORE deleting it, so a bounded grant can't delete an app
    // it cannot see. `delete_one` 404s on a missing id, which is the same answer an
    // out-of-scope id gets.
    let row = Apps::find_by_id(id)
        .one(&db)
        .await
        .map_err(internal)?
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, "App not found."))?;
    crate::server::api::admin::scope::deny_out_of_scope(&db, &user, row.org_id)
        .await
        // Status-aware so a 500 (unreadable grant) doesn't read as "App not
        // found." The 404 IS the intended scope-hiding answer for out-of-scope.
        .map_err(|s| {
            let msg = if s == StatusCode::NOT_FOUND {
                "App not found."
            } else {
                "Could not verify access to this app."
            };
            api_err(s, msg)
        })?;
    delete_app_unscoped(id).await
}

/// Deletion, once [`delete_app`] has checked scope. Private for the same reason
/// as [`create_app_unscoped`].
async fn delete_app_unscoped(id: Uuid) -> Result<StatusCode, ApiErr> {
    let db = establish_connection().await.map_err(internal)?;
    // Surface the AppOpError's message (not just its status) so the OLTP-store
    // refusal reaches the operator with its "deprovision first" instruction.
    delete_one(&db, id)
        .await
        .map_err(|e| api_err(e.status, e.message))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/customer-apps/batch/publish` — publish many apps at once.
pub async fn batch_publish_apps(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(req): Json<BatchIdsRequest>,
) -> Result<Json<BatchResponse>, ApiErr> {
    validate_batch(&req.ids)?;
    let db = establish_connection().await.map_err(internal)?;
    // Scope exception #2 — ids come from the body; see `split_by_scope`.
    let (ids, mut results) = split_by_scope(&db, &user, req.ids)
        .await
        .map_err(|s| api_err(s, "Could not verify grant scope."))?;
    for id in ids {
        results.push(match publish_one(&db, id, user.id).await {
            Ok(_) => BatchItemResult::ok(id),
            Err(e) => BatchItemResult::failed(id, e.message),
        });
    }
    // One global access-cache invalidation for the whole batch (per-app
    // canonical-dir caches are dropped inside publish_one). Skip when nothing
    // changed.
    if results.iter().any(|r| r.ok) {
        crate::server::api::custom_apps_auth::invalidate_access_cache();
    }
    Ok(Json(BatchResponse::from_results(results)))
}

/// `POST /api/customer-apps/batch/promote-latest` — roll many apps forward to
/// their newest build in one call (the bulk "mass promote latest versions"
/// action). Best-effort per id; an app with no builds is reported as a
/// per-item failure.
pub async fn batch_promote_latest_apps(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(req): Json<BatchIdsRequest>,
) -> Result<Json<BatchResponse>, ApiErr> {
    validate_batch(&req.ids)?;
    let db = establish_connection().await.map_err(internal)?;
    // Scope exception #2 — ids come from the body; see `split_by_scope`.
    let (ids, mut results) = split_by_scope(&db, &user, req.ids)
        .await
        .map_err(|s| api_err(s, "Could not verify grant scope."))?;
    for id in ids {
        results.push(match promote_latest_one(&db, id, user.id).await {
            Ok(_) => BatchItemResult::ok(id),
            Err(e) => BatchItemResult::failed(id, e.message),
        });
    }
    if results.iter().any(|r| r.ok) {
        crate::server::api::custom_apps_auth::invalidate_access_cache();
    }
    Ok(Json(BatchResponse::from_results(results)))
}

/// `POST /api/customer-apps/batch/unpublish` — unpublish many apps at once.
pub async fn batch_unpublish_apps(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(req): Json<BatchIdsRequest>,
) -> Result<Json<BatchResponse>, ApiErr> {
    validate_batch(&req.ids)?;
    let db = establish_connection().await.map_err(internal)?;
    // Scope exception #2 — ids come from the body; see `split_by_scope`.
    let (ids, mut results) = split_by_scope(&db, &user, req.ids)
        .await
        .map_err(|s| api_err(s, "Could not verify grant scope."))?;
    for id in ids {
        results.push(match unpublish_one(&db, id).await {
            Ok(_) => BatchItemResult::ok(id),
            Err(e) => BatchItemResult::failed(id, e.message),
        });
    }
    if results.iter().any(|r| r.ok) {
        crate::server::api::custom_apps_auth::invalidate_access_cache();
    }
    Ok(Json(BatchResponse::from_results(results)))
}

/// `POST /api/customer-apps/batch/delete` — delete many app registrations at
/// once. POST (not DELETE) because the id set travels in the request body.
pub async fn batch_delete_apps(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
    Json(req): Json<BatchIdsRequest>,
) -> Result<Json<BatchResponse>, ApiErr> {
    validate_batch(&req.ids)?;
    let db = establish_connection().await.map_err(internal)?;
    // Scope exception #2 — ids come from the body; see `split_by_scope`.
    let (ids, mut results) = split_by_scope(&db, &user, req.ids)
        .await
        .map_err(|s| api_err(s, "Could not verify grant scope."))?;
    for id in ids {
        results.push(match delete_one(&db, id).await {
            Ok(()) => BatchItemResult::ok(id),
            Err(e) => BatchItemResult::failed(id, e.message),
        });
    }
    Ok(Json(BatchResponse::from_results(results)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_template_id_accepts_vite() {
        assert_eq!(validate_template_id(Some("vite")).unwrap(), "vite");
    }

    #[test]
    fn validate_template_id_defaults_to_vite_when_none() {
        assert_eq!(validate_template_id(None).unwrap(), "vite");
    }

    #[test]
    fn validate_template_id_rejects_unknown() {
        assert!(validate_template_id(Some("does-not-exist")).is_err());
    }

    #[test]
    fn validate_display_name_accepts_typical_names() {
        assert!(validate_display_name("Acme Analytics").is_ok());
        assert!(validate_display_name("Store Pulse — v2 (beta)").is_ok());
        assert!(validate_display_name("数据看板").is_ok()); // unicode is fine
    }

    #[test]
    fn validate_display_name_rejects_empty_or_whitespace_only() {
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name("   ").is_err());
    }

    #[test]
    fn validate_display_name_rejects_json_breaking_chars() {
        // Each of these would corrupt the rendered oxy-app.json /
        // package.json scaffolds when blindly substituted.
        assert!(validate_display_name("My \"App\"").is_err()); // double quote
        assert!(validate_display_name("path\\to").is_err()); // backslash
        assert!(validate_display_name("line\nbreak").is_err()); // newline
        assert!(validate_display_name("tab\there").is_err()); // tab
    }

    #[test]
    fn validate_display_name_rejects_jsx_breaking_chars() {
        // The dashboard template's `<h1>{{APP_DISPLAY_NAME}}</h1>`
        // would break (or worse, render unintended markup) if these
        // landed there post-substitution.
        assert!(validate_display_name("<script>").is_err());
        assert!(validate_display_name("name>x").is_err());
        assert!(validate_display_name("name{x").is_err());
        assert!(validate_display_name("name}x").is_err());
    }

    #[test]
    fn validate_display_name_rejects_overlong() {
        let s = "a".repeat(129);
        assert!(validate_display_name(&s).is_err());
        let s = "a".repeat(128);
        assert!(validate_display_name(&s).is_ok());
    }

    #[test]
    fn validate_display_name_counts_chars_not_bytes() {
        // CJK names: 128 chars at 3 bytes each = 384 bytes. The old
        // byte-count check rejected at 43 chars (129 bytes).
        let cjk_128 = "数".repeat(128);
        assert!(validate_display_name(&cjk_128).is_ok());
        let cjk_129 = "数".repeat(129);
        assert!(validate_display_name(&cjk_129).is_err());
    }

    #[test]
    fn is_valid_slug_accepts_lower_kebab() {
        assert!(is_valid_slug("acme"));
        assert!(is_valid_slug("acme-analytics"));
        assert!(is_valid_slug("a"));
        assert!(is_valid_slug("acme-2"));
        assert!(is_valid_slug(&"a".repeat(63)));
    }

    #[test]
    fn is_valid_slug_rejects_bad_shapes() {
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("Acme")); // uppercase
        assert!(!is_valid_slug("acme--analytics")); // consecutive dashes
        assert!(!is_valid_slug("-acme")); // leading dash
        assert!(!is_valid_slug("acme-")); // trailing dash
        assert!(!is_valid_slug("acme.analytics")); // dot
        assert!(!is_valid_slug("acme/x")); // slash
        assert!(!is_valid_slug(&"a".repeat(64))); // too long
    }

    #[test]
    fn batch_response_counts_ok_and_failed() {
        let resp = BatchResponse::from_results(vec![
            BatchItemResult::ok(Uuid::nil()),
            BatchItemResult::failed(Uuid::nil(), "App not found.".into()),
            BatchItemResult::ok(Uuid::nil()),
        ]);
        assert_eq!(resp.succeeded, 2);
        assert_eq!(resp.failed, 1);
        assert_eq!(resp.results.len(), 3);
    }

    #[test]
    fn validate_batch_rejects_empty_and_oversized() {
        assert!(validate_batch(&[]).is_err());
        let too_many = vec![Uuid::nil(); MAX_BATCH_IDS + 1];
        assert!(validate_batch(&too_many).is_err());
        assert!(validate_batch(&[Uuid::nil()]).is_ok());
    }
}
