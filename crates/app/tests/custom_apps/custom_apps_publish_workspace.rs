//! A publish never moves an app to another workspace that still exists.
//!
//! `upsert_app` used to overwrite `apps.project_id` on every publish, drafts
//! included. A draft publish that named a different `--project` therefore moved
//! the LIVE app to that workspace, and with it `window.__OXY_APP__.projectId`,
//! the connectors, the secrets and the schedules it resolves.
//!
//! The one exception is an app whose workspace is no longer a live workspace of
//! its org: deleted, orphaned (`org_id` NULL), or in another org. Its next
//! publish into a workspace of the app's own org re-homes it.
//!
//! These drive the real `publish()` against a per-test database and the
//! filesystem build store, because the refusal is only worth anything if it
//! lands before a single byte or row is written.

use crate::common::test_db;
use entity::{
    app_builds, apps, org_members, org_members::OrgRole, organizations, users, workspaces,
};
use flate2::{Compression, write::GzEncoder};
use oxy_app::server::api::custom_apps_publish::{OrgRef, PublishError, PublishInput, publish};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use uuid::Uuid;

/// An org, an Admin who may publish into it, and two of its workspaces.
struct Tenant {
    org_id: Uuid,
    user_id: Uuid,
    email: String,
    workspace_a: Uuid,
    workspace_b: Uuid,
}

async fn seed_tenant(db: &DatabaseConnection) -> Tenant {
    let org_id = Uuid::new_v4();
    organizations::ActiveModel {
        id: ActiveValue::Set(org_id),
        name: ActiveValue::Set("Publish Workspace Org".into()),
        slug: ActiveValue::Set(format!("pub-ws-{}", &org_id.simple().to_string()[..12])),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed org");

    let user_id = Uuid::new_v4();
    let email = format!("publisher-{user_id}@example.com");
    users::ActiveModel {
        id: ActiveValue::Set(user_id),
        email: ActiveValue::Set(Some(email.clone())),
        name: ActiveValue::Set("Publisher".into()),
        picture: ActiveValue::Set(None),
        email_verified: ActiveValue::Set(true),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed user");

    // An org Admin publishing their own app — no staff standing, no partner.
    org_members::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        org_id: ActiveValue::Set(org_id),
        user_id: ActiveValue::Set(user_id),
        role: ActiveValue::Set(OrgRole::Admin),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed org member");

    let workspace_a = seed_workspace(db, org_id, "Workspace A").await;
    let workspace_b = seed_workspace(db, org_id, "Workspace B").await;

    Tenant {
        org_id,
        user_id,
        email,
        workspace_a,
        workspace_b,
    }
}

async fn seed_workspace(db: &DatabaseConnection, org_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    workspaces::ActiveModel {
        id: ActiveValue::Set(id),
        name: ActiveValue::Set(name.into()),
        org_id: ActiveValue::Set(Some(org_id)),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed workspace");
    id
}

/// The smallest bundle `validate_bundle` accepts: an index.html with a head.
fn bundle() -> Vec<u8> {
    let html = b"<!doctype html><html><head><title>t</title></head><body></body></html>";
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    let mut header = tar::Header::new_gnu();
    header.set_size(html.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "index.html", &html[..])
        .expect("append index.html");
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

fn input(t: &Tenant, slug: &str, project_id: Uuid, build_id: &str) -> PublishInput {
    PublishInput {
        org_ref: Some(OrgRef::Id(t.org_id)),
        app_slug: slug.to_string(),
        project_id,
        branch: None,
        build_id: build_id.to_string(),
        name: None,
        promote: false,
        tarball: bundle(),
        manifest: None,
        source_repo: None,
        commit_sha: None,
        published_by: Some(t.user_id),
        published_by_email: Some(t.email.clone()),
        machine_app_id: None,
    }
}

async fn reload(db: &DatabaseConnection, app_id: Uuid) -> apps::Model {
    apps::Entity::find_by_id(app_id)
        .one(db)
        .await
        .expect("reload app")
        .expect("app exists")
}

async fn build_row(
    db: &DatabaseConnection,
    app_id: Uuid,
    build_id: &str,
) -> Option<app_builds::Model> {
    app_builds::Entity::find()
        .filter(app_builds::Column::AppId.eq(app_id))
        .filter(app_builds::Column::BuildId.eq(build_id))
        .one(db)
        .await
        .expect("query app_builds")
}

#[tokio::test]
async fn a_publish_naming_another_workspace_is_refused_and_moves_nothing() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;
    let slug = "workspace-pinned";

    let first = publish(input(&t, slug, t.workspace_a, "build-1"))
        .await
        .expect("first publish creates the app in workspace A");
    assert!(first.is_new_app);
    let before = reload(&db, first.app_id).await;
    assert_eq!(before.project_id, t.workspace_a);

    // A draft publish of the same org + slug that names workspace B. Before the
    // fix this returned Ok and left the live app bound to workspace B.
    let mut moving = input(&t, slug, t.workspace_b, "build-2");
    moving.branch = Some("feature".to_string());
    moving.name = Some("Renamed".to_string());
    let err = publish(moving)
        .await
        .expect_err("a publish must not move an app to another workspace");
    match &err {
        PublishError::ProjectMismatch {
            app_slug,
            existing_project,
            requested_project,
            ..
        } => {
            assert_eq!(app_slug, slug);
            assert_eq!(*existing_project, t.workspace_a);
            assert_eq!(*requested_project, t.workspace_b);
        }
        other => panic!("expected ProjectMismatch, got {other:?}"),
    }

    // The row is untouched — not just `project_id`, but everything the update
    // path would have written (branch, name, sync stamp, channel pointers).
    let after = reload(&db, first.app_id).await;
    assert_eq!(after.project_id, t.workspace_a);
    assert_eq!(after.branch, before.branch);
    assert_eq!(after.name, before.name);
    assert_eq!(after.last_synced_at, before.last_synced_at);
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(after.draft_build_id, before.draft_build_id);
    assert_eq!(after.published_build_id, before.published_build_id);

    // And nothing was stored: no build row, no bytes under the build prefix.
    assert!(build_row(&db, first.app_id, "build-2").await.is_none());
    let state_dir = std::env::var("OXY_STATE_DIR").expect("test_db sets OXY_STATE_DIR");
    let prefix_of = |build_id: &str| {
        std::path::Path::new(&state_dir).join(
            oxy_app::server::api::custom_apps_build_store::build_prefix(first.app_id, build_id),
        )
    };
    // The first build's bytes are where this looks, so the absence below is
    // not an artefact of looking in the wrong place.
    assert!(prefix_of("build-1").join("index.html").exists());
    let prefix = prefix_of("build-2");
    assert!(
        !prefix.exists(),
        "refused publish left bytes at {}",
        prefix.display()
    );
}

#[tokio::test]
async fn a_publish_to_the_same_workspace_still_publishes() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;
    let slug = "workspace-same";

    let first = publish(input(&t, slug, t.workspace_a, "build-1"))
        .await
        .expect("first publish");

    // Branch and name stay overwritable: they label the app, they don't move
    // its data.
    let mut again = input(&t, slug, t.workspace_a, "build-2");
    again.branch = Some("feature".to_string());
    again.name = Some("Renamed".to_string());
    let second = publish(again).await.expect("same-workspace publish");
    assert_eq!(second.app_id, first.app_id);
    assert!(!second.is_new_app);

    let row = reload(&db, first.app_id).await;
    assert_eq!(row.project_id, t.workspace_a);
    assert_eq!(row.branch, "feature");
    assert_eq!(row.name, "Renamed");
    let build = build_row(&db, first.app_id, "build-2")
        .await
        .expect("second build recorded");
    assert_eq!(row.draft_build_id, Some(build.id));
}

/// `apps.project_id` has no foreign key and deleting a workspace leaves `apps`
/// alone, so an app can outlive its workspace. Refusing its next publish would
/// be a dead end: the `--project` the 409 suggests no longer exists, and the
/// PATCH that moves an app is on the Oxy app-admin surface, out of a tenant's
/// reach.
#[tokio::test]
async fn an_app_whose_workspace_was_deleted_rehomes_on_its_next_publish() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;
    let slug = "workspace-rehome";

    let first = publish(input(&t, slug, t.workspace_a, "build-1"))
        .await
        .expect("first publish creates the app in workspace A");

    // While workspace A exists, naming B is still the 409: the re-home below is
    // earned by the deletion, not by the mismatch alone.
    let err = publish(input(&t, slug, t.workspace_b, "build-2"))
        .await
        .expect_err("a mismatch with both workspaces alive must be refused");
    assert!(
        matches!(err, PublishError::ProjectMismatch { .. }),
        "expected ProjectMismatch, got {err:?}"
    );
    assert_eq!(reload(&db, first.app_id).await.project_id, t.workspace_a);

    workspaces::Entity::delete_by_id(t.workspace_a)
        .exec(&db)
        .await
        .expect("delete workspace A");
    assert_eq!(
        reload(&db, first.app_id).await.project_id,
        t.workspace_a,
        "deleting the workspace leaves the app row naming it"
    );

    // Re-homing never crosses orgs: a workspace of another org is still refused
    // by the org guard, and the row keeps naming the deleted workspace.
    let other = seed_tenant(&db).await;
    let err = publish(input(&t, slug, other.workspace_a, "build-2"))
        .await
        .expect_err("a workspace of another org must be refused");
    assert!(
        matches!(err, PublishError::UnknownProject(..)),
        "expected UnknownProject, got {err:?}"
    );
    assert_eq!(reload(&db, first.app_id).await.project_id, t.workspace_a);

    let second = publish(input(&t, slug, t.workspace_b, "build-2"))
        .await
        .expect("an app whose workspace was deleted publishes into another of its org's");
    assert_eq!(second.app_id, first.app_id);
    assert!(!second.is_new_app);

    let row = reload(&db, first.app_id).await;
    assert_eq!(
        row.project_id, t.workspace_b,
        "the publish re-homed the app"
    );
    let build = build_row(&db, first.app_id, "build-2")
        .await
        .expect("re-homing build recorded");
    assert_eq!(row.draft_build_id, Some(build.id));
}

/// Write `project_id` straight onto the app row, bypassing every handler. The
/// admin `PATCH /api/customer-apps/{id}` did exactly this, with no org check,
/// until #3174, so rows naming such a workspace may already exist.
async fn repoint_app(db: &DatabaseConnection, app_id: Uuid, project_id: Uuid) {
    let mut row: apps::ActiveModel = reload(db, app_id).await.into();
    row.project_id = ActiveValue::Set(project_id);
    row.update(db).await.expect("repoint app row");
}

/// The app's row names a live workspace, but one of ANOTHER org. Refusing would
/// tell the publisher to pass `--project` for that workspace, which resolves to
/// the other org, misses this app there, and registers a second app instead of
/// repairing this one.
#[tokio::test]
async fn an_app_whose_workspace_is_in_another_org_rehomes_on_its_next_publish() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;
    let other = seed_tenant(&db).await;
    let slug = "workspace-other-org";

    let first = publish(input(&t, slug, t.workspace_a, "build-1"))
        .await
        .expect("first publish creates the app in workspace A");
    repoint_app(&db, first.app_id, other.workspace_a).await;

    let second = publish(input(&t, slug, t.workspace_a, "build-2"))
        .await
        .expect("an app on another org's workspace publishes back into its own org's");
    assert_eq!(second.app_id, first.app_id);
    assert!(!second.is_new_app);

    let row = reload(&db, first.app_id).await;
    assert_eq!(
        row.project_id, t.workspace_a,
        "the publish re-homed the app"
    );
    let build = build_row(&db, first.app_id, "build-2")
        .await
        .expect("re-homing build recorded");
    assert_eq!(row.draft_build_id, Some(build.id));
}

/// The app's row names a workspace that exists but belongs to no org
/// (`workspaces.org_id` is nullable). Same dead end as a deleted workspace: it
/// can never be a `--project` that resolves to the app's org.
#[tokio::test]
async fn an_app_whose_workspace_is_orphaned_rehomes_on_its_next_publish() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;
    let slug = "workspace-orphaned";

    let first = publish(input(&t, slug, t.workspace_a, "build-1"))
        .await
        .expect("first publish creates the app in workspace A");
    let orphan = Uuid::new_v4();
    workspaces::ActiveModel {
        id: ActiveValue::Set(orphan),
        name: ActiveValue::Set("Orphaned Workspace".into()),
        org_id: ActiveValue::Set(None),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("seed orphaned workspace");
    repoint_app(&db, first.app_id, orphan).await;

    let second = publish(input(&t, slug, t.workspace_a, "build-2"))
        .await
        .expect("an app on an orphaned workspace publishes back into its own org's");
    assert_eq!(second.app_id, first.app_id);
    assert!(!second.is_new_app);

    let row = reload(&db, first.app_id).await;
    assert_eq!(
        row.project_id, t.workspace_a,
        "the publish re-homed the app"
    );
    let build = build_row(&db, first.app_id, "build-2")
        .await
        .expect("re-homing build recorded");
    assert_eq!(row.draft_build_id, Some(build.id));
}
