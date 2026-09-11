//! Seed **platform grants** — the staff roles `oxy seed` creates so the capability
//! model can actually be exercised locally.
//!
//! Without these, an App Operator exists only in tests. A developer opening `/admin`
//! is a Global Owner (via `OXY_OWNER`) or nothing, so the two things this model is
//! *for* — a narrowed console, and a grant bounded to specific orgs — are invisible
//! until someone hand-writes rows.
//!
//! Three grants, chosen to make the two axes visible side by side:
//!
//! | Email | Role | Scope | What it demonstrates |
//! | --- | --- | --- | --- |
//! | `global-admin@oxy.local` | `global_admin` | all orgs | the contrast case — full console |
//! | `app-operator@oxy.local` | `app_operator` | all orgs | **capabilities**: Custom apps only, no Orgs/Users/Jobs |
//! | `app-operator-acme@oxy.local` | `app_operator` | Acme only | **scope**: same nav, but the registry shows only Acme's apps |
//!
//! The scoped one is the point. Capability gating is visible from the nav; scope is
//! only visible by comparing two operators' app lists, which needs both to exist.
//!
//! **Guarded by the same locality check as the partner seed.** These rows are staff
//! standing — seeding them against a real database would be handing out platform
//! access, which is categorically worse than seeding a demo tenant. On a non-local DB
//! this skips (matching `seed_partner_tenants`, so a folded `oxy seed` stays safe).
//!
//! Idempotent by email, via the same `ON CONFLICT` upsert the admin API uses — so
//! re-running after editing a grant in the console puts it back to the seeded shape
//! rather than duplicating or erroring.

use chrono::Utc;
use entity::prelude::{AppAdmins, Organizations};
use entity::{app_admin_scope_orgs, app_admins, organizations};
use oxy::database::client::establish_connection;
use oxy::theme::StyledText;
use oxy_auth::types::Identity;
use oxy_auth::user::UserService;
use oxy_authz::PlatformRole;
use oxy_shared::errors::OxyError;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
    sea_query::OnConflict,
};
use uuid::Uuid;

/// One seeded staff account.
struct GrantSeed {
    email: &'static str,
    name: &'static str,
    role: PlatformRole,
    /// `None` = every org. `Some(slug)` = bounded to that org, so the scope path is
    /// exercisable rather than merely implemented.
    scope_org_slug: Option<&'static str>,
    /// Shown in the seed output, so `oxy seed` explains what it just created rather
    /// than leaving the developer to infer it from three similar addresses.
    demonstrates: &'static str,
}

const GRANTS: &[GrantSeed] = &[
    GrantSeed {
        email: "global-admin@oxy.local",
        name: "Gwen Admin",
        role: PlatformRole::GlobalAdmin,
        scope_org_slug: None,
        demonstrates: "full console (the contrast case)",
    },
    GrantSeed {
        email: "app-operator@oxy.local",
        name: "Ollie Operator",
        role: PlatformRole::AppOperator,
        scope_org_slug: None,
        demonstrates: "Custom apps only — no Orgs, Users, Jobs or Compiles",
    },
    GrantSeed {
        // Acme because `deploy_example_apps` publishes there as well as to the demo
        // workspace — so this grant's registry is non-empty AND visibly shorter than
        // the unscoped operator's. Scoping to an org with no apps would look
        // identical to a broken filter.
        email: "app-operator-acme@oxy.local",
        name: "Ada Acme-Only",
        role: PlatformRole::AppOperator,
        scope_org_slug: Some("acme"),
        demonstrates: "same nav, but only Acme's apps in the registry",
    },
];

/// The unscoped App Operator — what `/api/auth/dev-login?as=operator` names.
#[cfg(test)]
pub(crate) fn unscoped_app_operator_email() -> Option<&'static str> {
    GRANTS
        .iter()
        .find(|g| g.role == PlatformRole::AppOperator && g.scope_org_slug.is_none())
        .map(|g| g.email)
}

/// Seed the staff grants. Skips (does not error) on a non-local database, matching
/// `seed_partner_tenants` so the folded `oxy seed` stays safe to run anywhere.
pub async fn seed_platform_grants() -> Result<(), OxyError> {
    if !super::seed_partners::is_local_db() {
        println!(
            "{} skipping platform grants — OXY_DATABASE_URL does not look local",
            "⏭️".info()
        );
        return Ok(());
    }

    let conn = establish_connection().await?;
    println!(
        "{} seeding {} platform grant{}",
        "🔑".info(),
        GRANTS.len(),
        if GRANTS.len() == 1 { "" } else { "s" }
    );

    for grant in GRANTS {
        // Create the user too. A grant is keyed by email and may precede signup by
        // design — but a seeded operator nobody can log in as demonstrates nothing,
        // and this is the same thing `bind_org_admin_emails` does for env admins.
        UserService::get_or_create_user(&Identity {
            // A seed path: this must be able to mint the row, so no id.
            user_id: None,
            email: grant.email.to_string(),
            name: Some(grant.name.to_string()),
            picture: None,
        })
        .await?;

        let scope_org = match grant.scope_org_slug {
            None => None,
            Some(slug) => match find_org(&conn, slug).await? {
                Some(id) => Some(id),
                // The partner seed runs first and creates Acme, so this is unexpected
                // — but a missing org must not silently produce an UNBOUNDED grant.
                // Skip the row entirely; too little reach is recoverable, too much
                // is the bug this whole model exists to prevent.
                None => {
                    println!(
                        "{} skipping {} — org '{slug}' not found, and an unscoped \
                         fallback would grant MORE than intended",
                        "⚠️".warning(),
                        grant.email
                    );
                    continue;
                }
            },
        };

        upsert_grant(&conn, grant, scope_org).await?;

        let reach = match grant.scope_org_slug {
            None => "all orgs".to_string(),
            Some(slug) => format!("org '{slug}' only"),
        };
        println!(
            "   {} {} — {} · {} · {}",
            "•".info(),
            grant.email,
            grant.role.as_str(),
            reach,
            grant.demonstrates
        );
    }

    println!(
        "{} log in as any of the above to see the console each role actually gets",
        "💡".info()
    );
    Ok(())
}

/// Remove the seeded grants. Scope rows cascade on the grant's delete.
pub async fn clear_platform_grants(conn: &DatabaseConnection) -> Result<u64, OxyError> {
    let emails: Vec<String> = GRANTS.iter().map(|g| g.email.to_string()).collect();
    let deleted = AppAdmins::delete_many()
        .filter(app_admins::Column::Email.is_in(emails))
        .exec(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("delete seeded platform grants: {e}")))?;
    Ok(deleted.rows_affected)
}

async fn find_org(conn: &DatabaseConnection, slug: &str) -> Result<Option<Uuid>, OxyError> {
    Ok(Organizations::find()
        .filter(organizations::Column::Slug.eq(slug))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query org {slug}: {e}")))?
        .map(|o| o.id))
}

/// Upsert one grant + its scope rows, mirroring `admin::app_admins::create_app_admin`
/// so the seeded shape and the console-created shape can't drift.
async fn upsert_grant(
    conn: &DatabaseConnection,
    grant: &GrantSeed,
    scope_org: Option<Uuid>,
) -> Result<(), OxyError> {
    let now = Utc::now().fixed_offset();
    AppAdmins::insert(app_admins::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        email: ActiveValue::Set(grant.email.to_string()),
        // No granter: this is machine-issued, and attributing it to a person would
        // make the audit trail lie about who decided it.
        granted_by: ActiveValue::Set(None),
        created_at: ActiveValue::Set(now),
        role: ActiveValue::Set(grant.role.as_str().to_string()),
        scope_all: ActiveValue::Set(scope_org.is_none()),
        updated_at: ActiveValue::Set(now),
    })
    .on_conflict(
        OnConflict::column(app_admins::Column::Email)
            .update_columns([
                app_admins::Column::Role,
                app_admins::Column::ScopeAll,
                app_admins::Column::UpdatedAt,
            ])
            .to_owned(),
    )
    .exec(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("upsert grant {}: {e}", grant.email)))?;

    // Re-read: on the conflict path the row keeps its ORIGINAL id, which is what the
    // scope rows are keyed by.
    let row = AppAdmins::find()
        .filter(app_admins::Column::Email.eq(grant.email))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("read back grant {}: {e}", grant.email)))?
        .ok_or_else(|| OxyError::DBError(format!("grant {} vanished after upsert", grant.email)))?;

    // Replace, never merge — a re-run after someone widened the grant in the console
    // must restore the seeded reach, not union with it.
    app_admin_scope_orgs::Entity::delete_many()
        .filter(app_admin_scope_orgs::Column::AppAdminId.eq(row.id))
        .exec(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("clear scope for {}: {e}", grant.email)))?;

    if let Some(org_id) = scope_org {
        app_admin_scope_orgs::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            app_admin_id: ActiveValue::Set(row.id),
            org_id: ActiveValue::Set(org_id),
            created_at: ActiveValue::NotSet,
            created_by: ActiveValue::Set(None),
        }
        .insert(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("insert scope for {}: {e}", grant.email)))?;
    }
    Ok(())
}
