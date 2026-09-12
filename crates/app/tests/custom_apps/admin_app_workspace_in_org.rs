//! The admin app registry never points an app at a workspace outside its own org —
//! not when it creates the row, and not when it moves it.
//!
//! `apps.project_id` is what `window.__OXY_APP__.projectId` is injected from, and
//! the bundle's connectors, secrets and data plane resolve against it. Both the
//! admin create (`POST /api/customer-apps`) and the admin PATCH
//! used to write it straight from the body, so one mistyped id put this org's app
//! on another tenant's data. A publish refuses to move an app, so this PATCH is the
//! supported way to move one — which is why it has to hold the line.
//!
//! Drives the real handlers against a per-test database (`common::test_db`). Global
//! Owner standing comes from `OXY_OWNER`; nextest gives each test its own process,
//! so setting it here reaches no other test.

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use entity::{apps, organizations, users, workspaces};
use oxy_app::server::api::admin::apps::handlers::{create_app, update_app};
use oxy_auth::extractor::AuthenticatedUserExtractor;
use oxy_auth::types::AuthenticatedUser;
use sea_orm::{ActiveModelTrait, ActiveValue, DatabaseConnection, EntityTrait};
use serde_json::json;
use uuid::Uuid;

use crate::common::test_db;

/// Two orgs: A owns the app and two workspaces, B owns one workspace.
struct Fixture {
    owner: users::Model,
    org_a: Uuid,
    app_id: Uuid,
    workspace_a1: Uuid,
    workspace_a2: Uuid,
    workspace_b: Uuid,
}

async fn seed_org(db: &DatabaseConnection) -> Uuid {
    let id = Uuid::new_v4();
    organizations::ActiveModel {
        id: ActiveValue::Set(id),
        name: ActiveValue::Set("Update App Org".into()),
        slug: ActiveValue::Set(format!("upd-app-{}", &id.simple().to_string()[..12])),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed org");
    id
}

async fn seed_workspace(db: &DatabaseConnection, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    workspaces::ActiveModel {
        id: ActiveValue::Set(id),
        name: ActiveValue::Set(format!("ws-{id}")),
        org_id: ActiveValue::Set(Some(org_id)),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed workspace");
    id
}

async fn seed(db: &DatabaseConnection) -> Fixture {
    let org_a = seed_org(db).await;
    let org_b = seed_org(db).await;
    let workspace_a1 = seed_workspace(db, org_a).await;
    let workspace_a2 = seed_workspace(db, org_a).await;
    let workspace_b = seed_workspace(db, org_b).await;

    let user_id = Uuid::new_v4();
    let email = format!("staff-{user_id}@example.com");
    let owner = users::ActiveModel {
        id: ActiveValue::Set(user_id),
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

    let app_id = Uuid::new_v4();
    apps::ActiveModel {
        id: ActiveValue::Set(app_id),
        slug: ActiveValue::Set("move-me".into()),
        name: ActiveValue::Set("Move Me".into()),
        org_id: ActiveValue::Set(org_a),
        project_id: ActiveValue::Set(workspace_a1),
        branch: ActiveValue::Set("main".into()),
        source_repo: ActiveValue::Set("oxy-hq/customer-apps".into()),
        status: ActiveValue::Set("active".into()),
        source_type: ActiveValue::Set("s3".into()),
        source_config: ActiveValue::Set(json!({})),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed app");

    Fixture {
        owner,
        org_a,
        app_id,
        workspace_a1,
        workspace_a2,
        workspace_b,
    }
}

/// The handler's result, reduced to what the assertions need: the response's
/// `project_id` and `name`, or the refusal's status and message.
async fn patch(
    f: &Fixture,
    body: serde_json::Value,
) -> Result<(Uuid, String), (StatusCode, String)> {
    let req = serde_json::from_value(body).expect("a valid UpdateAppRequest body");
    update_app(actor(f), Path(f.app_id), Json(req))
        .await
        .map(|Json(resp)| (resp.project_id, resp.name))
        .map_err(|(status, Json(err))| (status, err.message))
}

/// The seeded Global Owner as the request principal.
fn actor(f: &Fixture) -> AuthenticatedUserExtractor {
    AuthenticatedUserExtractor(AuthenticatedUser {
        id: f.owner.id,
        email: f.owner.email.clone(),
        name: f.owner.name.clone(),
        picture: None,
        status: users::UserStatus::Active,
    })
}

/// Creates an app named "Created App" in `org_id` pointing at `project_id`, and
/// returns the new row's `id` and `project_id`, or the refusal's status and message.
///
/// Drives the `create_app` handler as the Global Owner, whose scope reaches every
/// org, so the only refusal left to prove is the workspace-in-org rule. The
/// unscoped registration it delegates to is private: the handler is the one way
/// in. `scaffold_pr: false` keeps GitHub out of it.
async fn create(
    f: &Fixture,
    org_id: Uuid,
    project_id: Uuid,
) -> Result<(Uuid, Uuid), (StatusCode, String)> {
    let req = serde_json::from_value(json!({
        "name": "Created App",
        "org_id": org_id,
        "project_id": project_id,
        "scaffold_pr": false,
    }))
    .expect("a valid CreateAppRequest body");
    create_app(actor(f), Json(req))
        .await
        .map(|Json(resp)| (resp.id, resp.project_id))
        .map_err(|(status, Json(err))| (status, err.message))
}

async fn reload(db: &DatabaseConnection, app_id: Uuid) -> apps::Model {
    apps::Entity::find_by_id(app_id)
        .one(db)
        .await
        .expect("load app")
        .expect("app row exists")
}

async fn all_app_ids(db: &DatabaseConnection) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = apps::Entity::find()
        .all(db)
        .await
        .expect("list apps")
        .into_iter()
        .map(|a| a.id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn creating_with_a_workspace_in_the_same_org_succeeds() {
    let db = test_db().await;
    let f = seed(&db).await;

    let (app_id, project_id) = create(&f, f.org_a, f.workspace_a2)
        .await
        .expect("a workspace of the app's own org is a valid target");

    assert_eq!(project_id, f.workspace_a2);
    let row = reload(&db, app_id).await;
    assert_eq!((row.org_id, row.project_id), (f.org_a, f.workspace_a2));
}

#[tokio::test]
async fn creating_with_another_orgs_workspace_is_refused_and_writes_no_row() {
    let db = test_db().await;
    let f = seed(&db).await;
    let before = all_app_ids(&db).await;

    let (status, message) = create(&f, f.org_a, f.workspace_b)
        .await
        .expect_err("a workspace of another org must be refused");

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{message}");
    assert_eq!(all_app_ids(&db).await, before);
}

#[tokio::test]
async fn creating_with_a_nonexistent_workspace_is_refused_and_writes_no_row() {
    let db = test_db().await;
    let f = seed(&db).await;
    let before = all_app_ids(&db).await;

    let (status, message) = create(&f, f.org_a, Uuid::new_v4())
        .await
        .expect_err("a workspace that does not exist must be refused");

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{message}");
    assert_eq!(all_app_ids(&db).await, before);
}

#[tokio::test]
async fn moving_to_a_workspace_in_the_same_org_succeeds() {
    let db = test_db().await;
    let f = seed(&db).await;

    let (project_id, _) = patch(&f, json!({ "project_id": f.workspace_a2 }))
        .await
        .expect("a workspace of the app's own org is a valid target");

    assert_eq!(project_id, f.workspace_a2);
    assert_eq!(reload(&db, f.app_id).await.project_id, f.workspace_a2);
}

#[tokio::test]
async fn moving_to_another_orgs_workspace_is_refused_and_leaves_the_row_alone() {
    let db = test_db().await;
    let f = seed(&db).await;
    let before = reload(&db, f.app_id).await;

    let (status, message) = patch(
        &f,
        json!({ "project_id": f.workspace_b, "name": "Should Not Stick" }),
    )
    .await
    .expect_err("a workspace of another org must be refused");

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{message}");
    // The whole row, `updated_at` included: the refusal lands before any write,
    // so the name in the same body did not stick either.
    assert_eq!(reload(&db, f.app_id).await, before);
}

#[tokio::test]
async fn moving_to_a_nonexistent_workspace_is_refused_like_another_orgs() {
    let db = test_db().await;
    let f = seed(&db).await;
    let before = reload(&db, f.app_id).await;
    let missing = Uuid::new_v4();

    let (status, missing_msg) = patch(&f, json!({ "project_id": missing }))
        .await
        .expect_err("a workspace that does not exist must be refused");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{missing_msg}");
    assert_eq!(reload(&db, f.app_id).await, before);

    // Indistinguishable from the other-org refusal once the id is masked, so the
    // answer does not reveal that an id exists in some other tenant.
    let (other_status, other_msg) = patch(&f, json!({ "project_id": f.workspace_b }))
        .await
        .expect_err("a workspace of another org must be refused");
    assert_eq!(other_status, status);
    assert_eq!(
        missing_msg.replace(&missing.to_string(), "<id>"),
        other_msg.replace(&f.workspace_b.to_string(), "<id>"),
    );
}

/// Only an actual move is checked. An app can outlive its workspace (no foreign
/// key), and an edit that re-sends the current `project_id` must still save.
#[tokio::test]
async fn resending_a_deleted_current_workspace_does_not_block_the_edit() {
    let db = test_db().await;
    let f = seed(&db).await;
    workspaces::Entity::delete_by_id(f.workspace_a1)
        .exec(&db)
        .await
        .expect("delete the app's workspace");

    let (project_id, name) = patch(
        &f,
        json!({ "project_id": f.workspace_a1, "name": "Renamed" }),
    )
    .await
    .expect("re-sending the current project_id is not a move");

    assert_eq!(project_id, f.workspace_a1);
    assert_eq!(name, "Renamed");
}
