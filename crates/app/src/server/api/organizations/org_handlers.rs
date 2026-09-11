use axum::extract::Json;
use axum::http::StatusCode;
use chrono::Utc;
use entity::org_members;
use entity::organizations;
use entity::prelude::*;
use entity::workspaces;
use oxy::database::client::establish_connection;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use sea_orm::{ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, QueryFilter, QuerySelect};
use uuid::Uuid;

use crate::server::api::middlewares::org_context::OrgContextExtractor;
use crate::server::api::middlewares::role_guards::{OrgAdmin, OrgOwner};

use super::dto::*;
use super::ops::*;

// Organization CRUD
//
// There is no `POST /orgs`. Customers do not create organizations: Oxy staff
// onboard them through `POST /admin/orgs` and partners through
// `POST /partners/{id}/orgs`, and each org arrives with a Ready `Default`
// workspace so its owner's first sign-in lands on Home.

/// GET /orgs
/// Every organization the caller can reach.
///
/// The first call in most sessions: it turns "the customer" into the org UUID
/// every other endpoint wants. Includes orgs reached through a live
/// assume-role session, not only direct memberships.
#[utoipa::path(
    method(get),
    path = "/orgs",
    responses(
        (status = OK, description = "Organizations the caller is a member of, or is acting as", body = Vec<OrgResponse>, content_type = "application/json")
    ),
    security(
        ("BearerAuth" = [])
    )
)]
pub async fn list_orgs(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
) -> Result<Json<Vec<OrgResponse>>, StatusCode> {
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

    let mut org_ids: Vec<Uuid> = memberships.iter().map(|m| m.org_id).collect();

    // Orgs the caller is ACTING AS. While an assume-role session is live, staff
    // effectively *are* an Owner of that org (`org_context` synthesizes exactly
    // that), so the org must appear in their org list — otherwise the frontend
    // can't resolve `/{slug}` and the operator is told "You don't have permission"
    // for a tenant they are, at that moment, administering.
    //
    // The old escape hatch was the admin org directory, but the admin surface is
    // closed while acting (`assume::block_admin_while_acting`), which is what
    // broke this. Making the list honest is better than punching a hole in the
    // block: the truth is that you have this org right now.
    let assumed: Vec<Uuid> = crate::server::api::admin::assume::live_sessions_for(&db, user.id)
        .await
        .into_iter()
        .map(|s| s.org_id)
        .collect();
    for id in &assumed {
        if !org_ids.contains(id) {
            org_ids.push(*id);
        }
    }

    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }

    let orgs = Organizations::find()
        .filter(organizations::Column::Id.is_in(org_ids.clone()))
        .all(&db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to query organizations: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let member_counts = count_members_per_org(&db, &org_ids).await?;
    let workspace_counts = count_workspaces_per_org(&db, &org_ids).await?;

    // The role an assumed org resolves to is NOT always Owner. Staff act as Owner;
    // a partner acting as a client is synthesized as Admin (see
    // `assume::ActingAs::org_role`). Label each assumed org with the role the
    // server will actually enforce for it — hardcoding "owner" would surface
    // owner-only affordances (delete-org, billing, promote) that then 403, which is
    // exactly the mismatch this block is supposed to avoid.
    let mut assumed_role: std::collections::HashMap<Uuid, &'static str> = Default::default();
    for org_id in &assumed {
        if let Some(authority) = crate::server::api::admin::assume::may_act_as(
            &db,
            user.id,
            user.email.as_deref().unwrap_or(""),
            *org_id,
        )
        .await
        {
            assumed_role.insert(*org_id, authority.org_role().as_str());
        }
    }

    let responses: Vec<OrgResponse> = orgs
        .iter()
        .filter_map(|org| {
            // A real membership wins; otherwise this org is here because we're
            // acting as it, and the label is the role `org_context` will actually
            // synthesize — never more, never less.
            let role = match memberships.iter().find(|m| m.org_id == org.id) {
                Some(m) => m.role.as_str().to_string(),
                None => assumed_role.get(&org.id)?.to_string(),
            };
            Some(OrgResponse {
                id: org.id,
                name: org.name.clone(),
                slug: org.slug.clone(),
                role,
                created_at: org.created_at.to_rfc3339(),
                updated_at: org.updated_at.to_rfc3339(),
                workspace_count: Some(workspace_counts.get(&org.id).copied().unwrap_or(0)),
                member_count: Some(member_counts.get(&org.id).copied().unwrap_or(0)),
            })
        })
        .collect();

    Ok(Json(responses))
}

/// GET /orgs/:org_id
pub async fn get_org(
    OrgContextExtractor(ctx): OrgContextExtractor,
) -> Result<Json<OrgResponse>, StatusCode> {
    Ok(Json(org_response(&ctx.org, &ctx.membership.role)))
}

/// PATCH /orgs/:org_id
pub async fn update_org(
    OrgAdmin(ctx): OrgAdmin,
    Json(req): Json<UpdateOrgRequest>,
) -> Result<Json<OrgResponse>, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut active: organizations::ActiveModel = ctx.org.clone().into();
    if let Some(name) = req.name {
        active.name = ActiveValue::Set(name);
    }
    if let Some(slug) = req.slug {
        let normalized = slugify_name(&slug);
        if normalized.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if is_reserved_slug(&normalized) {
            // 422 distinguishes "forbidden name" (it would shadow a top-level
            // frontend route) from a real slug-already-taken collision.
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        active.slug = ActiveValue::Set(normalized);
    }
    active.updated_at = ActiveValue::Set(Utc::now().fixed_offset());

    let updated = active.update(&db).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("unique") || msg.contains("duplicate") {
            tracing::warn!("Slug uniqueness conflict on update (caught at DB level): {e}");
            return StatusCode::CONFLICT;
        }
        tracing::error!("Failed to update organization: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // An org slug is half the custom-app resolution cache key, and the cached
    // org row also feeds `AppRuntimeConfig`. Without this the old slug keeps
    // resolving for up to the TTL and the serve-time base-path rewrite is
    // computed against a prefix that no longer exists. This is the tenant-facing
    // rename — an org admin renaming their own org — and is far more common than
    // the staff path in `admin::orgs_admin::rename_org`.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();

    Ok(Json(org_response(&updated, &ctx.membership.role)))
}

/// DELETE /orgs/:org_id
pub async fn delete_org(OrgOwner(ctx): OrgOwner) -> Result<StatusCode, StatusCode> {
    let db = establish_connection().await.map_err(|e| {
        tracing::error!("DB connection error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // The org delete below cascades its workspaces away (FK ON DELETE CASCADE),
    // so capture their ids up front — we need them both to deprovision airhouse
    // tenants (below) and to clean up their orphaned schedule rows (after the
    // delete), neither of which the cascade handles.
    let workspace_ids: Vec<Uuid> = match workspaces::Entity::find()
        .filter(workspaces::Column::OrgId.eq(ctx.org.id))
        .select_only()
        .column(workspaces::Column::Id)
        .into_tuple()
        .all(&db)
        .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                org_id = %ctx.org.id,
                "failed to list org workspaces for cleanup: {e}"
            );
            Vec::new()
        }
    };

    // Deprovision each of the org's workspaces before deleting the org.
    // The airhouse_tenants table is keyed by workspace_id (since the
    // m20260430 rebind), so passing org.id to deprovision was a no-op
    // and the SAs leaked. The FK to workspaces is ON DELETE CASCADE so
    // the local rows would still get cleaned up by the org delete below,
    // but the airhouse-side service accounts wouldn't be revoked. This
    // loop hits airhouse with each workspace's SA before we drop the
    // local data.
    if let Some(provisioner) = airhouse::provisioner_for(db.clone()) {
        for workspace_id in &workspace_ids {
            if let Err(e) = provisioner.deprovision(*workspace_id).await {
                tracing::warn!(
                    org_id = %ctx.org.id,
                    workspace_id = %workspace_id,
                    "airhouse tenant deprovisioning failed: {e}"
                );
            }
        }
    }

    // OLTP is org-keyed, not workspace-keyed, so it is one call rather than a
    // loop — and it has to happen BEFORE the delete, because the row carrying
    // the provider's project id goes with the org (FK ON DELETE CASCADE).
    //
    // On Neon that project is a real, billing resource: losing the row without
    // deleting the project leaves something nobody can find and nobody stops
    // paying for. Failure is logged rather than fatal, matching airhouse above
    // — a provider outage must not make an org undeletable — but it is logged
    // at `error` with the project id, because that string is the only way back
    // to an orphan.
    match oxy_oltp::provisioner::from_env(db.clone()).await {
        Ok(provisioner) => {
            if let Err(e) = provisioner.deprovision(ctx.org.id).await {
                tracing::error!(
                    org_id = %ctx.org.id,
                    "OLTP deprovisioning failed; the provider-side database may be \
                     orphaned and still billing: {e}"
                );
            }
        }
        // Disabled or misconfigured OLTP is the common case, and an org with no
        // OLTP database has nothing to deprovision.
        Err(e) => tracing::debug!(org_id = %ctx.org.id, "OLTP not configured: {e}"),
    }

    Organizations::delete_by_id(ctx.org.id)
        .exec(&db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete organization: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Schedules carry a plain `workspace_id` with no FK, so the org-delete
    // cascade above leaves them behind — remove each cascaded workspace's rows
    // or their health_eval/monitor schedules keep firing into the dead-letter
    // queue.
    for workspace_id in &workspace_ids {
        crate::server::api::workspaces::cleanup_workspace_schedules(&db, *workspace_id).await;
    }

    // The org delete cascades its apps away, but a cached `(org_slug, app_slug)`
    // resolution outlives them — and the access check that follows is computed
    // from the cached app model, so without this a global app admin can still be
    // served bundle bytes from a deleted org until the TTL expires.
    crate::server::api::custom_apps_cache::invalidate_app_resolution_cache();

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[path = "../organizations_tests.rs"]
mod organizations_tests;
