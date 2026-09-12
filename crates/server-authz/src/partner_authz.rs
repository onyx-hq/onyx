//! Partner authorization — "may this user administer this client org, and with
//! what permissions?"
//!
//! Model: `internal-docs/partner-platform.md`.
//!
//! ## The shape
//!
//! A partner is not an entity — it is a **grant an organization holds**. Its people
//! are ordinary `org_members` of that org who additionally hold **partner access**
//! (a `partner_role_bindings` row). So:
//!
//! ```text
//! authority(user, target_org) = ceiling                     // what Oxy allows the partner
//!                             ∩ has_access(user)             // is this member an operator
//!                             ∩ (target_org ∈ partner's clients)
//! ```
//!
//! The **ceiling** is `partner_capabilities` — Oxy's grant to the partner, and the
//! whole capability story: there are no per-person roles and no per-client scope.
//! Every operator reaches every client and can do exactly what the ceiling allows.
//!
//! ## The load-bearing decision (unchanged)
//!
//! Partner power flows through this capability-gated surface — it is **never**
//! injected as a synthetic membership into `OrgContext`. A partner admin given a
//! synthetic Org Admin membership would silently pass `OrgAdminStrict` (billing /
//! admin-promotion) regardless of their permissions. Making a partner org-backed
//! changed *identity and home*, not *enforcement*: this module stays additive and
//! the org/workspace guards are untouched.
//!
//! Everything here **fails closed**: a DB error, a missing ceiling row, a suspended
//! grant, or no partner-access binding all resolve to "no access".
//!
//! ## Where authorization lives — read it in this order
//!
//! Four files, one flow:
//!
//! 1. **Vocabulary** — [`PartnerCapability`] (this file): the closed set of
//!    permissions. The single source of truth; everything below derives from it.
//! 2. **Ceiling** — [`Capabilities`] (this file) + the `partner_capabilities`
//!    table: which of those a given partner was granted (Oxy-set, per partner).
//! 3. **Decision** — `authz::partner_allows`, which asks the unified model
//!    (`Ring::PartnerCap` in `oxy-authz`): the capability must come from the partner
//!    being ACTED AS, and — for an org-scoped action — the org must be one of its
//!    clients. The partner tier used to carry its own Cedar policy for this; there is
//!    now one model for all of Oxy's authorization, and no engine.
//! 4. **Enforcement** — `partner_console::require_org_scope` (org-scoped actions) and
//!    the custom-app gate (via [`partner_grants_app_access`], in this file) call the
//!    decision and map a Deny to 403/404. This is where a handler opts a route into a
//!    capability.
//!
//! ## Extending it
//!
//! **Add a capability** (say `manage_schedules`):
//!   1. Migration: add a `partner_capabilities.manage_schedules` boolean.
//!   2. In this file: add the [`PartnerCapability`] variant + its `as_str` arm +
//!      its [`PartnerCapability::ALL`] entry; add the [`Capabilities`] field + its
//!      `from_model` / `allows` / `set` arms. The compiler forces every one of
//!      these **except the `ALL` entry** — that is the single step you must remember.
//!   3. In `authz`: add the matching `Cap` variant + `Action` + its `Ring::PartnerCap`
//!      mapping. The compiler forces the `cap_of` / `partner_action` arms, so it will
//!      not let you half-do this.
//!   4. Enforce it: at the handler, `require_org_scope(&db, &scope, org_id,
//!      PartnerCapability::ManageSchedules)`.
//!
//! **Gate a new endpoint on an existing capability**: just call `require_org_scope`
//! (or check the cap in the custom-app gate). No policy or vocabulary change.
//!
//! **Change what a capability *means*** (the rule, not the name): edit `Ring::PartnerCap`
//! in `oxy_authz::allows` — the one place every capability's rule is stated. If a
//! capability ever needs a non-uniform rule, give it its own `Ring` there.
//!
//! §2–§3 of the design doc is the prose version of this map.

use entity::prelude::*;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use uuid::Uuid;

/// A single permission an action can require. Compile-time — Oxy owns this
/// vocabulary; customers never invent permissions (see the design doc for why we
/// don't build AWS/GCP-style IAM).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PartnerCapability {
    ManageMembers,
    /// Publish / unpublish only — NOT data access.
    ManageApps,
    /// The custom-app data plane (query / semantic-query / agent runs, oxyc proxy).
    DevelopApps,
    ViewAudit,
    ManageBilling,
    ManageSecrets,
    /// Onboard a client org (create + attach). Mints billable tenants.
    CreateOrgs,
    /// Rename / configure a managed org.
    ManageOrgSettings,
}

impl PartnerCapability {
    /// Stable name — the string in a request's capability set, and what the console
    /// reports. The RULE for each capability lives in `oxy_authz`'s `Ring::PartnerCap`;
    /// `authz::cap_of` maps this enum onto it, and the compiler keeps that exhaustive.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManageMembers => "manage_members",
            Self::ManageApps => "manage_apps",
            Self::DevelopApps => "develop_apps",
            Self::ViewAudit => "view_audit",
            Self::ManageBilling => "manage_billing",
            Self::ManageSecrets => "manage_secrets",
            Self::CreateOrgs => "create_orgs",
            Self::ManageOrgSettings => "manage_org_settings",
        }
    }

    /// Every variant. **`granted_names` and `authz`'s scope-facts both iterate this**,
    /// so a new capability MUST be added here — the compiler won't force it (see the
    /// module-level "Extending it" recipe). It is the one manual step; every other arm
    /// is an exhaustive `match` the compiler checks.
    pub const ALL: [Self; 8] = [
        Self::ManageMembers,
        Self::ManageApps,
        Self::DevelopApps,
        Self::ViewAudit,
        Self::ManageBilling,
        Self::ManageSecrets,
        Self::CreateOrgs,
        Self::ManageOrgSettings,
    ];
}

// There is deliberately NO role catalog. A partner has one kind of person — an
// operator — and their authority IS the partner's ceiling. What Oxy grants the
// partner (below) is the whole story; the partner does not slice it further.

/// A permission set. `Default` is all-false so a missing row fails closed rather
/// than granting blanket power.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub manage_members: bool,
    pub manage_apps: bool,
    pub develop_apps: bool,
    pub view_audit: bool,
    pub manage_billing: bool,
    pub manage_secrets: bool,
    pub create_orgs: bool,
    pub manage_org_settings: bool,
}

impl Capabilities {
    pub fn from_model(m: &entity::partner_capabilities::Model) -> Self {
        Self {
            manage_members: m.manage_members,
            manage_apps: m.manage_apps,
            develop_apps: m.develop_apps,
            view_audit: m.view_audit,
            manage_billing: m.manage_billing,
            manage_secrets: m.manage_secrets,
            create_orgs: m.create_orgs,
            manage_org_settings: m.manage_org_settings,
        }
    }

    pub fn allows(&self, cap: PartnerCapability) -> bool {
        use PartnerCapability as C;
        match cap {
            C::ManageMembers => self.manage_members,
            C::ManageApps => self.manage_apps,
            C::DevelopApps => self.develop_apps,
            C::ViewAudit => self.view_audit,
            C::ManageBilling => self.manage_billing,
            C::ManageSecrets => self.manage_secrets,
            C::CreateOrgs => self.create_orgs,
            C::ManageOrgSettings => self.manage_org_settings,
        }
    }

    pub fn set(&mut self, cap: PartnerCapability, on: bool) {
        use PartnerCapability as C;
        match cap {
            C::ManageMembers => self.manage_members = on,
            C::ManageApps => self.manage_apps = on,
            C::DevelopApps => self.develop_apps = on,
            C::ViewAudit => self.view_audit = on,
            C::ManageBilling => self.manage_billing = on,
            C::ManageSecrets => self.manage_secrets = on,
            C::CreateOrgs => self.create_orgs = on,
            C::ManageOrgSettings => self.manage_org_settings = on,
        }
    }

    /// The granted names — what the console reports as this partner's ceiling.
    pub fn granted_names(&self) -> Vec<&'static str> {
        PartnerCapability::ALL
            .into_iter()
            .filter(|c| self.allows(*c))
            .map(|c| c.as_str())
            .collect()
    }
}

/// Resolved partner authority for one request.
#[derive(Clone, Debug)]
pub struct PartnerScope {
    /// The partner — which IS an org.
    pub partner_id: Uuid,
    /// The partner org's slug (console routing).
    pub slug: String,
    /// What this operator may do — the partner's ceiling, verbatim. Every operator
    /// of a partner has the same authority; there are no per-person roles.
    pub capabilities: Capabilities,
    /// Every client the partner manages. All operators reach all clients, so this
    /// is the partner's whole managed set — kept on the scope so org-scoped gates
    /// (`require_org_scope`) and the console listings read one field.
    pub org_ids: Vec<Uuid>,
}

impl PartnerScope {
    pub fn allows(&self, cap: PartnerCapability) -> bool {
        self.capabilities.allows(cap)
    }
}

/// Resolve `user`'s authority within the partner org `partner_org_id`, or `None`.
///
/// Fails closed at every step: no partner grant, a suspended grant, not a member of
/// the partner org, no partner-access binding, or any DB error.
///
/// The operator + ceiling determination comes from [`operated_partners`] — the single
/// source that the authz fact loader also reads, so the console and the authz model
/// cannot drift apart on who operates what.
pub async fn resolve_scope(
    db: &DatabaseConnection,
    partner_org_id: Uuid,
    user_id: Uuid,
    _user_email: &str,
) -> Option<PartnerScope> {
    let grant = PartnerGrants::find_by_id(partner_org_id)
        .one(db)
        .await
        .ok()??;
    if grant.status != "active" {
        return None;
    }

    let org = Organizations::find_by_id(partner_org_id)
        .one(db)
        .await
        .ok()??;

    // Real operator first; failing that, a live assume session. Both now come from the
    // same standings, so the console's scope and the authz facts cannot disagree about
    // what a partner's ceiling is.
    let standing = operated_partners_for_user(db, user_id)
        .await
        .into_iter()
        .find(|p| p.partner_id == partner_org_id);
    let standing = match standing {
        Some(p) => Some(p),
        None => {
            let assumed = assumed_partners(db, user_id, is_oxy_staff(db, _user_email).await)
                .await
                .into_iter()
                .find(|p| p.partner_id == partner_org_id);
            if assumed.is_some() {
                tracing::warn!(
                    actor = %_user_email, partner_org_id = %partner_org_id,
                    "partner_authz: assume-role session active — acting as this partner's admin"
                );
            }
            assumed
        }
    };
    match standing {
        // An operator's authority IS the ceiling — nothing narrows it further, and all
        // operators reach every client the partner manages.
        Some(operated) => Some(PartnerScope {
            partner_id: partner_org_id,
            slug: org.slug,
            capabilities: operated.capabilities,
            org_ids: operated.managed_org_ids,
        }),
        // Neither a real operator nor a live assume session ⇒ a plain 403, exactly like
        // any other non-member.
        None => None,
    }
}

/// Staff reach into the **partner console** — `ManagePartners`, read through the one
/// place that knows what the platform sources are.
///
/// Not `is_staff()`: that is now true for every platform role, so it would let an App
/// Operator operate the partner registry. Capability without scope, matching
/// `Ring::PlatformCap` — the partner registry is Oxy's own surface, and a platform
/// grant's scope names *client* orgs, which is a different set from partner orgs.
async fn is_oxy_staff(db: &DatabaseConnection, email: &str) -> bool {
    crate::globals::platform_holds(db, email, oxy_authz::Cap::ManagePartners).await
}

/// Every client this partner manages.
pub async fn partner_org_ids(db: &DatabaseConnection, partner_org_id: Uuid) -> Vec<Uuid> {
    PartnerOrgs::find()
        .filter(entity::partner_orgs::Column::PartnerOrgId.eq(partner_org_id))
        .all(db)
        .await
        .map(|rows| rows.into_iter().map(|r| r.managed_org_id).collect())
        .unwrap_or_default()
}

/// Whether `user` reaches `org_id`'s custom-app DATA plane through partner
/// delegation: the partner that manages the org, operated by this user (or assumed),
/// whose ceiling grants `develop_apps`.
///
/// The DATA PLANE requires develop_apps — manage_apps is lifecycle only. Ownership still
/// comes from the partner's managed set, so this only holds for orgs it actually manages.
/// Fails closed. Called by the custom-app gate so the console, admin preview and
/// serve/proxy share one decision.
pub async fn partner_grants_app_access(
    db: &DatabaseConnection,
    user_id: Uuid,
    user_email: &str,
    org_id: Uuid,
) -> bool {
    let Some(partner_id) = partner_for_org(db, org_id).await else {
        return false;
    };
    let Some(scope) = resolve_scope(db, partner_id, user_id, user_email).await else {
        return false;
    };
    crate::partner_allows(&scope, Some(org_id), PartnerCapability::DevelopApps)
}

/// One partner a user operates: its ceiling, and the clients it manages.
pub struct OperatedPartner {
    pub partner_id: Uuid,
    pub capabilities: Capabilities,
    pub managed_org_ids: Vec<Uuid>,
}

/// Every partner `memberships`' user operates — **the single source** for "who operates
/// what, with which ceiling".
///
/// [`resolve_scope`] (one partner, for the console) and the authz fact loader (all of
/// them) both read this, so the two cannot drift. The loader previously hand-rolled
/// these queries and carried a "keep in sync" comment, which is exactly the
/// drift-between-siblings failure the authz work exists to remove.
///
/// An operator is: a member of an org that holds an ACTIVE partner grant, who also
/// holds partner access (a `partner_role_bindings` row). Bounded — four queries
/// regardless of how many orgs the user belongs to — and it short-circuits to empty for
/// the common non-partner user. Deliberately excludes the staff assume-role path: that
/// is per-request override state, not a standing fact. Fail-closed: a DB error yields
/// nothing.
pub async fn operated_partners(
    db: &DatabaseConnection,
    memberships: &[entity::org_members::Model],
) -> Vec<OperatedPartner> {
    if memberships.is_empty() {
        return Vec::new();
    }
    let member_org_ids: Vec<Uuid> = memberships.iter().map(|m| m.org_id).collect();
    let active: std::collections::HashSet<Uuid> = PartnerGrants::find()
        .filter(entity::partner_grants::Column::OrgId.is_in(member_org_ids))
        .filter(entity::partner_grants::Column::Status.eq("active"))
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|g| g.org_id)
        .collect();
    if active.is_empty() {
        return Vec::new();
    }

    let candidates: Vec<Uuid> = memberships
        .iter()
        .filter(|m| active.contains(&m.org_id))
        .map(|m| m.id)
        .collect();
    let bound: std::collections::HashSet<Uuid> = PartnerRoleBindings::find()
        .filter(entity::partner_role_bindings::Column::OrgMemberId.is_in(candidates))
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|b| b.org_member_id)
        .collect();

    let operated: Vec<Uuid> = memberships
        .iter()
        .filter(|m| active.contains(&m.org_id) && bound.contains(&m.id))
        .map(|m| m.org_id)
        .collect();
    if operated.is_empty() {
        return Vec::new();
    }

    standings_for(db, operated).await
}

/// [`operated_partners`] for a user whose memberships aren't already loaded.
pub async fn operated_partners_for_user(
    db: &DatabaseConnection,
    user_id: Uuid,
) -> Vec<OperatedPartner> {
    let memberships = OrgMembers::find()
        .filter(entity::org_members::Column::UserId.eq(user_id))
        .all(db)
        .await
        .unwrap_or_default();
    operated_partners(db, &memberships).await
}

/// Build standings for `partner_org_ids` — their ceilings and their managed clients.
/// Shared by the real-operator path and the assumed path so a standing means the same
/// thing however it was reached.
async fn standings_for(
    db: &DatabaseConnection,
    partner_org_ids: Vec<Uuid>,
) -> Vec<OperatedPartner> {
    if partner_org_ids.is_empty() {
        return Vec::new();
    }
    let client_rows = PartnerOrgs::find()
        .filter(entity::partner_orgs::Column::PartnerOrgId.is_in(partner_org_ids.clone()))
        .all(db)
        .await
        .unwrap_or_default();
    let ceilings = PartnerCapabilities::find()
        .filter(entity::partner_capabilities::Column::OrgId.is_in(partner_org_ids.clone()))
        .all(db)
        .await
        .unwrap_or_default();

    partner_org_ids
        .into_iter()
        .map(|partner_id| OperatedPartner {
            partner_id,
            // No ceiling row means no capabilities — the operator's authority IS the
            // ceiling, so absent means nothing, never everything.
            capabilities: ceilings
                .iter()
                .find(|c| c.org_id == partner_id)
                .map(Capabilities::from_model)
                .unwrap_or_default(),
            managed_org_ids: client_rows
                .iter()
                .filter(|r| r.partner_org_id == partner_id)
                .map(|r| r.managed_org_id)
                .collect(),
        })
        .collect()
}

/// Partners this user is standing in via a live **assume-role session** — Oxy staff
/// looking through a partner's eyes.
///
/// This is the standing `assumed_scope` grants, expressed as a fact so the authz model
/// knows about it too. Without it the model is blind to the override and would deny
/// staff the console, which is what kept `partner_policy` alive: the console's scope
/// knew something the facts didn't.
///
/// Their authority is the partner's ceiling — exactly what the partner's own operators
/// get. Staff do not exceed the ceiling Oxy granted; if a capability is off it stays
/// off, and they see the same walls the customer does. That is the point of looking
/// through their eyes.
///
/// Costs nothing for the 99.9%: non-staff short-circuit before any query, because only
/// staff can assume at all. `is_staff` is passed in rather than re-derived — the caller
/// has already read the platform sources to build its facts, and this sits on the
/// custom-app query hot path.
pub async fn assumed_partners(
    db: &DatabaseConnection,
    user_id: Uuid,
    is_staff: bool,
) -> Vec<OperatedPartner> {
    if !is_staff {
        return Vec::new();
    }
    let assumed = crate::assume_liveness::live_assumed_org_ids(db, user_id).await;
    if assumed.is_empty() {
        return Vec::new();
    }
    // An assume session over a non-partner org (a plain tenant) confers no partner
    // standing, and a suspended grant confers nothing at all.
    let partner_orgs: Vec<Uuid> = PartnerGrants::find()
        .filter(entity::partner_grants::Column::OrgId.is_in(assumed))
        .filter(entity::partner_grants::Column::Status.eq("active"))
        .all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|g| g.org_id)
        .collect();
    standings_for(db, partner_orgs).await
}

/// The partner managing `org_id`, if any. `managed_org_id` is UNIQUE, so at most one.
pub async fn partner_for_org(db: &DatabaseConnection, org_id: Uuid) -> Option<Uuid> {
    PartnerOrgs::find()
        .filter(entity::partner_orgs::Column::ManagedOrgId.eq(org_id))
        .one(db)
        .await
        .ok()?
        .map(|r| r.partner_org_id)
}

/// Whether `partner_org_id` manages `org_id`.
pub async fn partner_owns_org(db: &DatabaseConnection, partner_org_id: Uuid, org_id: Uuid) -> bool {
    partner_for_org(db, org_id).await == Some(partner_org_id)
}

/// Every partner this user holds a role in (drives the console entry).
pub async fn scopes_for_user(
    db: &DatabaseConnection,
    user_id: Uuid,
    user_email: &str,
) -> Vec<PartnerScope> {
    let memberships = OrgMembers::find()
        .filter(entity::org_members::Column::UserId.eq(user_id))
        .all(db)
        .await
        .unwrap_or_default();

    let mut org_ids: Vec<Uuid> = memberships.into_iter().map(|m| m.org_id).collect();

    // Orgs the caller is ACTING AS. Without this a staff member who assumed a
    // partner would resolve a scope on `/partners/{id}` but never see the partner
    // listed — the console would be empty and the mode would look broken.
    for session in crate::assume_liveness::live_sessions_for(db, user_id).await {
        if !org_ids.contains(&session.org_id) {
            org_ids.push(session.org_id);
        }
    }

    let mut out = Vec::new();
    for org_id in org_ids {
        if let Some(scope) = resolve_scope(db, org_id, user_id, user_email).await {
            out.push(scope);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_fail_closed() {
        let c = Capabilities::default();
        for cap in PartnerCapability::ALL {
            assert!(!c.allows(cap), "{cap:?} must be denied by default");
        }
    }

    /// An operator's authority IS the ceiling — what Oxy grants the partner is the
    /// whole story. A capability the ceiling withholds is simply absent, and one it
    /// grants is present, with no role in between.
    #[test]
    fn authority_is_exactly_the_ceiling() {
        let mut ceiling = Capabilities::default();
        ceiling.set(PartnerCapability::ManageApps, true);
        ceiling.set(PartnerCapability::ManageMembers, true);

        // Present because the ceiling grants them.
        assert!(ceiling.allows(PartnerCapability::ManageApps));
        assert!(ceiling.allows(PartnerCapability::ManageMembers));
        // Absent because the ceiling withholds them — the data plane and billing
        // stay denied no matter who the operator is.
        assert!(!ceiling.allows(PartnerCapability::DevelopApps));
        assert!(!ceiling.allows(PartnerCapability::ManageBilling));
    }

    #[test]
    fn granted_names_match_the_capability_strings() {
        let mut c = Capabilities::default();
        c.set(PartnerCapability::CreateOrgs, true);
        c.set(PartnerCapability::ViewAudit, true);
        let names = c.granted_names();
        assert!(names.contains(&"create_orgs"));
        assert!(names.contains(&"view_audit"));
        assert!(!names.contains(&"develop_apps"));
    }

    /// Acting as a partner shows you what that partner's BOSS sees — never more.
    ///
    /// Staff do not out-rank the ceiling Oxy granted. If `develop_apps` is off for
    /// this partner, it is off for the operator impersonating them too, and they
    /// hit the same wall the customer would. An impersonation that quietly exceeds
    /// the impersonated identity isn't a view — it's a backdoor.
    #[test]
    fn acting_as_a_partner_is_capped_by_the_ceiling() {
        let ceiling = Capabilities {
            manage_apps: true,
            view_audit: true,
            ..Default::default()
        };
        // Acting-as resolves the operator's authority straight to the ceiling.
        let assumed = ceiling;

        assert!(assumed.allows(PartnerCapability::ManageApps));
        assert!(assumed.allows(PartnerCapability::ViewAudit));
        // The partner was never granted these, so neither is the operator.
        assert!(!assumed.allows(PartnerCapability::DevelopApps));
        assert!(!assumed.allows(PartnerCapability::ManageMembers));
        assert!(!assumed.allows(PartnerCapability::CreateOrgs));
        assert!(!assumed.allows(PartnerCapability::ManageBilling));
    }
}
