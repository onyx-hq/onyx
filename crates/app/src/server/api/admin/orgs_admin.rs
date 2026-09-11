//! `/api/admin/orgs/*` — staff meta surface for organizations.
//!
//! Adds to the existing `/admin/orgs` listing endpoint (defined in
//! `admin::billing`) the bits the admin UI needs to provision and operate on a
//! single organization: create (+ onboard its owner), detail view, rename,
//! transfer ownership, and delete.
//!
//! These routes are merged into `admin::staff_surface`, which sits behind the
//! **permissive** outer guard (`oxy_owner_or_app_admin_guard_middleware`) plus
//! `block_admin_while_acting` — so they are reachable by Global Owners **and**
//! Global Admins (Oxy ops), matching the `adminOrAppAdmin` Tenants UI that
//! fronts them. Only `billing`/`app_admins` escalate to owner-strict. Handlers
//! therefore assume a staff caller, not necessarily an OXY_OWNER.

use std::collections::HashMap;

use axum::extract::{OriginalUri, Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::Utc;
use entity::org_invitations::InviteStatus;
use entity::org_members::OrgRole;
use entity::workspaces::WorkspaceStatus;
use entity::{org_billing, org_invitations, org_members, organizations, users, workspaces};
use oxy::database::client::establish_connection;
use oxy::database::filters::UserQueryFilterExt;
use oxy_app_core::pagination::{self, Paged, trim_overfetch};
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_shared::fleet_role::{RouteRole, RouteRoleDecl};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseBackend, EntityTrait, FromQueryResult,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::server::api::admin::scope;
use crate::server::router::AppState;
use crate::server::service::workspace_provisioning::{StagedWorkspace, create_default_workspace};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs", post(create_org))
        .route("/orgs-meta", get(list_orgs_meta))
        .route("/orgs/{org_id}/detail", get(get_org_detail))
        .route(
            "/orgs/{org_id}/logo",
            get(crate::server::api::org_logo::admin_get_org_logo)
                .put(crate::server::api::org_logo::admin_upload_org_logo)
                .delete(crate::server::api::org_logo::admin_delete_org_logo),
        )
        .route("/orgs/{org_id}", patch(rename_org).delete(delete_org))
        .route(
            "/orgs/{org_id}/transfer-ownership",
            post(transfer_ownership),
        )
}

/// `POST /admin/orgs` creates the org's `Default` workspace — a working copy on
/// node-local disk — so it must reach the ide. Everything else in [`router`] is
/// Postgres and takes the console's FleetOk wildcard (`admin::router_roles`).
pub(crate) const CREATE_ORG_ROLE: RouteRoleDecl = RouteRoleDecl {
    method: "POST",
    path: "/orgs",
    role: RouteRole::IdeOnly,
};

// Create org + onboard owner  (POST /admin/orgs)

#[derive(Deserialize)]
pub struct AdminCreateOrgBody {
    /// Display name; also the slug source when `slug` is omitted.
    pub name: String,
    /// Optional explicit slug; derived from `name` when omitted/blank.
    pub slug: Option<String>,
    /// Email of the org owner to onboard.
    pub owner_email: String,
}

#[derive(Serialize)]
pub struct AdminCreateOrgResponse {
    pub org: AdminOrgMeta,
    /// `"seeded"` — the email was an existing user, added as Owner immediately.
    /// `"invited"` — an Owner-role invitation was created and emailed.
    pub owner_status: String,
    /// Echo of the (normalized) owner email so the UI can phrase the toast.
    pub owner_email: String,
    /// The Ready blank `Default` workspace every new org is created with.
    pub default_workspace_id: Uuid,
}

/// The two ways to onboard an owner. Kept as a pure decision so the seed-vs-invite
/// policy is unit-testable without Postgres (mirrors `plan_owner_transfer`).
#[derive(Debug, PartialEq, Eq)]
enum OwnerSeed {
    /// The email already belongs to a user — add them as Owner directly.
    SeedMember { user_id: Uuid },
    /// No account yet — create an Owner-role invitation and email it.
    InviteOwner { email: String },
}

fn plan_owner_seeding(existing_user_id: Option<Uuid>, email: &str) -> OwnerSeed {
    match existing_user_id {
        Some(user_id) => OwnerSeed::SeedMember { user_id },
        None => OwnerSeed::InviteOwner {
            email: email.to_string(),
        },
    }
}

/// POST /admin/orgs — create an organization and onboard its owner in one step.
///
/// If `owner_email` is a known user they are seeded as `Owner`; otherwise an
/// `Owner`-role invitation is created and emailed (7-day expiry). The org is
/// created billing-`Incomplete` (admin provisions the subscription separately),
/// and with a Ready blank `Default` workspace in the same transaction. That
/// scaffolds a working copy, so the route is IdeOnly — see `admin::router_roles`.
///
/// Requires [`Action::PlatformOrgCreate`] (`Cap::CreateOrgs`), not merely the router's
/// broader `PlatformOrgs` gate: creating a tenant and being able to administer one are
/// different powers, and the partner tier already draws that line. Today every role that
/// reaches this router holds both, so the check is inert — but an inert check that is
/// actually wired stays true when a role is added; a documented one that isn't wired is
/// just a comment.
pub async fn create_org(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    headers: HeaderMap,
    Json(body): Json<AdminCreateOrgBody>,
) -> Result<Json<AdminCreateOrgResponse>, StatusCode> {
    use crate::server::api::organizations::{
        is_reserved_slug, normalize_invite_email, send_invitation_email, slugify_name,
    };

    let db = establish_connection().await.map_err(internal)?;

    {
        // `load_platform_facts` returns `None` ONLY on a DbErr — `Ok(None)` (a real
        // non-staff caller) still yields `Some(facts)`. So an unreadable grant must be a
        // 500, not a 403: the same rule `deny_out_of_scope` and `split_by_scope` settled
        // in this change. Answering "you don't have permission" to a database blip is
        // both wrong and unactionable, and two call sites disagreeing about it is the
        // drift the helper split was meant to end.
        let facts = match crate::server::authz::loader::load_platform_facts(
            &db,
            actor.id,
            actor.email.as_deref().unwrap_or(""),
        )
        .await
        {
            Some(facts) => facts,
            None => {
                tracing::error!(
                    target: "authz",
                    "platform facts unreadable on create_org — refusing"
                );
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };
        if !crate::server::authz::allows(
            &facts,
            crate::server::authz::Action::PlatformOrgCreate,
            &crate::server::authz::Resource::platform(),
        ) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Explicit slug wins; otherwise derive from the name.
    let slug_source = body
        .slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(name.as_str());
    let slug = slugify_name(slug_source);
    if slug.is_empty() || is_reserved_slug(&slug) {
        // Empty → unusable; reserved → would shadow a top-level route. Both are
        // 422 (unprocessable), distinct from a real slug-taken 409 below.
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Normalize + format-check the owner email (shared with the invite path). A
    // malformed email is a 422 (well-formed request, unprocessable field) — the
    // same shape as the reserved-slug case above, and what the client maps.
    let owner_email =
        normalize_invite_email(&body.owner_email).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;

    // Best-effort slug uniqueness pre-check; the DB UNIQUE constraint is the
    // real guard against races (handled on insert below).
    if organizations::Entity::find()
        .filter(organizations::Column::Slug.eq(&slug))
        .one(&db)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(StatusCode::CONFLICT);
    }

    // Resolve the owner email to an existing LIVE user (read-only, pre-txn). Only
    // an active account is seeded as Owner; a deleted/suspended match falls
    // through to the invite path rather than handing the org to an inactive account.
    let existing_user = users::Entity::find()
        .filter_active_by_email(&owner_email)
        .one(&db)
        .await
        .map_err(internal)?;
    let plan = plan_owner_seeding(existing_user.map(|u| u.id), &owner_email);

    let now = Utc::now().fixed_offset();
    let org_id = Uuid::new_v4();
    let tx = db.begin().await.map_err(internal)?;

    let org = organizations::ActiveModel {
        id: Set(org_id),
        name: Set(name.clone()),
        slug: Set(slug),
        logo: ActiveValue::NotSet,
        logo_content_type: ActiveValue::NotSet,
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(&tx)
    .await
    .map_err(map_insert_conflict)?;

    // Eager-insert the org_billing row so `SubscriptionGuard` always finds one
    // (the same invariant every create-org path upholds). Starts `Incomplete`.
    org_billing::ActiveModel {
        id: Set(Uuid::new_v4()),
        org_id: Set(org_id),
        status: Set(org_billing::BillingStatus::Incomplete),
        seats_paid: Set(0),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&tx)
    .await
    .map_err(internal)?;

    // Onboard the owner: seed an existing user, or stage an Owner invitation.
    let mut pending_invite: Option<(String, String)> = None;
    let owner_status = match &plan {
        OwnerSeed::SeedMember { user_id } => {
            org_members::ActiveModel {
                id: Set(Uuid::new_v4()),
                org_id: Set(org_id),
                user_id: Set(*user_id),
                role: Set(OrgRole::Owner),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&tx)
            .await
            .map_err(internal)?;
            "seeded"
        }
        OwnerSeed::InviteOwner { email } => {
            let token = Uuid::new_v4().to_string();
            org_invitations::ActiveModel {
                id: Set(Uuid::new_v4()),
                org_id: Set(org_id),
                email: Set(email.clone()),
                role: Set(OrgRole::Owner),
                invited_by: Set(actor.id),
                token: Set(token.clone()),
                status: Set(InviteStatus::Pending),
                expires_at: Set((Utc::now() + chrono::Duration::days(7)).fixed_offset()),
                created_at: Set(now),
            }
            .insert(&tx)
            .await
            .map_err(internal)?;
            pending_invite = Some((email.clone(), token));
            "invited"
        }
    };

    // Every org starts with a Ready blank workspace, so the owner's first sign-in
    // lands on Home rather than a workspace wizard. Its row joins this
    // transaction: a workspace that cannot be made rolls the org back with it.
    let workspace = default_workspace(&tx, org_id, actor.id).await?;
    let default_workspace_id = workspace.commit(tx).await.map_err(internal)?;

    // Post-commit: email the Owner invitation in the background (the row +
    // token are already the source of truth, so a send failure never fails
    // the request). Seeded owners get no email by design.
    if let Some((to_email, token)) = pending_invite {
        let base_url = crate::server::api::auth::extract_base_url_from_headers(&headers);
        let inviter_name = actor.name.clone();
        // The inviter's ADDRESS, not their display label — this is the
        // reply-to an invitation carries. Empty when the actor has none, which
        // is what `invitation_handlers` passes too; the two paths now agree
        // rather than one of them claiming they do.
        let inviter_email = actor.email.clone().unwrap_or_default();
        let org_name = name.clone();
        tokio::spawn(async move {
            if let Err(e) = send_invitation_email(
                &to_email,
                &token,
                &base_url,
                &inviter_name,
                &inviter_email,
                &org_name,
            )
            .await
            {
                tracing::error!("admin create_org: owner invitation email failed: {e}");
            }
        });
    }

    tracing::info!(
        admin_email = %actor.label(),
        target_id = %org.id,
        action = "create_org",
        owner_status,
        "admin tenant action"
    );

    let seeded = matches!(plan, OwnerSeed::SeedMember { .. });
    Ok(Json(AdminCreateOrgResponse {
        org: AdminOrgMeta {
            id: org.id,
            name: org.name,
            slug: org.slug,
            created_at: org.created_at.to_rfc3339(),
            // A fresh org has one member iff we seeded an existing owner.
            member_count: if seeded { 1 } else { 0 },
            workspace_count: 1,
            // `owner_email` on the meta row reflects the Owner *member*; an
            // invited (not-yet-accepted) owner isn't a member yet.
            owner_email: seeded.then(|| owner_email.clone()),
            partner: None,
            is_partner: false,
        },
        owner_status: owner_status.to_string(),
        owner_email,
        default_workspace_id,
    }))
}

/// Stage the new org's `Default` workspace inside the org's transaction. Any
/// failure is a 500 — never the 409 a name clash would map to, which the client
/// reads as "slug taken" — and the transaction is dropped, rolling the org back.
async fn default_workspace(
    tx: &sea_orm::DatabaseTransaction,
    org_id: Uuid,
    actor_id: Uuid,
) -> Result<StagedWorkspace, StatusCode> {
    create_default_workspace(tx, org_id, actor_id)
        .await
        .map_err(|e| {
            tracing::error!(
                %org_id,
                error = %e,
                "admin create_org: default workspace could not be created; org rolled back"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Map an org-insert DbErr to a status: slug collisions caught at the DB UNIQUE
/// constraint become 409; everything else is a 500 via `internal`.
fn map_insert_conflict(e: sea_orm::DbErr) -> StatusCode {
    let msg = e.to_string();
    if msg.contains("unique") || msg.contains("duplicate") {
        tracing::warn!("admin create_org slug conflict (DB-level): {e}");
        return StatusCode::CONFLICT;
    }
    internal(e)
}

/// Every org that holds a partner grant. Small (a handful of partners), so one
/// unfiltered read is cheaper than a join per page.
async fn partner_grant_ids(
    db: &sea_orm::DatabaseConnection,
) -> Result<std::collections::HashSet<Uuid>, StatusCode> {
    Ok(entity::prelude::PartnerGrants::find()
        .all(db)
        .await
        .map_err(|e| {
            tracing::error!("orgs_admin: load partner grants: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .into_iter()
        .map(|g| g.org_id)
        .collect())
}

/// The partner (if any) that manages an org — `id` is the partner's ORG id, since
/// a partner IS an org. `partner_orgs.managed_org_id` is UNIQUE, so an org has at
/// most one. Powers the partner chip + relationship strip in the admin tenants UI
/// without a per-row round-trip.
#[derive(Serialize, Clone)]
pub struct OrgPartnerRef {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
}

#[derive(Serialize)]
pub struct AdminOrgMeta {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: String,
    pub member_count: i64,
    pub workspace_count: i64,
    pub owner_email: Option<String>,
    /// The partner that MANAGES this org, if any.
    pub partner: Option<OrgPartnerRef>,
    /// This org IS a partner (it holds a grant). Distinct from `partner` above,
    /// which is the org's *manager* — an org can be neither, either, or (in
    /// principle) both, and the Tenants directory lists partners separately, so
    /// the two facts must not be conflated.
    pub is_partner: bool,
}

#[derive(Serialize)]
pub struct AdminOrgDetail {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: String,
    pub updated_at: String,
    pub member_count: i64,
    pub workspace_count: i64,
    pub owner_email: Option<String>,
    pub owners: Vec<OrgUserSummary>,
    pub workspaces: Vec<OrgWorkspaceSummary>,
    pub partner: Option<OrgPartnerRef>,
}

#[derive(Serialize)]
pub struct OrgUserSummary {
    pub user_id: Uuid,
    pub email: String,
    pub name: String,
    pub role: String,
}

#[derive(Serialize)]
pub struct OrgWorkspaceSummary {
    pub id: Uuid,
    pub name: String,
    pub status: String,
    pub created_at: String,
}

#[derive(Deserialize)]
pub struct ListMetaQuery {
    pub search: Option<String>,
    pub page: Option<u64>,
    pub page_size: Option<u64>,
}

#[derive(Deserialize)]
pub struct RenameOrgBody {
    pub name: Option<String>,
    pub slug: Option<String>,
}

#[derive(Deserialize)]
pub struct TransferOwnershipBody {
    pub new_owner_user_id: Uuid,
}

pub async fn list_orgs_meta(
    OriginalUri(uri): OriginalUri,
    Query(q): Query<ListMetaQuery>,
) -> Result<Paged<AdminOrgMeta>, StatusCode> {
    let db = establish_connection().await.map_err(internal)?;
    let page = q.page.unwrap_or(0);
    // CLAMPED AT BOTH ENDS. `?page_size=0` past a top-only clamp is an infinite
    // pagination loop, not an empty page: the offset stays 0 on every page while
    // `page + 1` keeps advancing, so every request answers `[]` with a link to
    // the next one. See `oxy_app_core::pagination`.
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);

    let mut query = organizations::Entity::find().order_by_asc(organizations::Column::Name);

    if let Some(needle) = q.search.as_ref().filter(|s| !s.trim().is_empty()) {
        let like = format!("%{}%", needle.trim());
        query = query.filter(
            sea_orm::Condition::any()
                .add(organizations::Column::Name.contains(like.as_str()))
                .add(organizations::Column::Slug.contains(like.as_str())),
        );
    }

    // `page_size + 1`: the extra row is how the `Link: rel="next"` below knows a
    // next page exists, with no COUNT(*) that could disagree with this query.
    let mut orgs = query
        .offset(page.saturating_mul(page_size))
        .limit(page_size + 1)
        .all(&db)
        .await
        .map_err(internal)?;
    let has_more = trim_overfetch(&mut orgs, page_size);

    // Pre-aggregate the three per-row lookups into three IN (...) GROUP BY
    // queries indexed into HashMaps. Three extra queries per list is O(1)
    // per page instead of O(3N) — the previous loop was a real N+1.
    let org_ids: Vec<Uuid> = orgs.iter().map(|o| o.id).collect();
    let member_counts = count_org_members_in(&db, &org_ids)
        .await
        .map_err(internal)?;
    let workspace_counts = count_org_workspaces_in(&db, &org_ids)
        .await
        .map_err(internal)?;
    let owner_emails = lookup_owner_emails_in(&db, &org_ids)
        .await
        .map_err(internal)?;
    let partners = lookup_partners_for_orgs(&db, &org_ids)
        .await
        .map_err(internal)?;

    let partner_org_ids = partner_grant_ids(&db).await?;

    let mut out = Vec::with_capacity(orgs.len());
    for org in orgs {
        out.push(AdminOrgMeta {
            id: org.id,
            name: org.name,
            slug: org.slug,
            created_at: org.created_at.to_rfc3339(),
            member_count: member_counts.get(&org.id).copied().unwrap_or(0),
            workspace_count: workspace_counts.get(&org.id).copied().unwrap_or(0),
            owner_email: owner_emails.get(&org.id).cloned(),
            partner: partners.get(&org.id).cloned(),
            is_partner: partner_org_ids.contains(&org.id),
        });
    }

    // 0-indexed `page`, which is the reason the next page is handed over as a
    // URL: `admin/explorer.rs` counts from 1 under the same parameter name.
    Ok(pagination::page(
        out,
        has_more,
        &uri,
        // Saturating so `?page=<u64::MAX>` cannot panic a debug build; the
        // offset below saturates for the same reason.
        &[("page", page.saturating_add(1).to_string())],
    ))
}

pub async fn get_org_detail(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path(org_id): Path<Uuid>,
) -> Result<Json<AdminOrgDetail>, StatusCode> {
    let db = establish_connection().await.map_err(internal)?;
    // Scope: a bounded grant must not reach this org. See `admin::scope`.
    scope::deny_out_of_scope(&db, &actor, org_id).await?;
    let org = organizations::Entity::find_by_id(org_id)
        .one(&db)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let member_count = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(org.id))
        .count(&db)
        .await
        .map_err(internal)? as i64;
    let workspace_count = workspaces::Entity::find()
        .filter(workspaces::Column::OrgId.eq(org.id))
        .count(&db)
        .await
        .map_err(internal)? as i64;

    let owners = load_owners(&db, org.id).await.map_err(internal)?;
    let owner_email = owners.first().map(|u| u.email.clone());
    let ws_list = load_workspace_summaries(&db, org.id)
        .await
        .map_err(internal)?;
    let partner = lookup_partners_for_orgs(&db, &[org.id])
        .await
        .map_err(internal)?
        .remove(&org.id);

    Ok(Json(AdminOrgDetail {
        id: org.id,
        name: org.name,
        slug: org.slug,
        created_at: org.created_at.to_rfc3339(),
        updated_at: org.updated_at.to_rfc3339(),
        member_count,
        workspace_count,
        owner_email,
        owners,
        workspaces: ws_list,
        partner,
    }))
}

pub async fn rename_org(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path(org_id): Path<Uuid>,
    Json(body): Json<RenameOrgBody>,
) -> Result<Json<AdminOrgMeta>, StatusCode> {
    let trimmed_name = body.name.as_ref().map(|s| s.trim().to_string());
    let trimmed_slug = body.slug.as_ref().map(|s| s.trim().to_string());
    if trimmed_name.as_deref().is_some_and(str::is_empty)
        || trimmed_slug.as_deref().is_some_and(str::is_empty)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let db = establish_connection().await.map_err(internal)?;
    // Scope: a bounded grant must not reach this org. See `admin::scope`.
    scope::deny_out_of_scope(&db, &actor, org_id).await?;
    let org = organizations::Entity::find_by_id(org_id)
        .one(&db)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let mut active: organizations::ActiveModel = org.into();
    if let Some(name) = trimmed_name {
        active.name = Set(name);
    }
    if let Some(slug) = trimmed_slug {
        active.slug = Set(slug);
    }
    active.updated_at = Set(Utc::now().fixed_offset());
    let updated = active.update(&db).await.map_err(|e| {
        tracing::error!("rename_org failed: {e}");
        StatusCode::CONFLICT
    })?;

    // The custom-app serve path caches `(org_slug, app_slug) -> rows`, so an
    // org slug is a cache KEY as well as a cached value. Without this the old
    // slug keeps resolving for up to the TTL, and — worse — the cached org
    // model carries the stale slug into `AppRuntimeConfig`, so the serve-time
    // base-path rewrite is computed against a prefix that no longer exists.
    // Same reason `update_app` invalidates on an app-slug change.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();

    let member_count = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(updated.id))
        .count(&db)
        .await
        .map_err(internal)? as i64;
    let workspace_count = workspaces::Entity::find()
        .filter(workspaces::Column::OrgId.eq(updated.id))
        .count(&db)
        .await
        .map_err(internal)? as i64;
    let owner_email = lookup_owner_email(&db, updated.id).await;

    tracing::info!(
        admin_email = %actor.label(),
        target_id = %updated.id,
        action = "rename_org",
        "admin tenant action"
    );

    let partner = lookup_partners_for_orgs(&db, &[updated.id])
        .await
        .map_err(internal)?
        .remove(&updated.id);

    let is_partner = partner_grant_ids(&db).await?.contains(&updated.id);

    Ok(Json(AdminOrgMeta {
        id: updated.id,
        name: updated.name,
        slug: updated.slug,
        created_at: updated.created_at.to_rfc3339(),
        member_count,
        workspace_count,
        owner_email,
        partner,
        is_partner,
    }))
}

pub async fn delete_org(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path(org_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let db = establish_connection().await.map_err(internal)?;
    // Scope: a bounded grant must not reach this org. See `admin::scope`.
    scope::deny_out_of_scope(&db, &actor, org_id).await?;
    let res = organizations::Entity::delete_by_id(org_id)
        .exec(&db)
        .await
        .map_err(map_destructive_db_err)?;
    if res.rows_affected == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    // Cascaded apps outlive their rows in the resolution cache — see the same
    // call in the tenant-facing `organizations::delete_org`.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();
    tracing::info!(
        admin_email = %actor.label(),
        target_id = %org_id,
        action = "delete_org",
        "admin tenant action"
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn transfer_ownership(
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path(org_id): Path<Uuid>,
    Json(body): Json<TransferOwnershipBody>,
) -> Result<StatusCode, StatusCode> {
    let db = establish_connection().await.map_err(internal)?;
    // Scope: a bounded grant must not reach this org. See `admin::scope`.
    scope::deny_out_of_scope(&db, &actor, org_id).await?;
    organizations::Entity::find_by_id(org_id)
        .one(&db)
        .await
        .map_err(internal)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let tx = db.begin().await.map_err(internal)?;
    let now = Utc::now().fixed_offset();

    // Demote every existing Owner of this org (other than the new owner) to
    // Admin first. Doing this inside the same tx as the promotion below
    // guarantees we never leave the org with two owners or with zero owners
    // — even if the new owner already happens to be the current owner.
    let existing_owners = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .filter(org_members::Column::Role.eq(OrgRole::Owner))
        .filter(org_members::Column::UserId.ne(body.new_owner_user_id))
        .all(&tx)
        .await
        .map_err(internal)?;
    for prev in existing_owners {
        let mut active: org_members::ActiveModel = prev.into();
        active.role = Set(OrgRole::Admin);
        active.updated_at = Set(now);
        active.update(&tx).await.map_err(internal)?;
    }

    let membership = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .filter(org_members::Column::UserId.eq(body.new_owner_user_id))
        .one(&tx)
        .await
        .map_err(internal)?;

    match membership {
        Some(existing) => {
            let mut active: org_members::ActiveModel = existing.into();
            active.role = Set(OrgRole::Owner);
            active.updated_at = Set(now);
            active.update(&tx).await.map_err(internal)?;
        }
        None => {
            let active = org_members::ActiveModel {
                id: ActiveValue::Set(Uuid::new_v4()),
                org_id: Set(org_id),
                user_id: Set(body.new_owner_user_id),
                role: Set(OrgRole::Owner),
                created_at: Set(now),
                updated_at: Set(now),
            };
            active.insert(&tx).await.map_err(|e| {
                tracing::error!("transfer_ownership insert failed: {e}");
                StatusCode::BAD_REQUEST
            })?;
        }
    }

    tx.commit().await.map_err(internal)?;
    tracing::info!(
        admin_email = %actor.label(),
        target_id = %org_id,
        new_owner_user_id = %body.new_owner_user_id,
        action = "transfer_ownership",
        "admin tenant action"
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn lookup_owner_email(db: &sea_orm::DatabaseConnection, org_id: Uuid) -> Option<String> {
    let owner = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .filter(org_members::Column::Role.eq(OrgRole::Owner))
        .order_by_asc(org_members::Column::CreatedAt)
        .one(db)
        .await
        .ok()
        .flatten()?;
    users::Entity::find_by_id(owner.user_id)
        .one(db)
        .await
        .ok()
        .flatten()
        // `and_then`, not `map`: the owner's address is itself optional now, so
        // "no owner row" and "owner has no mailbox" both collapse to None —
        // which is what every caller of this already handles.
        .and_then(|u| u.email)
}

#[derive(FromQueryResult)]
struct OrgIdCountRow {
    org_id: Uuid,
    cnt: i64,
}

#[derive(FromQueryResult)]
struct OrgIdEmailRow {
    org_id: Uuid,
    email: String,
}

#[derive(FromQueryResult)]
struct OrgPartnerRow {
    org_id: Uuid,
    partner_id: Uuid,
    partner_name: String,
    partner_slug: String,
}

/// IN (...) join `partner_orgs` → `organizations`: the managing partner per org.
///
/// A partner IS an org, so the partner's name/slug come from `organizations` —
/// there is no `partners` table any more. `partner_orgs.managed_org_id` is
/// UNIQUE (one partner per client), so at most one row per org. Empty `org_ids`
/// returns an empty map without round-tripping the DB.
async fn lookup_partners_for_orgs(
    db: &sea_orm::DatabaseConnection,
    org_ids: &[Uuid],
) -> Result<HashMap<Uuid, OrgPartnerRef>, sea_orm::DbErr> {
    if org_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = sql_placeholders(org_ids.len());
    let sql = format!(
        "SELECT po.managed_org_id AS org_id, o.id AS partner_id, o.name AS partner_name, \
                o.slug AS partner_slug \
         FROM partner_orgs po JOIN organizations o ON o.id = po.partner_org_id \
         WHERE po.managed_org_id IN ({placeholders})"
    );
    let values: Vec<sea_orm::Value> = org_ids.iter().map(|id| (*id).into()).collect();
    let rows = OrgPartnerRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.org_id,
                OrgPartnerRef {
                    id: r.partner_id,
                    name: r.partner_name,
                    slug: r.partner_slug,
                },
            )
        })
        .collect())
}

/// IN (...) + GROUP BY member_count per org. Empty `org_ids` returns an
/// empty map without round-tripping the DB.
async fn count_org_members_in(
    db: &sea_orm::DatabaseConnection,
    org_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, sea_orm::DbErr> {
    if org_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = sql_placeholders(org_ids.len());
    let sql = format!(
        "SELECT org_id, COUNT(*)::bigint AS cnt FROM org_members \
         WHERE org_id IN ({placeholders}) GROUP BY org_id"
    );
    let values: Vec<sea_orm::Value> = org_ids.iter().map(|id| (*id).into()).collect();
    let rows = OrgIdCountRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.org_id, r.cnt)).collect())
}

/// IN (...) + GROUP BY workspace_count per org.
async fn count_org_workspaces_in(
    db: &sea_orm::DatabaseConnection,
    org_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, sea_orm::DbErr> {
    if org_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = sql_placeholders(org_ids.len());
    let sql = format!(
        "SELECT org_id, COUNT(*)::bigint AS cnt FROM workspaces \
         WHERE org_id IN ({placeholders}) GROUP BY org_id"
    );
    let values: Vec<sea_orm::Value> = org_ids.iter().map(|id| (*id).into()).collect();
    let rows = OrgIdCountRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.org_id, r.cnt)).collect())
}

/// One join against (org_members, users) returning the earliest-joined
/// Owner email per org. Equivalent to looping `lookup_owner_email` but
/// without the per-org round-trips.
async fn lookup_owner_emails_in(
    db: &sea_orm::DatabaseConnection,
    org_ids: &[Uuid],
) -> Result<HashMap<Uuid, String>, sea_orm::DbErr> {
    if org_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = sql_placeholders(org_ids.len());
    // DISTINCT ON picks the earliest-created Owner row per org, matching the
    // single-row variant that orders by created_at ASC LIMIT 1.
    let sql = format!(
        "SELECT DISTINCT ON (m.org_id) m.org_id AS org_id, u.email AS email \
         FROM org_members m JOIN users u ON u.id = m.user_id \
         WHERE m.org_id IN ({placeholders}) AND m.role = 'owner' \
         ORDER BY m.org_id, m.created_at ASC"
    );
    let values: Vec<sea_orm::Value> = org_ids.iter().map(|id| (*id).into()).collect();
    let rows = OrgIdEmailRow::find_by_statement(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.org_id, r.email)).collect())
}

/// Build a `$1, $2, ..., $n` placeholder string for use inside an IN clause.
fn sql_placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

async fn load_owners(
    db: &sea_orm::DatabaseConnection,
    org_id: Uuid,
) -> Result<Vec<OrgUserSummary>, sea_orm::DbErr> {
    let memberships = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .order_by_asc(org_members::Column::CreatedAt)
        .all(db)
        .await?;

    // Batch the user lookups into a single query instead of one per row.
    let user_ids: Vec<Uuid> = memberships.iter().map(|m| m.user_id).collect();
    let users_by_id: HashMap<Uuid, users::Model> = users::Entity::find()
        .filter(users::Column::Id.is_in(user_ids))
        .all(db)
        .await?
        .into_iter()
        .map(|u| (u.id, u))
        .collect();

    let mut out = Vec::with_capacity(memberships.len());
    for m in memberships {
        if let Some(u) = users_by_id.get(&m.user_id) {
            out.push(OrgUserSummary {
                user_id: u.id,
                email: u.label().to_string(),
                name: u.name.clone(),
                role: m.role.as_str().to_string(),
            });
        }
    }
    Ok(out)
}

async fn load_workspace_summaries(
    db: &sea_orm::DatabaseConnection,
    org_id: Uuid,
) -> Result<Vec<OrgWorkspaceSummary>, sea_orm::DbErr> {
    let rows = workspaces::Entity::find()
        .filter(workspaces::Column::OrgId.eq(org_id))
        .order_by_desc(workspaces::Column::CreatedAt)
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|w| OrgWorkspaceSummary {
            id: w.id,
            name: w.name,
            status: status_label(&w.status),
            created_at: w.created_at.to_rfc3339(),
        })
        .collect())
}

fn status_label(status: &WorkspaceStatus) -> String {
    match status {
        WorkspaceStatus::Ready => "ready".to_string(),
        WorkspaceStatus::Cloning => "cloning".to_string(),
        WorkspaceStatus::Failed => "failed".to_string(),
        WorkspaceStatus::NotOxyProject => "not_oxy_project".to_string(),
    }
}

fn internal<E: std::fmt::Display>(e: E) -> StatusCode {
    tracing::error!("orgs_admin internal error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Map a DbErr from a destructive admin operation (DELETE / UPDATE) to a
/// StatusCode. Only foreign-key violations yield 409 — they indicate that
/// other rows still reference this one and the operator must clean those up
/// first. Every other error (connection failures, internal SQL bugs, type
/// errors) is mapped to 500 via `internal` so we don't lie about the cause.
fn map_destructive_db_err(e: sea_orm::DbErr) -> StatusCode {
    use sea_orm::SqlErr;
    match e.sql_err() {
        Some(SqlErr::ForeignKeyConstraintViolation(msg)) => {
            tracing::warn!("destructive admin op blocked by FK constraint: {msg}");
            StatusCode::CONFLICT
        }
        _ => internal(e),
    }
}

/// Pure helper that captures the role-transition logic for
/// `transfer_ownership`. Given the current membership rows and the new owner
/// user id, return the (user_id, role) tuples that should exist after the
/// transfer. Extracted so the policy is testable without spinning up
/// Postgres — the actual transaction in `transfer_ownership` mirrors this
/// shape.
fn plan_owner_transfer(
    current: &[(Uuid, OrgRole)],
    new_owner_user_id: Uuid,
) -> Vec<(Uuid, OrgRole)> {
    let mut out: Vec<(Uuid, OrgRole)> = Vec::with_capacity(current.len() + 1);
    let mut new_owner_seen = false;
    for (user_id, role) in current {
        if *user_id == new_owner_user_id {
            new_owner_seen = true;
            out.push((*user_id, OrgRole::Owner));
        } else if *role == OrgRole::Owner {
            // Previous owner — demote so we never end up with two owners.
            out.push((*user_id, OrgRole::Admin));
        } else {
            out.push((*user_id, role.clone()));
        }
    }
    if !new_owner_seen {
        out.push((new_owner_user_id, OrgRole::Owner));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_covers_all_variants() {
        assert_eq!(status_label(&WorkspaceStatus::Ready), "ready");
        assert_eq!(status_label(&WorkspaceStatus::Cloning), "cloning");
        assert_eq!(status_label(&WorkspaceStatus::Failed), "failed");
        assert_eq!(
            status_label(&WorkspaceStatus::NotOxyProject),
            "not_oxy_project"
        );
    }

    #[test]
    fn rename_body_rejects_only_whitespace() {
        let body = RenameOrgBody {
            name: Some("   ".to_string()),
            slug: None,
        };
        let trimmed = body.name.as_ref().map(|s| s.trim().to_string());
        assert!(trimmed.as_deref().is_some_and(str::is_empty));
    }

    fn role_of(plan: &[(Uuid, OrgRole)], uid: Uuid) -> Option<OrgRole> {
        plan.iter().find(|(u, _)| *u == uid).map(|(_, r)| r.clone())
    }

    #[test]
    fn transfer_demotes_previous_owner_and_promotes_new_member() {
        let old_owner = Uuid::new_v4();
        let new_owner = Uuid::new_v4();
        let bystander = Uuid::new_v4();
        let before = vec![
            (old_owner, OrgRole::Owner),
            (new_owner, OrgRole::Admin),
            (bystander, OrgRole::Member),
        ];

        let after = plan_owner_transfer(&before, new_owner);

        assert_eq!(
            role_of(&after, old_owner),
            Some(OrgRole::Admin),
            "previous owner must be demoted to Admin"
        );
        assert_eq!(
            role_of(&after, new_owner),
            Some(OrgRole::Owner),
            "new owner must be promoted to Owner"
        );
        assert_eq!(
            role_of(&after, bystander),
            Some(OrgRole::Member),
            "bystander roles must be untouched"
        );
        let owner_count = after
            .iter()
            .filter(|(_, r)| matches!(r, OrgRole::Owner))
            .count();
        assert_eq!(
            owner_count, 1,
            "exactly one owner must remain after transfer"
        );
    }

    #[test]
    fn transfer_inserts_new_owner_when_not_already_member() {
        let old_owner = Uuid::new_v4();
        let new_owner = Uuid::new_v4();
        let before = vec![(old_owner, OrgRole::Owner)];

        let after = plan_owner_transfer(&before, new_owner);

        assert_eq!(role_of(&after, old_owner), Some(OrgRole::Admin));
        assert_eq!(role_of(&after, new_owner), Some(OrgRole::Owner));
        let owner_count = after
            .iter()
            .filter(|(_, r)| matches!(r, OrgRole::Owner))
            .count();
        assert_eq!(owner_count, 1);
    }

    #[test]
    fn transfer_demotes_multiple_legacy_owners() {
        let owner_a = Uuid::new_v4();
        let owner_b = Uuid::new_v4();
        let new_owner = Uuid::new_v4();
        let before = vec![
            (owner_a, OrgRole::Owner),
            (owner_b, OrgRole::Owner),
            (new_owner, OrgRole::Member),
        ];

        let after = plan_owner_transfer(&before, new_owner);

        assert_eq!(role_of(&after, owner_a), Some(OrgRole::Admin));
        assert_eq!(role_of(&after, owner_b), Some(OrgRole::Admin));
        assert_eq!(role_of(&after, new_owner), Some(OrgRole::Owner));
        let owner_count = after
            .iter()
            .filter(|(_, r)| matches!(r, OrgRole::Owner))
            .count();
        assert_eq!(owner_count, 1, "all legacy owners must be demoted");
    }

    #[test]
    fn transfer_idempotent_when_already_owner() {
        let owner = Uuid::new_v4();
        let before = vec![(owner, OrgRole::Owner)];

        let after = plan_owner_transfer(&before, owner);

        assert_eq!(role_of(&after, owner), Some(OrgRole::Owner));
        let owner_count = after
            .iter()
            .filter(|(_, r)| matches!(r, OrgRole::Owner))
            .count();
        assert_eq!(owner_count, 1);
    }

    #[test]
    fn seeding_adds_existing_user_as_owner() {
        let uid = Uuid::new_v4();
        assert_eq!(
            plan_owner_seeding(Some(uid), "owner@example.com"),
            OwnerSeed::SeedMember { user_id: uid },
            "a known user is seeded directly as Owner"
        );
    }

    #[test]
    fn seeding_invites_unknown_email_as_owner() {
        assert_eq!(
            plan_owner_seeding(None, "new@example.com"),
            OwnerSeed::InviteOwner {
                email: "new@example.com".to_string()
            },
            "an unknown email gets an Owner-role invitation"
        );
    }
}
