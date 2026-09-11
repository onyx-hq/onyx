//! `/api/partners` — the partner self-service surface.
//!
//! Distinct from `/api/admin/partners` (Oxy-staff provisioning): this is what a
//! partner's own people use to run their subtree. Every org-scoped handler passes
//! through [`require_org_scope`] — the cross-tenant boundary — which checks the
//! capability AND that **this person** is assigned the org, returning `404` for
//! out-of-set orgs so existence isn't leaked.
//!
//! There are no per-person roles here, and no per-person org assignment: both collapsed
//! into a single **partner access** switch. Everyone with access to a partner reaches
//! all of that partner's clients and holds that partner's whole ceiling — so what a
//! person may do is `ceiling ∩ clients`, resolved once in
//! `partner_authz::resolve_scope` and decided by the `PartnerCap` rings in `oxy_authz`.
//!
//! The ceiling is therefore the entire capability story: a partner's people can do
//! exactly what the partner was granted, no more, and there is no narrower slice to
//! hand one of them.

mod app_mgmt;
mod health;
mod orgs;
mod partner_context;
mod people;
mod publish_tokens;
mod route_roles;
mod workspaces;
mod write;

use axum::extract::{OriginalUri, Path, Query};
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use entity::prelude::{Apps, OrgMembers, Organizations, PartnerOrgs, Users};
use entity::{apps, org_members, organizations, partner_orgs, users};
use oxy::database::client::establish_connection;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::partner_context::{PartnerActor, partner_middleware};
use oxy_app_core::AppState;
use oxy_app_core::audit::events_for_partner;
use oxy_app_core::pagination::{self, Paged, trim_overfetch};
use oxy_server_authz::partner_authz::{PartnerCapability, PartnerScope, scopes_for_user};

pub use route_roles::route_roles;

/// The whole `/api/partners` surface: the top-level list route plus the
/// partner-scoped subtree (wrapped in `partner_middleware`, which resolves
/// [`PartnerScope`] and 403s anyone holding no partner role in that org).
/// Mounted by the `oxy-server` composition root.
///
/// Every handler here but one is Postgres-only, so these routes are **FleetOk by
/// default** — undeclared, which means HA-safe and not workspace-scoped. The one
/// is `POST /orgs`, which scaffolds the new org's `Default` workspace onto
/// node-local disk and is declared IdeOnly in [`route_roles`]. A route added
/// here that touches the workspace FS, `.git`, or the state dir needs the same.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/partners", get(list_my_partners))
        .nest("/partners/{partner_org_id}", scoped_routes())
}

fn scoped_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(overview))
        // A partner can now ONBOARD a client itself (`create_orgs`) — the hole that
        // made the reseller channel a support queue. Attaching an *existing* org
        // stays Oxy-only; creating a brand-new one affects nobody else's tenant.
        .route("/orgs", get(list_orgs).post(orgs::create_org))
        .route("/orgs/{org_id}", patch(orgs::update_org))
        .route(
            "/orgs/{org_id}/members",
            get(list_org_members).post(write::invite_member),
        )
        .route(
            "/orgs/{org_id}/members/{user_id}",
            patch(write::update_member_role).delete(write::remove_member),
        )
        .route(
            "/orgs/{org_id}/workspaces",
            get(workspaces::list_org_workspaces),
        )
        .route("/orgs/{org_id}/apps", get(app_mgmt::list_org_apps))
        // The partner's OWN apps — a partner is a real org with its own custom
        // apps, and those are not reachable through the client routes because the
        // partner is not its own client. Authorized by ORG authority (an officer of
        // the partner org), never by the ceiling, so no operator gains reach over
        // their own org that they didn't already have. See `load_manageable_app`.
        .route("/own-apps", get(app_mgmt::list_own_apps))
        .route("/own-teams", get(app_mgmt::list_own_teams))
        .route("/own-people", get(app_mgmt::list_own_people))
        .route("/orgs/{org_id}/teams", get(app_mgmt::list_org_teams))
        // Both halves of the grant picker are gated on `manage_apps` — the
        // member-management endpoint above needs `manage_members`, which a
        // manage_apps-only partner correctly does not hold.
        .route(
            "/orgs/{org_id}/grantable-people",
            get(app_mgmt::list_grantable_people),
        )
        // Who may open a managed app — `manage_apps`, the same capability as
        // publish. Naming an audience is lifecycle, not data access.
        .route(
            "/apps/{app_id}/access",
            get(app_mgmt::get_app_access).put(app_mgmt::set_app_access),
        )
        .route(
            "/apps/{app_id}/publish",
            post(app_mgmt::publish_app).delete(app_mgmt::unpublish_app),
        )
        // App-scoped publish tokens for a client's app (CI credentials). Confined to
        // the one app + consent at publish time — see `publish_tokens`.
        .route(
            "/apps/{app_id}/publish-tokens",
            get(publish_tokens::list_tokens).post(publish_tokens::create_token),
        )
        .route(
            "/apps/{app_id}/publish-tokens/{token_id}",
            axum::routing::delete(publish_tokens::revoke_token),
        )
        // The partner's OWN people: who is an operator. Managed by the partner org's
        // owner/admin — see `people.rs`.
        .route("/people", get(people::list_people))
        .route(
            "/people/{org_member_id}",
            put(people::grant_access).delete(people::revoke_access),
        )
        .route("/health", get(health::list_health))
        .route("/audit", get(partner_audit))
        .layer(axum::middleware::from_fn(partner_middleware))
}

pub(crate) async fn db() -> Result<DatabaseConnection, StatusCode> {
    establish_connection().await.map_err(|e| {
        tracing::error!("partner_console: DB connect failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

pub(crate) fn internal<E: std::fmt::Display>(ctx: &'static str) -> impl Fn(E) -> StatusCode {
    move |e| {
        tracing::error!("partner_console: {ctx}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// The cross-tenant boundary. A partner action on a client org is allowed only if the
/// **ceiling** of the partner being acted as holds `cap` AND the org is one of that
/// partner's clients. Out-of-set orgs return `404` (not `403`) so a partner can't probe
/// which orgs exist outside their subtree.
pub(crate) async fn require_org_scope(
    _db: &DatabaseConnection,
    scope: &PartnerScope,
    org_id: Uuid,
    cap: PartnerCapability,
) -> Result<(), StatusCode> {
    // `scope.org_ids` is the partner's managed clients — every operator reaches all of
    // them. The unified model decides both ownership (the org is this partner's client)
    // and capability, scoped to the partner being acted as.
    let managed = &scope.org_ids;
    if oxy_server_authz::partner_allows(scope, Some(org_id), cap) {
        return Ok(());
    }
    // Deny: 404 when the partner doesn't manage the org (don't leak existence),
    // else 403 (manages it but the ceiling lacks the capability).
    // Presentation only — the security decision was made above.
    Err(if managed.contains(&org_id) {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::NOT_FOUND
    })
}

// ── DTOs ───────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MyPartner {
    /// The partner IS an org, so this is an org id.
    pub partner_id: String,
    pub slug: String,
    pub name: String,
    pub org_count: usize,
    /// The partner's ceiling — what any operator here may do. The UI shows exactly
    /// what is possible; there are no per-person roles.
    pub capabilities: CapabilitiesDto,
}

#[derive(Serialize)]
pub struct CapabilitiesDto {
    pub manage_members: bool,
    pub manage_apps: bool,
    pub develop_apps: bool,
    pub view_audit: bool,
    pub manage_billing: bool,
    pub manage_secrets: bool,
    pub create_orgs: bool,
    pub manage_org_settings: bool,
}

impl From<&PartnerScope> for CapabilitiesDto {
    fn from(s: &PartnerScope) -> Self {
        Self {
            manage_members: s.capabilities.manage_members,
            manage_apps: s.capabilities.manage_apps,
            develop_apps: s.capabilities.develop_apps,
            view_audit: s.capabilities.view_audit,
            manage_billing: s.capabilities.manage_billing,
            manage_secrets: s.capabilities.manage_secrets,
            create_orgs: s.capabilities.create_orgs,
            manage_org_settings: s.capabilities.manage_org_settings,
        }
    }
}

#[derive(Serialize)]
pub struct ChildOrg {
    pub org_id: Uuid,
    pub name: String,
    pub slug: String,
    pub member_count: usize,
    pub app_count: usize,
}

#[derive(Serialize)]
pub struct OrgMemberDto {
    pub user_id: Uuid,
    pub email: String,
    pub name: Option<String>,
    pub role: String,
}

#[derive(Serialize)]
pub struct AuditEventDto {
    pub id: Uuid,
    pub created_at: String,
    pub actor_email: String,
    pub action: String,
    pub org_id: Option<Uuid>,
    pub target_label: Option<String>,
    pub outcome: String,
}

// ── handlers ─────────────────────────────────────────────────────────────

/// `GET /partners` — partners the caller holds a role at (drives the console entry).
pub async fn list_my_partners(
    AuthenticatedUserExtractor(user): AuthenticatedUserExtractor,
) -> Result<Json<Vec<MyPartner>>, StatusCode> {
    let db = db().await?;
    let scopes = scopes_for_user(&db, user.id, user.email.as_deref().unwrap_or("")).await;
    if scopes.is_empty() {
        return Ok(Json(Vec::new()));
    }

    let ids: Vec<Uuid> = scopes.iter().map(|s| s.partner_id).collect();
    // The partner's name comes from the ORG — there is no separate partner record.
    let names: HashMap<Uuid, String> = Organizations::find()
        .filter(organizations::Column::Id.is_in(ids.clone()))
        .all(&db)
        .await
        .map_err(internal("load partner orgs"))?
        .into_iter()
        .map(|o| (o.id, o.name))
        .collect();

    let mut org_counts: HashMap<Uuid, usize> = HashMap::new();
    for row in PartnerOrgs::find()
        .filter(partner_orgs::Column::PartnerOrgId.is_in(ids))
        .all(&db)
        .await
        .map_err(internal("count orgs"))?
    {
        *org_counts.entry(row.partner_org_id).or_default() += 1;
    }

    let out = scopes
        .iter()
        .map(|s| MyPartner {
            partner_id: s.partner_id.to_string(),
            slug: s.slug.clone(),
            name: names.get(&s.partner_id).cloned().unwrap_or_default(),
            org_count: org_counts.get(&s.partner_id).copied().unwrap_or(0),
            capabilities: s.into(),
        })
        .collect();
    Ok(Json(out))
}

/// `GET /partners/{id}` — the clients THIS person handles, with counts.
pub async fn overview(
    PartnerActor(scope): PartnerActor,
) -> Result<Json<Vec<ChildOrg>>, StatusCode> {
    let db = db().await?;
    child_orgs(&db, &scope).await.map(Json)
}

/// `GET /partners/{id}/orgs` — same list (explicit route for the UI).
pub async fn list_orgs(
    PartnerActor(scope): PartnerActor,
) -> Result<Json<Vec<ChildOrg>>, StatusCode> {
    let db = db().await?;
    child_orgs(&db, &scope).await.map(Json)
}

pub(crate) async fn child_orgs(
    db: &DatabaseConnection,
    scope: &PartnerScope,
) -> Result<Vec<ChildOrg>, StatusCode> {
    let org_ids = scope.org_ids.clone();
    if org_ids.is_empty() {
        return Ok(Vec::new());
    }

    let orgs = Organizations::find()
        .filter(organizations::Column::Id.is_in(org_ids.clone()))
        .all(db)
        .await
        .map_err(internal("load orgs"))?;

    let mut member_counts: HashMap<Uuid, usize> = HashMap::new();
    for m in OrgMembers::find()
        .filter(org_members::Column::OrgId.is_in(org_ids.clone()))
        .all(db)
        .await
        .map_err(internal("count members"))?
    {
        *member_counts.entry(m.org_id).or_default() += 1;
    }

    let mut app_counts: HashMap<Uuid, usize> = HashMap::new();
    for a in Apps::find()
        .filter(apps::Column::OrgId.is_in(org_ids))
        .all(db)
        .await
        .map_err(internal("count apps"))?
    {
        *app_counts.entry(a.org_id).or_default() += 1;
    }

    Ok(orgs
        .into_iter()
        .map(|o| ChildOrg {
            member_count: member_counts.get(&o.id).copied().unwrap_or(0),
            app_count: app_counts.get(&o.id).copied().unwrap_or(0),
            org_id: o.id,
            name: o.name,
            slug: o.slug,
        })
        .collect())
}

/// `GET /partners/{id}/orgs/{org_id}/members` — members of one client org.
pub async fn list_org_members(
    PartnerActor(scope): PartnerActor,
    Path((_partner_org_id, org_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Vec<OrgMemberDto>>, StatusCode> {
    let db = db().await?;
    require_org_scope(&db, &scope, org_id, PartnerCapability::ManageMembers).await?;

    let members = OrgMembers::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .all(&db)
        .await
        .map_err(internal("load members"))?;

    let user_ids: Vec<Uuid> = members.iter().map(|m| m.user_id).collect();
    let users: HashMap<Uuid, users::Model> = Users::find()
        .filter(users::Column::Id.is_in(user_ids))
        .all(&db)
        .await
        .map_err(internal("load users"))?
        .into_iter()
        .map(|u| (u.id, u))
        .collect();

    let out = members
        .into_iter()
        .map(|m| {
            let u = users.get(&m.user_id);
            OrgMemberDto {
                user_id: m.user_id,
                email: u.map(|u| u.label().to_string()).unwrap_or_default(),
                name: u.map(|u| u.name.clone()),
                role: m.role.as_str().to_string(),
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    pub limit: Option<u64>,
    /// Same spelling as `admin/audit.rs`. One way to page an audit log across
    /// the platform, not two.
    pub offset: Option<u64>,
}

/// `GET /partners/{id}/audit` — the audit view, scoped to this person's clients.
pub async fn partner_audit(
    PartnerActor(scope): PartnerActor,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<AuditQuery>,
) -> Result<Paged<AuditEventDto>, StatusCode> {
    if !oxy_server_authz::partner_allows(&scope, None, PartnerCapability::ViewAudit) {
        return Err(StatusCode::FORBIDDEN);
    }
    let db = db().await?;
    let org_ids = scope.org_ids.clone();
    // Clamped at both ends — a zero page size makes `rel="next"` point at the
    // request that produced it. See `oxy_app_core::pagination`.
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);

    // `limit + 1`: the extra event is how the `Link: rel="next"` below knows
    // there is another page. A partner's audit log only grows, and "have I read
    // all of it" is the question this endpoint exists to answer.
    let mut events = events_for_partner(&db, scope.partner_id, &org_ids, limit + 1, offset)
        .await
        .map_err(internal("load audit"))?;
    let has_more = trim_overfetch(&mut events, limit);

    let out = events
        .into_iter()
        .map(|e| AuditEventDto {
            id: e.id,
            created_at: e.created_at.to_rfc3339(),
            actor_email: e.actor_email,
            action: e.action,
            org_id: e.org_id,
            target_label: e.target_label,
            outcome: e.outcome,
        })
        .collect();
    Ok(pagination::page(
        out,
        has_more,
        &uri,
        &[("offset", offset.saturating_add(limit).to_string())],
    ))
}
