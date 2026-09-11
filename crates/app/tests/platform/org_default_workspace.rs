//! Every new org arrives with exactly one Ready workspace — or not at all.
//!
//! Customers no longer create orgs: Oxy staff (`POST /admin/orgs`) and partners
//! onboard them. The owner's first sign-in should land on Home, which exists
//! only under a workspace route, so creation leaves a Ready `Default` workspace
//! behind — its row committed with the org's, its working copy on disk.
//!
//! The second case is the half that matters when it goes wrong: a workspace
//! that cannot be made must take the org down with it, not leave a tenant that
//! looks created and has nowhere to land.
//!
//! Drives the real handler against a per-test database (`common::test_db`, which
//! also points the process's state dir at a temp dir, so the working copy lands
//! there). Global Owner standing comes from `OXY_OWNER`, the one standing that
//! needs no grant row; nextest gives each test its own process, so setting it
//! here reaches no other test.

use std::path::Path;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use entity::workspaces::WorkspaceStatus;
use entity::{org_members, organizations, users, workspaces};
use oxy_app::server::api::admin::orgs_admin::{AdminCreateOrgBody, create_org};
use oxy_app::server::api::middlewares::org_context::{OrgContext, OrgContextExtractor};
use oxy_app::server::api::workspaces::list_workspaces;
use oxy_app::server::router::bare_app_state;
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_auth::types::AuthenticatedUser;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use uuid::Uuid;

use crate::common::test_db;

/// A live user who is also the Global Owner, so `PlatformOrgCreate` allows them.
async fn seed_owner(db: &DatabaseConnection) -> users::Model {
    let id = Uuid::new_v4();
    let email = format!("staff-{id}@example.com");
    let user = users::ActiveModel {
        id: ActiveValue::Set(id),
        email: ActiveValue::Set(Some(email.clone())),
        name: ActiveValue::Set("Staff".into()),
        picture: ActiveValue::Set(None),
        email_verified: ActiveValue::Set(true),
        status: ActiveValue::Set(users::UserStatus::Active),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed staff user");
    // SAFETY: process-per-test (asserted by `test_db`), before any request runs.
    unsafe { std::env::set_var("OXY_OWNER", &email) };
    user
}

fn actor(user: &users::Model) -> AuthenticatedUserExtractor {
    AuthenticatedUserExtractor(AuthenticatedUser {
        id: user.id,
        email: user.email.clone(),
        name: user.name.clone(),
        picture: None,
        status: users::UserStatus::Active,
    })
}

/// The owner is the staff member themselves: an existing user is seeded as a
/// member, so no invitation email is sent.
fn body(user: &users::Model, slug: &str) -> Json<AdminCreateOrgBody> {
    Json(AdminCreateOrgBody {
        name: "Default Workspace Co".into(),
        slug: Some(slug.to_string()),
        owner_email: user.email.clone().expect("seeded with an email"),
    })
}

fn fresh_slug() -> String {
    format!("dw-{}", Uuid::new_v4().simple())
}

#[tokio::test]
async fn admin_create_org_yields_exactly_one_ready_workspace() {
    let db = test_db().await;
    let staff = seed_owner(&db).await;

    let Json(created) = create_org(actor(&staff), HeaderMap::new(), body(&staff, &fresh_slug()))
        .await
        .expect("staff create org");

    let rows = workspaces::Entity::find()
        .filter(workspaces::Column::OrgId.eq(created.org.id))
        .all(&db)
        .await
        .expect("load workspaces");
    assert_eq!(rows.len(), 1, "a new org has exactly one workspace");
    let ws = &rows[0];
    assert_eq!(ws.id, created.default_workspace_id, "the response names it");
    assert_eq!(ws.status, WorkspaceStatus::Ready);
    assert_eq!(ws.name, "Default");
    assert_eq!(
        ws.created_by,
        Some(staff.id),
        "attributed to the staff caller"
    );
    assert_eq!(created.org.workspace_count, 1);

    let dir = ws
        .path
        .as_deref()
        .expect("a workspace row carries its path");
    assert!(
        Path::new(dir).join("config.yml").is_file(),
        "the working copy is scaffolded, not just the row: {dir}"
    );
}

/// A serve replica refuses to scaffold a working copy. Reached through a
/// misclassified route, org creation must fail whole: no org, no workspace.
#[tokio::test]
async fn an_org_whose_workspace_cannot_be_made_is_not_created() {
    let db = test_db().await;
    let staff = seed_owner(&db).await;
    // SAFETY: process-per-test; the role is read once, into a once-cell.
    unsafe { std::env::set_var("OXY_ROLE", "serve") };
    oxy_app::server::role_manifest::init_process_role_from_env();

    let slug = fresh_slug();
    let refused = create_org(actor(&staff), HeaderMap::new(), body(&staff, &slug)).await;
    assert_eq!(
        refused.err(),
        Some(StatusCode::INTERNAL_SERVER_ERROR),
        "a workspace that cannot be made is a server error — never the 409 a \
         client reads as 'slug taken'"
    );

    let org = organizations::Entity::find()
        .filter(organizations::Column::Slug.eq(&slug))
        .one(&db)
        .await
        .expect("look up org");
    assert!(org.is_none(), "the org rolled back with its workspace");
}

/// Cloud refuses a nil-UUID workspace path outright — that id is the legacy
/// `--local` convention — so an org's workspace list must never offer one. A dev
/// database a `--local` run once touched carries a nil "Local" row in its org;
/// listed, the post-login dispatcher picks it and the workspace shell spins on
/// `/meta` 404s forever.
#[tokio::test]
async fn the_workspace_list_never_offers_the_local_mode_nil_workspace() {
    let db = test_db().await;
    let staff = seed_owner(&db).await;
    let Json(created) = create_org(actor(&staff), HeaderMap::new(), body(&staff, &fresh_slug()))
        .await
        .expect("staff create org");

    let now = chrono::Utc::now();
    workspaces::ActiveModel {
        id: ActiveValue::Set(Uuid::nil()),
        name: ActiveValue::Set("Local".into()),
        org_id: ActiveValue::Set(Some(created.org.id)),
        status: ActiveValue::Set(WorkspaceStatus::Ready),
        created_at: ActiveValue::Set(now.into()),
        updated_at: ActiveValue::Set(now.into()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("seed the row a --local run leaves behind");

    let ctx = OrgContext {
        org: organizations::Entity::find_by_id(created.org.id)
            .one(&db)
            .await
            .expect("load org")
            .expect("org exists"),
        membership: org_members::Entity::find()
            .filter(org_members::Column::OrgId.eq(created.org.id))
            .filter(org_members::Column::UserId.eq(staff.id))
            .one(&db)
            .await
            .expect("load membership")
            .expect("staff owns the org"),
        is_global_override: false,
    };

    let Json(listed) = list_workspaces(OrgContextExtractor(ctx), State(bare_app_state()))
        .await
        .expect("list workspaces");
    let ids: Vec<Uuid> = listed.iter().map(|w| w.id).collect();
    assert_eq!(
        ids,
        vec![created.default_workspace_id],
        "only the workspace cloud can open is listed"
    );
}
