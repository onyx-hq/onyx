//! `POST/PATCH /api/partners/{partner_org_id}/orgs…` — **partner-initiated
//! onboarding**, the product hole this permission model was written to close.
//!
//! Before this, a partner's org list was read-only and `attach_org` was
//! Oxy-staff-only, so every new client needed an Oxy employee. A reseller channel
//! whose resellers cannot onboard customers is a support queue, not a channel.
//!
//! Creating a client is safe to delegate in a way that *attaching* one is not:
//! a brand-new org affects nobody else's tenant, whereas attaching hands
//! administration of a **live** tenant to a third party (that stays staff-only,
//! in `admin::partners::membership`). Detaching likewise — a partner must not be
//! able to orphan a customer.
//!
//! Everything here is one transaction: org → membership → billing row →
//! attachment → audit (via `record_in_txn`) → the `Default` workspace's row. A
//! half-created client is not a state we allow.

use axum::Json;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use entity::org_invitations::{self, InviteStatus};
use entity::org_members::{self, OrgRole};
use entity::prelude::{Apps, OrgMembers, Organizations, Users};
use entity::{organizations, partner_orgs};
use oxy::database::filters::UserQueryFilterExt;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ChildOrg, db, internal, require_org_scope};
use crate::partner_context::PartnerActor;
use oxy_app::server::api::organizations::{
    is_reserved_slug, normalize_invite_email, send_invitation_email, slugify_name,
};
use oxy_app::server::service::workspace_provisioning::create_default_workspace;
use oxy_app_core::audit::{self, ActorType, AuditEntry};
use oxy_server_authz::partner_authz::PartnerCapability;

#[derive(Deserialize)]
pub struct CreateOrgBody {
    pub name: String,
    pub slug: String,
    /// Optional: the client's first Owner. They own their org from day one — the
    /// partner administers it, it never *belongs* to the partner.
    pub owner_email: Option<String>,
}

#[derive(Serialize)]
pub struct CreatedOrg {
    pub org: ChildOrg,
    /// How the first owner was onboarded:
    /// - `"seeded"` — the email was an existing user, added as Owner now.
    /// - `"invited"` — an unknown email got an Owner-role invitation emailed.
    /// - `"none"` — no owner email was given; the org has no owner yet.
    pub owner_status: String,
    /// The Ready blank `Default` workspace every new org is created with.
    pub default_workspace_id: Uuid,
}

/// `POST /partners/{partner_org_id}/orgs` — create + attach a client org, with
/// its `Default` workspace. That scaffolds a working copy, so this is the one
/// IdeOnly route in the crate — see `route_roles` in `lib.rs`.
pub async fn create_org(
    PartnerActor(scope): PartnerActor,
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    headers: HeaderMap,
    Json(body): Json<CreateOrgBody>,
) -> Result<Json<CreatedOrg>, StatusCode> {
    // Capability only — there is no target org to scope against yet. This is the
    // one partner write that isn't gated on an existing assignment. `create_orgs`
    // is a per-partner ceiling flag (staff can revoke it) that mints billable
    // tenants; it's granted by default to new partnerships (see `sane_default`).
    if !oxy_server_authz::partner_allows(&scope, None, PartnerCapability::CreateOrgs) {
        return Err(StatusCode::FORBIDDEN);
    }

    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let slug = slugify_name(&body.slug);
    if slug.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if is_reserved_slug(&slug) {
        // Not a collision — the name is forbidden because it would shadow a
        // top-level frontend route. 422 lets the client tell the two apart.
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    let db = db().await?;

    // Normalize + format-check the owner email (shared with the invite path), then
    // resolve it to an existing LIVE user BEFORE opening the txn. An unknown (or
    // inactive) email is not an error — the org is still created and we email the
    // owner an invite. A malformed email is a 422, matching the reserved-slug case.
    let owner_email: Option<String> = match body.owner_email.as_deref().map(str::trim) {
        Some(e) if !e.is_empty() => {
            Some(normalize_invite_email(e).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?)
        }
        _ => None,
    };
    let existing_owner = match &owner_email {
        // Only an active account is seeded as Owner; a deleted/suspended match
        // falls through to the invite path below.
        Some(email) => Users::find()
            .filter_active_by_email(email)
            .one(&db)
            .await
            .map_err(internal("load owner"))?,
        None => None,
    };

    let now = Utc::now().fixed_offset();
    let org_id = Uuid::new_v4();

    let txn = db.begin().await.map_err(internal("begin create org"))?;

    organizations::ActiveModel {
        id: ActiveValue::Set(org_id),
        name: ActiveValue::Set(name.clone()),
        slug: ActiveValue::Set(slug.clone()),
        logo: ActiveValue::NotSet,
        logo_content_type: ActiveValue::NotSet,
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    }
    .insert(&txn)
    .await
    .map_err(map_slug_conflict)?;

    // Onboard the first owner: seed an existing user immediately, or stage an
    // Owner-role invitation for an unknown email (emailed after commit).
    let mut pending_invite: Option<(String, String)> = None;
    let owner_status = if let Some(u) = &existing_owner {
        org_members::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            org_id: ActiveValue::Set(org_id),
            user_id: ActiveValue::Set(u.id),
            role: ActiveValue::Set(OrgRole::Owner),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
        }
        .insert(&txn)
        .await
        .map_err(internal("seed owner"))?;
        "seeded"
    } else if let Some(email) = &owner_email {
        let token = Uuid::new_v4().to_string();
        org_invitations::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            org_id: ActiveValue::Set(org_id),
            email: ActiveValue::Set(email.clone()),
            role: ActiveValue::Set(OrgRole::Owner),
            invited_by: ActiveValue::Set(actor.id),
            token: ActiveValue::Set(token.clone()),
            status: ActiveValue::Set(InviteStatus::Pending),
            expires_at: ActiveValue::Set((Utc::now() + chrono::Duration::days(7)).fixed_offset()),
            created_at: ActiveValue::Set(now),
        }
        .insert(&txn)
        .await
        .map_err(internal("create owner invitation"))?;
        pending_invite = Some((email.clone(), token));
        "invited"
    } else {
        "none"
    };

    // The SubscriptionGuard expects this row to exist for every org. Today a
    // partner-created org bills for itself, exactly like any other — who pays is
    // a business decision we deliberately deferred (design §6).
    entity::org_billing::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        org_id: ActiveValue::Set(org_id),
        status: ActiveValue::Set(entity::org_billing::BillingStatus::Incomplete),
        seats_paid: ActiveValue::Set(0),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await
    .map_err(internal("insert billing"))?;

    partner_orgs::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        partner_org_id: ActiveValue::Set(scope.partner_id),
        managed_org_id: ActiveValue::Set(org_id),
        created_by: ActiveValue::Set(Some(actor.id)),
        created_at: ActiveValue::NotSet,
    }
    .insert(&txn)
    .await
    .map_err(internal("attach new org"))?;

    audit::record_in_txn(
        &txn,
        AuditEntry::new(actor.label().to_string(), "partner.org.created")
            .actor(actor.id, ActorType::User)
            .partner(scope.partner_id)
            .org(org_id)
            .target("organization", org_id.to_string(), name.clone())
            .metadata(serde_json::json!({
                "slug": slug,
                "owner_email": owner_email,
                "owner_status": owner_status,
            })),
    )
    .await
    .map_err(internal("audit org.created"))?;

    // Last in the transaction, so nothing after it can fail and orphan the
    // working copy: every client org starts with a Ready blank workspace, and
    // one that cannot be made rolls the whole client back with it.
    let workspace = create_default_workspace(&txn, org_id, actor.id)
        .await
        .map_err(|e| {
            tracing::error!(
                %org_id,
                error = %e,
                "partner create_org: default workspace could not be created; org rolled back"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let default_workspace_id = workspace
        .commit(txn)
        .await
        .map_err(internal("commit create org"))?;

    // Post-commit: email the Owner invitation for an unknown owner. The row +
    // token are already committed, so a send failure never fails the request.
    if let Some((to_email, token)) = pending_invite {
        let base_url = oxy_app::server::api::auth::extract_base_url_from_headers(&headers);
        let inviter_name = actor.name.clone();
        let inviter_email = actor.email.clone();
        let org_name = name.clone();
        tokio::spawn(async move {
            if let Err(e) = send_invitation_email(
                &to_email,
                &token,
                &base_url,
                &inviter_name,
                inviter_email.as_deref().unwrap_or(""),
                &org_name,
            )
            .await
            {
                tracing::error!("partner create_org: owner invitation email failed: {e}");
            }
        });
    }

    Ok(Json(CreatedOrg {
        org: ChildOrg {
            org_id,
            name,
            slug,
            member_count: usize::from(existing_owner.is_some()),
            app_count: 0,
        },
        owner_status: owner_status.to_string(),
        default_workspace_id,
    }))
}

/// Map an org-insert DbErr: a slug UNIQUE violation is a 409, anything else
/// (connection drop, serialization failure) is a real 500 — never surface an
/// unrelated DB error to the partner as "slug taken". Mirrors the admin
/// handler's `map_insert_conflict`.
fn map_slug_conflict(e: sea_orm::DbErr) -> StatusCode {
    let msg = e.to_string();
    if msg.contains("unique") || msg.contains("duplicate") {
        tracing::warn!("partner create_org slug conflict (DB-level): {e}");
        return StatusCode::CONFLICT;
    }
    tracing::error!("partner_console: insert org: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}

#[derive(Deserialize)]
pub struct UpdateOrgBody {
    pub name: Option<String>,
}

/// `PATCH /partners/{partner_org_id}/orgs/{org_id}` — rename a client org.
///
/// The slug is deliberately NOT editable here: it is the org's public identity
/// (subdomains, custom-app URLs), so changing it breaks live links. That is the
/// client's own call, not their partner's.
pub async fn update_org(
    PartnerActor(scope): PartnerActor,
    AuthenticatedUserExtractor(actor): AuthenticatedUserExtractor,
    Path((_partner_org_id, org_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateOrgBody>,
) -> Result<Json<ChildOrg>, StatusCode> {
    let db = db().await?;
    require_org_scope(&db, &scope, org_id, PartnerCapability::ManageOrgSettings).await?;

    let org = Organizations::find_by_id(org_id)
        .one(&db)
        .await
        .map_err(internal("load org"))?
        .ok_or(StatusCode::NOT_FOUND)?;

    let Some(name) = body.name.as_deref().map(str::trim) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let before = org.name.clone();

    let txn = db.begin().await.map_err(internal("begin update org"))?;
    let mut active: organizations::ActiveModel = org.into();
    active.name = ActiveValue::Set(name.to_string());
    active.updated_at = ActiveValue::Set(Utc::now().fixed_offset());
    let saved = active.update(&txn).await.map_err(internal("update org"))?;

    audit::record_in_txn(
        &txn,
        AuditEntry::new(actor.label().to_string(), "partner.org.updated")
            .actor(actor.id, ActorType::User)
            .partner(scope.partner_id)
            .org(org_id)
            .target("organization", org_id.to_string(), saved.name.clone())
            .change(
                serde_json::json!({ "name": before }),
                serde_json::json!({ "name": saved.name }),
            ),
    )
    .await
    .map_err(internal("audit org.updated"))?;
    txn.commit().await.map_err(internal("commit update org"))?;

    let member_count = OrgMembers::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .count(&db)
        .await
        .map_err(internal("count members"))? as usize;
    let app_count = Apps::find()
        .filter(entity::apps::Column::OrgId.eq(org_id))
        .count(&db)
        .await
        .map_err(internal("count apps"))? as usize;

    Ok(Json(ChildOrg {
        org_id,
        name: saved.name,
        slug: saved.slug,
        member_count,
        app_count,
    }))
}
