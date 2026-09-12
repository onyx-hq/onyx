//! A publish whose manifest declares functions must carry their bundled JS.
//!
//! The server used to accept a bundle whose `oxy-app.json` declared functions
//! but held no `functions/<name>.js`: it recorded a function row per name,
//! pointing at an artifact that was never uploaded, and the app went live with
//! every one of those functions missing. An `oxy` CLI older than 0.5.96 — which
//! never bundled functions — produces exactly that bundle.
//!
//! These drive the real `publish()` against a per-test database and the
//! filesystem build store, because the refusal only matters if it lands before
//! anything is written.

use crate::common::test_db;
use entity::{
    app_builds, app_functions, apps, org_members, org_members::OrgRole, organizations, users,
    workspaces,
};
use flate2::{Compression, write::GzEncoder};
use oxy_app::server::api::custom_apps_build_store::build_prefix;
use oxy_app::server::api::custom_apps_publish::{OrgRef, PublishError, PublishInput, publish};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use uuid::Uuid;

const INDEX_HTML: &[u8] = b"<!doctype html><html><head><title>t</title></head><body></body></html>";
const MANIFEST: &[u8] =
    br#"{ "schemaVersion": 2, "slug": "fn-app", "functions": { "notify": {}, "top-stores": { "route": true } } }"#;
const FUNCTION_JS: &[u8] = b"export default async () => new Response('ok');";

/// An org, an Admin who may publish into it, and one of its workspaces.
struct Tenant {
    org_id: Uuid,
    user_id: Uuid,
    email: String,
    workspace: Uuid,
}

async fn seed_tenant(db: &DatabaseConnection) -> Tenant {
    let org_id = Uuid::new_v4();
    organizations::ActiveModel {
        id: ActiveValue::Set(org_id),
        name: ActiveValue::Set("Function Artifacts Org".into()),
        slug: ActiveValue::Set(format!("fn-art-{}", &org_id.simple().to_string()[..12])),
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

    let workspace = Uuid::new_v4();
    workspaces::ActiveModel {
        id: ActiveValue::Set(workspace),
        name: ActiveValue::Set("Function Artifacts Workspace".into()),
        org_id: ActiveValue::Set(Some(org_id)),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed workspace");

    Tenant {
        org_id,
        user_id,
        email,
        workspace,
    }
}

fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, *bytes)
            .expect("append file");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

fn input(t: &Tenant, build_id: &str, tarball: Vec<u8>) -> PublishInput {
    PublishInput {
        org_ref: Some(OrgRef::Id(t.org_id)),
        app_slug: "fn-app".to_string(),
        project_id: t.workspace,
        branch: None,
        build_id: build_id.to_string(),
        name: None,
        promote: false,
        tarball,
        manifest: None,
        source_repo: None,
        commit_sha: None,
        published_by: Some(t.user_id),
        published_by_email: Some(t.email.clone()),
        machine_app_id: None,
    }
}

fn state_dir() -> std::path::PathBuf {
    std::env::var("OXY_STATE_DIR")
        .expect("test_db sets OXY_STATE_DIR")
        .into()
}

fn expect_missing(err: PublishError, expected: &[&str]) {
    match err {
        PublishError::MissingFunctionArtifacts { missing } => assert_eq!(missing, expected),
        other => panic!("expected MissingFunctionArtifacts, got {other:?}"),
    }
}

#[tokio::test]
async fn a_first_publish_without_function_artifacts_is_refused_and_creates_nothing() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;

    let bundle = tar_gz(&[("index.html", INDEX_HTML), ("oxy-app.json", MANIFEST)]);
    let err = publish(input(&t, "build-1", bundle))
        .await
        .expect_err("a bundle declaring functions it does not carry must be refused");
    expect_missing(err, &["notify", "top-stores"]);

    // No app row, and no bytes anywhere in the build store.
    let app = apps::Entity::find()
        .filter(apps::Column::OrgId.eq(t.org_id))
        .filter(apps::Column::Slug.eq("fn-app"))
        .one(&db)
        .await
        .expect("query apps");
    assert!(app.is_none(), "refused first publish created an app row");
    assert!(
        !state_dir().join("customer-apps").exists(),
        "refused first publish stored bytes"
    );
}

#[tokio::test]
async fn a_republish_missing_one_artifact_is_refused_and_leaves_the_live_app_alone() {
    let db = test_db().await;
    let t = seed_tenant(&db).await;

    // A complete bundle publishes, and records a function row per declaration.
    let complete = tar_gz(&[
        ("index.html", INDEX_HTML),
        ("oxy-app.json", MANIFEST),
        ("functions/notify.js", FUNCTION_JS),
        ("functions/top-stores.js", FUNCTION_JS),
    ]);
    let first = publish(input(&t, "build-1", complete))
        .await
        .expect("a bundle carrying every declared function publishes");
    let before = apps::Entity::find_by_id(first.app_id)
        .one(&db)
        .await
        .expect("reload app")
        .expect("app exists");
    let build_1 = before.draft_build_id.expect("draft points at build-1");
    let fns = app_functions::Entity::find()
        .filter(app_functions::Column::BuildId.eq(build_1))
        .all(&db)
        .await
        .expect("query app_functions");
    assert_eq!(fns.len(), 2);

    // The next bundle still declares both but only carries one.
    let partial = tar_gz(&[
        ("index.html", INDEX_HTML),
        ("oxy-app.json", MANIFEST),
        ("functions/notify.js", FUNCTION_JS),
    ]);
    let err = publish(input(&t, "build-2", partial))
        .await
        .expect_err("a bundle missing a declared function's artifact must be refused");
    expect_missing(err, &["top-stores"]);

    let after = apps::Entity::find_by_id(first.app_id)
        .one(&db)
        .await
        .expect("reload app")
        .expect("app exists");
    assert_eq!(after.draft_build_id, before.draft_build_id);
    assert_eq!(after.updated_at, before.updated_at);
    let build_2 = app_builds::Entity::find()
        .filter(app_builds::Column::AppId.eq(first.app_id))
        .filter(app_builds::Column::BuildId.eq("build-2"))
        .one(&db)
        .await
        .expect("query app_builds");
    assert!(build_2.is_none(), "refused publish recorded a build row");
    // build-1's bytes are where this looks, so build-2's absence is real.
    let prefix_of = |build_id: &str| state_dir().join(build_prefix(first.app_id, build_id));
    assert!(
        prefix_of("build-1")
            .join("functions/top-stores.js")
            .exists()
    );
    assert!(
        !prefix_of("build-2").exists(),
        "refused publish stored bytes"
    );
}
