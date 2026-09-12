use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// The **ceiling**: what Oxy permits this partner to do AT ALL.
///
/// This is deliberately NOT "what a partner's people can do". Each member holds a
/// **role** (`partner_role_bindings`) and their effective authority is
/// `role ∩ ceiling ∩ assigned orgs`. So the partner's own admin can hand out roles
/// freely but can never exceed what Oxy granted — the ceiling is not negotiable
/// from inside.
///
/// Sensitive flags (`manage_billing`, `manage_secrets`) are Owner-only to grant.
#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "partner_capabilities")]
pub struct Model {
    /// The partner org (see `partner_grants`).
    #[sea_orm(primary_key, auto_increment = false)]
    pub org_id: Uuid,
    pub manage_members: bool,
    /// Publish / unpublish only — NOT data access.
    pub manage_apps: bool,
    /// The custom-app DATA PLANE (query / semantic-query / agent runs, oxyc proxy).
    /// Off by default: shipping an app is not the same as reading the warehouse.
    pub develop_apps: bool,
    pub view_audit: bool,
    pub manage_billing: bool,
    pub manage_secrets: bool,
    /// Onboard a client org (create + attach). Sensitive: it mints billable tenants.
    pub create_orgs: bool,
    /// Rename / configure a managed org.
    pub manage_org_settings: bool,
    pub updated_at: DateTimeWithTimeZone,
    #[sea_orm(
        belongs_to,
        from = "org_id",
        to = "org_id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    #[serde(skip)]
    pub partner_grants: BelongsTo<super::partner_grants::Entity>,
}

impl ActiveModelBehavior for ActiveModel {}
