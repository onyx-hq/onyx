//! An app secret you can write must be an app secret `ctx.env` can read.
//!
//! ## Why this test exists
//!
//! `ctx.env` resolves per-app secrets by listing the project's secrets and
//! keeping the ones prefixed `apps/<app_id>/`, stripping the prefix. The write
//! side builds that prefix independently, inside `set_app_secret`. Two places
//! spelling the same convention, in different crates, with nothing between them
//! — and for a long time only one of them existed at all, because the public
//! secrets API rejects the `/` such a name needs.
//!
//! A write path whose rows never appear on the read path looks exactly like a
//! working write path: the POST returns 204, the row is really in the table, and
//! the function still sees nothing. So the round trip is asserted against the
//! real reader's filter rather than against the writer's own idea of the prefix.
//!
//! Isolation is the other half. The prefix is the only thing keeping one app out
//! of another's secrets, so a near-miss (a different app id, a project-wide name)
//! must not be picked up.

use crate::common::test_db;
use entity::users;
use oxy::service::secret_manager::SecretManagerService;
use sea_orm::{ActiveModelTrait, ActiveValue, DatabaseConnection};
use uuid::Uuid;

/// The reader's rule, restated here on purpose.
///
/// This mirrors `custom_apps_secrets`/`resolve_function_env` rather than calling
/// it: both are private and one is feature-gated, and a test that called the
/// implementation would pass just as happily if BOTH sides changed prefix
/// together — which is the drift worth catching.
fn env_visible_to(app_id: Uuid, names: &[String]) -> Vec<String> {
    let prefix = format!("apps/{app_id}/");
    names
        .iter()
        .filter_map(|n| n.strip_prefix(&prefix))
        .map(str::to_string)
        .collect()
}

async fn seed_user(db: &DatabaseConnection) -> Uuid {
    let id = Uuid::new_v4();
    users::ActiveModel {
        id: ActiveValue::Set(id),
        email: ActiveValue::Set(Some(format!("secrets-{id}@example.com"))),
        name: ActiveValue::Set("App Secrets Test".into()),
        picture: ActiveValue::Set(None),
        email_verified: ActiveValue::Set(true),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed user");
    id
}

/// 32 zero bytes, base64. `get_encryption_key` otherwise generates one and
/// writes it into the state dir, which makes the test depend on filesystem
/// state it did not set up.
fn use_a_fixed_encryption_key() {
    // SAFETY: nextest runs each test in its own process, and this happens before
    // any secret is encrypted.
    unsafe {
        std::env::set_var(
            "OXY_ENCRYPTION_KEY",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
    }
}

async fn stored_names(manager: &SecretManagerService, db: &DatabaseConnection) -> Vec<String> {
    manager
        .list_secrets(db)
        .await
        .expect("list secrets")
        .into_iter()
        .map(|s| s.name)
        .collect()
}

#[tokio::test]
async fn a_written_app_secret_is_visible_to_ctx_env() {
    use_a_fixed_encryption_key();
    let db = test_db().await;
    let actor = seed_user(&db).await;
    let (project, app) = (Uuid::new_v4(), Uuid::new_v4());
    let manager = SecretManagerService::new(project);

    manager
        .set_app_secret(&db, app, "STRIPE_API_KEY", "sk_test_123", actor)
        .await
        .expect("set app secret");

    let names = stored_names(&manager, &db).await;
    assert_eq!(
        env_visible_to(app, &names),
        vec!["STRIPE_API_KEY".to_string()],
        "the reader must see the key with the prefix stripped — the write path \
         builds `apps/<app_id>/` itself, so a change on either side breaks this"
    );

    let value = manager
        .get_secret(&format!("apps/{app}/STRIPE_API_KEY"))
        .await;
    assert_eq!(value.as_deref(), Some("sk_test_123"), "value round-trips");
}

#[tokio::test]
async fn one_apps_secrets_are_invisible_to_another() {
    use_a_fixed_encryption_key();
    let db = test_db().await;
    let actor = seed_user(&db).await;
    let project = Uuid::new_v4();
    let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());
    let manager = SecretManagerService::new(project);

    manager
        .set_app_secret(&db, mine, "TOKEN", "mine", actor)
        .await
        .expect("set mine");
    manager
        .set_app_secret(&db, theirs, "TOKEN", "theirs", actor)
        .await
        .expect("set theirs");
    // A project-wide secret shares the store and must never leak into an app.
    manager
        .create_secret(
            &db,
            oxy::service::secret_manager::CreateSecretParams {
                name: "TOKEN".to_string(),
                value: "project-wide".to_string(),
                description: None,
                created_by: actor,
            },
        )
        .await
        .expect("set project-wide");

    let names = stored_names(&manager, &db).await;
    assert_eq!(env_visible_to(mine, &names), vec!["TOKEN".to_string()]);
    assert_eq!(env_visible_to(theirs, &names), vec!["TOKEN".to_string()]);
    assert_eq!(
        manager
            .get_secret(&format!("apps/{mine}/TOKEN"))
            .await
            .as_deref(),
        Some("mine"),
        "same key name in three scopes must resolve to three different values"
    );
    assert_eq!(
        manager
            .get_secret(&format!("apps/{theirs}/TOKEN"))
            .await
            .as_deref(),
        Some("theirs")
    );
    assert_eq!(
        manager.get_secret("TOKEN").await.as_deref(),
        Some("project-wide")
    );
}

/// `set_app_secret` is an upsert. Rotating must replace the value rather than
/// add a second active row — the table has no unique constraint on active names,
/// so a second row would make reads nondeterministic instead of failing loudly.
#[tokio::test]
async fn rotating_replaces_rather_than_duplicates() {
    use_a_fixed_encryption_key();
    let db = test_db().await;
    let actor = seed_user(&db).await;
    let (project, app) = (Uuid::new_v4(), Uuid::new_v4());
    let manager = SecretManagerService::new(project);

    manager
        .set_app_secret(&db, app, "TOKEN", "first", actor)
        .await
        .expect("first write");
    manager
        .set_app_secret(&db, app, "TOKEN", "second", actor)
        .await
        .expect("rotation");

    let names = stored_names(&manager, &db).await;
    assert_eq!(
        env_visible_to(app, &names).len(),
        1,
        "a rotation must leave exactly one active row"
    );
    assert_eq!(
        manager
            .get_secret(&format!("apps/{app}/TOKEN"))
            .await
            .as_deref(),
        Some("second"),
        "and the cache must not keep serving the old value"
    );
}

/// The key half is validated; the system-built prefix is not. A key carrying a
/// `/` would otherwise write outside the app's namespace.
#[tokio::test]
async fn a_key_cannot_escape_its_namespace() {
    use_a_fixed_encryption_key();
    let db = test_db().await;
    let actor = seed_user(&db).await;
    let (project, app) = (Uuid::new_v4(), Uuid::new_v4());
    let manager = SecretManagerService::new(project);

    let escaped = manager
        .set_app_secret(&db, app, "../other/TOKEN", "nope", actor)
        .await;
    assert!(escaped.is_err(), "a slash in the key must be rejected");

    assert!(
        stored_names(&manager, &db).await.is_empty(),
        "and nothing may be written on the way to that rejection"
    );
}
