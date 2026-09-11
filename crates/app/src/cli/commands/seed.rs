//! `oxy seed` — seed the demo project so local dev / multi-instance demos
//! can compile + serve a workspace immediately after migrations.
//!
//! Seeds:
//! - Local org (nil-UUID, shared with local-mode for consistency)
//! - Demo workspace at a deterministic non-nil UUID, with `path` set to
//!   `./examples` (or `--workspace-path`). Non-nil so the enterprise
//!   `workspace_context` middleware accepts it — that guard 404s nil-UUID
//!   workspaces because nil is the local-mode-only convention.
//! - Every email in `OXY_GLOBAL_ADMINS` as
//!   Owner of Local org, so OAuth login (GitHub / Google) lands in a
//!   workspace already without the user clicking through a setup wizard.
//!
//! Then, from the CLI ([`SeedOptions`]): LLM provider keys from the environment
//! become workspace secrets (`seed_llm_keys`), and every seeded workspace is
//! compiled + promoted (`seed_compile`) so it serves on the first request.
//!
//! Idempotent — safe to re-run. Already-bound emails are skipped.

use std::path::PathBuf;

use airhouse::LOCAL_ORG_ID;
use chrono::Utc;
use entity::org_members::{self, OrgRole};
use entity::organizations;
use entity::prelude::{OrgMembers, Organizations, Workspaces};
use entity::workspaces::{self, WorkspaceStatus};
use oxy::database::client::establish_connection;
use oxy::theme::StyledText;
use oxy_auth::types::Identity;
use oxy_auth::user::UserService;
use oxy_shared::errors::OxyError;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
};
use uuid::Uuid;

/// Deterministic UUID for the demo workspace. UUID v5 over a fixed name in
/// the DNS namespace — same input → same output across machines, so a fresh
/// clone + `oxy seed` lands on the same workspace_id and any saved IDE
/// state / bookmarks stay valid.
fn demo_workspace_id() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"demo.oxy.local")
}

/// What `oxy seed` does beyond writing rows. The CLI compiles by default and
/// copies keys only under `--llm-keys`; the integration-test fixture
/// ([`seed_demo`]) leaves both off — a test wants rows, not ten compiles and a
/// copy of whatever keys its shell exports.
#[derive(Clone, Copy, Debug)]
pub struct SeedOptions {
    /// Compile + promote every seeded workspace. `--no-compile` clears it.
    pub compile: bool,
    /// Copy the LLM keys each workspace's `config.yml` references from the
    /// environment into its secrets store (`--llm-keys`; local DB only, never
    /// overwriting).
    pub llm_keys: bool,
}

/// Seed rows only — the fixture integration tests drive. See [`seed_demo_with`].
pub async fn seed_demo(workspace_path: Option<PathBuf>) -> Result<(), OxyError> {
    let rows_only = SeedOptions {
        compile: false,
        llm_keys: false,
    };
    seed_demo_with(workspace_path, rows_only).await
}

/// Run the full demo seed. `workspace_path` defaults to `./examples`.
///
/// A compile failure is returned only after everything else — rows, keys, the
/// other workspaces' compiles — has run, so a wrapper script fails visibly
/// without a single bad file leaving the box half-seeded.
pub async fn seed_demo_with(
    workspace_path: Option<PathBuf>,
    options: SeedOptions,
) -> Result<(), OxyError> {
    let resolved = resolve_workspace_path(workspace_path)?;
    let resolved_str = resolved.to_string_lossy().to_string();
    let workspace_id = demo_workspace_id();
    let conn = seed_rows(workspace_id, &resolved_str).await?;

    let mut compiled = Ok(());
    if options.compile || options.llm_keys {
        let targets = super::seed_compile::seeded_workspaces(&conn, workspace_id).await?;
        // Warns instead of failing: chat without a key is a clear, fixable error
        // in the product, and the rest of the seed is what a developer needs.
        if options.llm_keys
            && let Err(e) = super::seed_llm_keys::store_llm_keys(&conn, &targets).await
        {
            println!("{} LLM keys not stored: {e}", "⚠️".warning());
        }
        if options.compile {
            compiled = super::seed_compile::compile_seeded(&targets).await;
        }
    }

    print_next_steps(options.compile, workspace_id, &resolved_str);
    compiled
}

fn print_next_steps(compiled: bool, workspace_id: Uuid, path: &str) {
    println!();
    println!("Next:");
    if !compiled {
        println!(
            "  cargo run -p oxy-server -- compile --workspace-path {path} \
             --workspace-id {workspace_id} --promote --skip-migrations"
        );
    }
    println!("  OXY_ROLE=ide cargo run -p oxy-server -- serve --enterprise");
    println!(
        "  http://localhost:5173/dev-login?as=member   (staff | owner | member | operator | partner)"
    );
}

/// The personas `/api/auth/dev-login?as=` names, derived from what this seed
/// creates. `staff` is absent: it is the operator's own roster, not seed data.
#[cfg(test)]
pub(crate) fn seeded_persona_emails() -> Vec<(&'static str, String)> {
    let mut personas = super::seed_partners::acme_persona_emails();
    let operator = super::seed_platform_grants::unscoped_app_operator_email()
        .expect("the seed grants an unscoped App Operator");
    personas.push(("operator", operator.to_string()));
    personas
}

/// Every row the seed owns: the Local org + demo workspace, `OXY_GLOBAL_ADMINS`
/// bound as Owners of Local (so OAuth login lands in a workspace already; skipped
/// when unset, and a `.env` carrying only the removed `OXY_APP_ADMINS` gets an
/// error logged rather than binding nobody in silence), the tenant/partner orgs,
/// platform grants, example apps and demo threads.
async fn seed_rows(
    workspace_id: Uuid,
    resolved_str: &str,
) -> Result<sea_orm::DatabaseConnection, OxyError> {
    println!(
        "{} seeding demo (workspace_id={}, path={})",
        "🌱".info(),
        workspace_id,
        resolved_str
    );

    let conn = establish_connection().await?;
    ensure_local_org(&conn).await?;
    ensure_demo_workspace(&conn, workspace_id, resolved_str).await?;

    println!(
        "{} workspace {} → {}",
        "✅".success(),
        workspace_id,
        resolved_str
    );

    bind_org_admin_emails(&conn).await?;

    // Fold in the multi-tenant + partner test data (orgs, generated users,
    // partnerships, workspaces) so the admin cockpit + partner console have
    // realistic data out of the box — no separate `--partners` step. Each seeded
    // workspace points at the demo project (`resolved_str`). Skips on a non-local
    // DB, so this stays safe to run anywhere the demo-workspace seed runs.
    super::seed_partners::seed_partner_tenants(resolved_str).await?;

    // After the tenants (the Acme-scoped grant needs Acme to exist) and before the
    // apps summary, so the output reads: tenants → staff → what they can see.
    super::seed_platform_grants::seed_platform_grants().await?;

    deploy_example_apps(&conn, workspace_id, resolved_str).await;

    // After `bind_org_admin_emails`: threads are user-scoped, and this
    // materializes one copy per Local-org member — the memberships that call
    // creates. Run before it and there is nobody to own a thread.
    //
    // Warns instead of failing, like the example apps: demo chat history is a
    // convenience on top of a workspace that is already usable without it.
    if let Err(e) = super::seed_threads::seed_demo_threads(&conn, workspace_id).await {
        println!("{} demo threads not seeded: {e}", "⚠️".warning());
    }
    Ok(conn)
}

/// Deploy the example custom app to the demo workspace, and to Acme's (so the
/// admin cockpit and the partner console both have a real app to show).
///
/// **Warns instead of failing.** The rest of the seed is the part a developer
/// can't work without; the example app is a bonus on top. The one failure that's
/// actually likely — `OXY_ROLE` exported in the shell, which makes the build
/// store refuse a filesystem write — would otherwise turn a stray env var into a
/// completely failed seed.
async fn deploy_example_apps(conn: &sea_orm::DatabaseConnection, workspace_id: Uuid, path: &str) {
    let mut targets = vec![super::seed_apps::AppTarget::open(
        LOCAL_ORG_ID,
        "local".to_string(),
        workspace_id,
    )];
    // Absent when the partner seed skipped (non-local DB) — deploy to the demo
    // workspace anyway rather than treating that as an error.
    match super::seed_apps::seeded_app_workspace_of(conn, "acme").await {
        Ok(Some(acme)) => {
            // A SECOND, restricted app beside the open one — never instead of it.
            // Acme's only app being invisible to most of Acme would read as a broken
            // seed; two apps show the contrast the feature is about, with the open
            // one still on everyone's launcher.
            if let Some(team) = super::seed_partners::restricted_team_for("acme") {
                targets.push(acme.restricted(team));
            }
            targets.push(acme);
        }
        Ok(None) => {}
        Err(e) => println!("{} could not resolve Acme's workspace: {e}", "⚠️".warning()),
    }

    if let Err(e) = super::seed_apps::seed_example_apps(conn, path, &targets).await {
        println!(
            "{} example app not deployed: {e}\n  \
             Everything else seeded. Re-run `oxy seed` once the cause is fixed.",
            "⚠️".warning()
        );
    }

    // After the apps exist — storage usage hangs off `apps.id` (with an
    // ON DELETE CASCADE foreign key), so seeding it earlier would insert
    // nothing. Warn rather than fail: demo storage numbers are a nicety, and
    // the rest of the seed is what a developer cannot work without.
    if let Err(e) = super::seed_storage::seed_storage_usage(conn).await {
        println!("{} storage usage not seeded: {e}", "⚠️".warning());
    }
}

/// Ensure the Local organization exists at LOCAL_ORG_ID (nil). Shared with
/// local-mode's seed for consistency; FK constraints on airhouse_tenants
/// reference this id.
async fn ensure_local_org(conn: &sea_orm::DatabaseConnection) -> Result<(), OxyError> {
    if Organizations::find_by_id(LOCAL_ORG_ID)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query Local org: {e}")))?
        .is_some()
    {
        return Ok(());
    }
    let now = Utc::now().fixed_offset();
    organizations::ActiveModel {
        id: ActiveValue::Set(LOCAL_ORG_ID),
        name: ActiveValue::Set("Local".to_string()),
        slug: ActiveValue::Set("local".to_string()),
        logo: ActiveValue::NotSet,
        logo_content_type: ActiveValue::NotSet,
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert Local org: {e}")))?;
    Ok(())
}

/// Ensure the demo workspace row exists at the given (non-nil) UUID,
/// pointing at the resolved path. On re-runs, patches the `path` if the
/// row exists but the path differs.
async fn ensure_demo_workspace(
    conn: &sea_orm::DatabaseConnection,
    workspace_id: Uuid,
    path: &str,
) -> Result<(), OxyError> {
    let existing = Workspaces::find_by_id(workspace_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query demo workspace: {e}")))?;
    let now = Utc::now().fixed_offset();
    if let Some(row) = existing {
        if row.path.as_deref() == Some(path) {
            return Ok(());
        }
        let mut active = row.into_active_model();
        active.path = ActiveValue::Set(Some(path.to_string()));
        active.updated_at = ActiveValue::Set(now);
        active
            .update(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("update demo workspace path: {e}")))?;
        return Ok(());
    }
    workspaces::ActiveModel {
        id: ActiveValue::Set(workspace_id),
        name: ActiveValue::Set("Demo".to_string()),
        git_namespace_id: ActiveValue::Set(None),
        git_remote_url: ActiveValue::Set(None),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
        path: ActiveValue::Set(Some(path.to_string())),
        last_opened_at: ActiveValue::Set(None),
        created_by: ActiveValue::Set(None),
        org_id: ActiveValue::Set(Some(LOCAL_ORG_ID)),
        status: ActiveValue::Set(WorkspaceStatus::Ready),
        error: ActiveValue::Set(None),
        monthly_vlm_budget_micros: ActiveValue::Set(None),
        current_revision_id: ActiveValue::Set(None),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert demo workspace: {e}")))?;
    Ok(())
}

/// Bind every email in OXY_GLOBAL_ADMINS as Owner of the Local org.
/// Idempotent — already-bound emails are skipped. When the env is not set,
/// returns Ok without action.
async fn bind_org_admin_emails(conn: &sea_orm::DatabaseConnection) -> Result<(), OxyError> {
    // This path lost the OXY_APP_ADMINS fallback too, and `oxy seed` never
    // touches the serve boot that carries the same warning — so without this
    // call it would bind nobody and say nothing, which is the exact silent
    // failure the removal is supposed to announce.
    crate::server::api::custom_apps_auth::warn_on_removed_legacy_admins_env();
    let raw = std::env::var("OXY_GLOBAL_ADMINS").unwrap_or_default();
    let parsed: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if parsed.is_empty() {
        return Ok(());
    }
    println!(
        "{} binding {} email{} from OXY_GLOBAL_ADMINS as Owner of Local",
        "🔗".info(),
        parsed.len(),
        if parsed.len() == 1 { "" } else { "s" }
    );
    let mut bound = 0u32;
    let mut skipped = 0u32;
    for email in &parsed {
        let user = UserService::get_or_create_user(&Identity {
            // A seed path: this must be able to mint the row, so no id.
            user_id: None,
            email: email.clone(),
            name: Some(email.split('@').next().unwrap_or(email).to_string()),
            picture: None,
        })
        .await?;

        let existing = OrgMembers::find()
            .filter(org_members::Column::OrgId.eq(LOCAL_ORG_ID))
            .filter(org_members::Column::UserId.eq(user.id))
            .one(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("query membership for {email}: {e}")))?;
        if existing.is_some() {
            skipped += 1;
            continue;
        }

        let now = Utc::now().fixed_offset();
        org_members::ActiveModel {
            id: ActiveValue::Set(Uuid::new_v4()),
            org_id: ActiveValue::Set(LOCAL_ORG_ID),
            user_id: ActiveValue::Set(user.id),
            role: ActiveValue::Set(OrgRole::Owner),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
        }
        .insert(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("insert membership for {email}: {e}")))?;
        bound += 1;
    }
    println!(
        "  {} {bound} newly bound, {skipped} already Owner",
        "✅".success()
    );
    Ok(())
}

/// Drop the demo workspace row and every seeded example app. Org + guest user
/// are left in place since other code paths (Airhouse provision, customer-apps
/// demos) depend on the nil-UUID org existing.
pub async fn clear_demo() -> Result<(), OxyError> {
    let conn = establish_connection().await?;

    // Apps first. `apps.project_id` carries no foreign key, so deleting the
    // workspace does NOT cascade to them — they'd survive as rows pointing at a
    // workspace that no longer exists, and their bundle bytes would sit on disk
    // with nothing left to reference them.
    // Before the apps: the FK would cascade these away, but `--clear` also runs
    // where apps survive, and orphaned usage keeps counting against an org's quota.
    if let Err(e) = super::seed_storage::clear_storage_usage(&conn).await {
        println!("{} storage usage not cleared: {e}", "⚠️".warning());
    }

    // Before the workspace delete: `threads.project_id` cascades from
    // `workspaces`, so dropping the demo workspace would take these with it —
    // but `--clear` also runs on boxes where the workspace row is already gone
    // or was re-pointed, and a seeded thread nobody can reach still shows up in
    // its owner's history.
    match super::seed_threads::clear_demo_threads(&conn).await {
        Ok(n) if n > 0 => println!(
            "{} cleared {n} seeded thread{}",
            "🧹".info(),
            if n == 1 { "" } else { "s" }
        ),
        Ok(_) => {}
        Err(e) => println!("{} seeded threads not cleared: {e}", "⚠️".warning()),
    }

    let apps = super::seed_apps::clear_example_apps(&conn).await?;

    // Staff grants are keyed by email, not by workspace or org, so nothing else in
    // this teardown reaches them — left behind, they would keep granting console
    // access to accounts the developer believes they deleted.
    let grants = super::seed_platform_grants::clear_platform_grants(&conn).await?;
    if grants > 0 {
        println!(
            "{} cleared {grants} seeded platform grant{}",
            "🧹".info(),
            if grants == 1 { "" } else { "s" }
        );
    }

    let deleted = Workspaces::delete_by_id(demo_workspace_id())
        .exec(&conn)
        .await
        .map_err(|e| OxyError::DBError(format!("delete demo workspace: {e}")))?;
    println!(
        "{} cleared demo workspace ({} row{}) and {apps} example app{}",
        "🧹".info(),
        deleted.rows_affected,
        if deleted.rows_affected == 1 { "" } else { "s" },
        if apps == 1 { "" } else { "s" }
    );
    Ok(())
}

fn resolve_workspace_path(path: Option<PathBuf>) -> Result<PathBuf, OxyError> {
    let raw = path.unwrap_or_else(|| PathBuf::from("./examples"));
    let absolute = std::path::absolute(&raw)
        .map_err(|e| OxyError::RuntimeError(format!("absolute path for {raw:?}: {e}")))?;
    if !absolute.exists() {
        return Err(OxyError::RuntimeError(format!(
            "workspace path does not exist: {}",
            absolute.display()
        )));
    }
    Ok(absolute)
}
