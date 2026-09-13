//! The seeded example app actually renders — the launcher lists it, and its
//! bundle serves.
//!
//! `seed_example_app.rs` proves the seed writes the right rows and bytes. That's
//! not the same as the app working: the rows are only correct in terms of what
//! the *reading* code does with them. These tests drive the real handlers —
//! `list_custom_apps` (the home grid) and `serve_dispatch` (the bundle) —
//! against real seeded data, so a change to either reader that orphans the seed
//! fails here rather than on a developer's first `oxy seed`.
//!
//! Auth: `BuiltInAuthenticator` falls back to a guest identity when no auth
//! method is configured, which is the default in a test process. The guest is a
//! plain user with no special standing, so the org-membership gate is exercised
//! for real — the harness strips `OXY_OWNER`/`OXY_GLOBAL_ADMINS` precisely so
//! the staff path can't short-circuit it.

use crate::common::{APP_SLUG, demo_workspace_id, examples_path, test_db};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use chrono::Utc;

/// The seed's second, restricted deployment into Acme — see `seed_apps`.
const RESTRICTED_APP_SLUG: &str = "oxy-starter-private";
use entity::prelude::{Apps, OrgMembers, Organizations};
use entity::{apps, org_members, org_members::OrgRole, organizations};
use oxy_app::cli::commands::seed;
use oxy_app::server::api::{custom_apps_serve, workspace_custom_apps};
use oxy_auth::types::Identity;
use oxy_auth::user::UserService;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    QueryFilter,
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

/// Just the two readers under test. Mounting these rather than the whole
/// `api_router` keeps the test to the handlers in question — the full router
/// drags in background workers and agentic state that have nothing to do with
/// whether an app renders.
fn router() -> Router {
    Router::new()
        .route(
            "/{workspace_id}/custom-apps",
            get(workspace_custom_apps::list_custom_apps),
        )
        .route(
            "/customer-apps/{*path}",
            get(custom_apps_serve::serve_dispatch),
        )
}

async fn get_json(uri: &str) -> (StatusCode, Value) {
    let resp = router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get_text(uri: &str) -> (StatusCode, String) {
    let resp = router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Make the guest a member of `org_id`. The serve path authenticates as the
/// guest, and the customer path in `user_can_access_app` requires membership —
/// the seed doesn't grant it, so a test that wants a *successful* render has to.
async fn add_guest_to_org(db: &DatabaseConnection, org_id: Uuid) -> Uuid {
    let guest = UserService::get_or_create_user(&Identity {
        // A test fixture that must MINT its guest, so no id.
        user_id: None,
        email: oxy_auth::user::LOCAL_GUEST_EMAIL.to_string(),
        name: Some("Local User".to_string()),
        picture: None,
    })
    .await
    .expect("guest user");
    let now = Utc::now().fixed_offset();
    org_members::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        org_id: ActiveValue::Set(org_id),
        user_id: ActiveValue::Set(guest.id),
        role: ActiveValue::Set(OrgRole::Member),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    }
    .insert(db)
    .await
    .expect("insert guest membership");
    guest.id
}

async fn org_id(db: &DatabaseConnection, slug: &str) -> Uuid {
    Organizations::find()
        .filter(organizations::Column::Slug.eq(slug))
        .one(db)
        .await
        .expect("query org")
        .unwrap_or_else(|| panic!("no {slug} org"))
        .id
}

// ── The home grid ────────────────────────────────────────────────────────────

#[tokio::test]
async fn home_grid_lists_the_seeded_app_with_its_card_metadata() {
    // Also the call that points this process at a fresh migrated DB — the
    // handler reaches it through `establish_connection()`.
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");
    let build = seeded_published_build(&db).await;

    let (status, body) = get_json(&format!("/{}/custom-apps", demo_workspace_id())).await;
    assert_eq!(status, StatusCode::OK);

    let apps = body.as_array().expect("array of apps");
    let card = apps
        .iter()
        .find(|a| a["slug"] == APP_SLUG)
        .unwrap_or_else(|| panic!("{APP_SLUG} not on the home grid; got {body}"));

    assert_eq!(card["name"], "Oxy Starter");
    assert_eq!(card["org_slug"], "local");
    assert_eq!(card["url"], format!("/customer-apps/local/{APP_SLUG}/"));
    // Everything below comes from the bundle's oxy-app.json via
    // app_builds.manifest_json. If the seed stopped storing the manifest, the
    // card would still list — just blank — so assert the fields, not the row.
    assert!(
        card["description"].is_string(),
        "card has no description: {card}"
    );
    // Pinned to the published build, so the serve route can call them
    // `immutable` — see `a_launcher_image_pinned_to_the_served_build_is_immutable`.
    assert_eq!(
        card["icon_url"],
        format!("/customer-apps/local/{APP_SLUG}/icon.svg?v={build}")
    );
    assert_eq!(
        card["art_url"],
        format!("/customer-apps/local/{APP_SLUG}/card.svg?v={build}")
    );
    assert!(
        card["suggested_questions"]
            .as_array()
            .is_some_and(|q| !q.is_empty()),
        "the manifest's ask block should reach the card: {card}"
    );
}

#[tokio::test]
async fn home_grid_is_scoped_to_its_own_workspace() {
    let _db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");

    // Acme's app exists, but it belongs to Acme's workspace. Asking the demo
    // workspace's grid must not return it — `list_custom_apps` filters on
    // project_id, and cross-tenant leakage here would put another org's app on
    // your home page.
    let (status, body) = get_json(&format!("/{}/custom-apps", demo_workspace_id())).await;
    assert_eq!(status, StatusCode::OK);
    let orgs: Vec<&str> = body
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|a| a["org_slug"].as_str())
        .collect();
    assert!(
        orgs.iter().all(|o| *o == "local"),
        "the demo workspace's grid leaked another org's app: {orgs:?}"
    );

    // And an unrelated workspace shows nothing.
    let (status, body) = get_json(&format!("/{}/custom-apps", Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().expect("array").len(), 0);
}

#[tokio::test]
async fn unpublishing_removes_the_app_from_the_home_grid() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");

    let local = org_id(&db, "local").await;
    let app = Apps::find()
        .filter(apps::Column::OrgId.eq(local))
        .filter(apps::Column::Slug.eq(APP_SLUG))
        .one(&db)
        .await
        .expect("query app")
        .expect("seeded app");

    let mut active = app.into_active_model();
    active.published_at = ActiveValue::Set(None);
    active.update(&db).await.expect("unpublish");

    // `published_at` is the ONLY thing separating "on the grid" from "reachable
    // by direct URL", which is why the seed always sets it. Pin the rule.
    let (status, body) = get_json(&format!("/{}/custom-apps", demo_workspace_id())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body
            .as_array()
            .expect("array")
            .iter()
            .any(|a| a["slug"] == APP_SLUG),
        "an unpublished app must not appear on the home grid: {body}"
    );
}

// ── The bundle ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn opening_the_app_serves_the_bundle_with_its_runtime_identity() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");
    add_guest_to_org(&db, org_id(&db, "local").await).await;

    let (status, html) = get_text(&format!("/customer-apps/local/{APP_SLUG}/")).await;
    assert_eq!(status, StatusCode::OK, "body: {html}");

    // The app is dead without this: it reads projectId from __OXY_APP__ to
    // address the data plane, and injection only happens if the bundle has a
    // </head> — which it silently skips with a warn! when absent.
    assert!(
        html.contains("window.__OXY_APP__"),
        "runtime identity was not injected into the bundle"
    );
    assert!(
        html.contains(&demo_workspace_id().to_string()),
        "the injected projectId should be the demo workspace"
    );
    assert!(html.contains("Oxy Starter"), "served the wrong document");
}

#[tokio::test]
async fn app_assets_serve_from_the_build_store() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");
    add_guest_to_org(&db, org_id(&db, "local").await).await;

    // The launcher card points at this exact URL, so a card with a broken image
    // is exactly this request failing.
    let (status, svg) = get_text(&format!("/customer-apps/local/{APP_SLUG}/icon.svg")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(svg.contains("<svg"), "icon.svg did not serve: {svg}");
}

/// The launcher's `?v=` pin, end to end through the real router: the query has
/// to survive down to the cache decision, the URL the home grid hands out has
/// to come back `immutable`, and a pin naming any other build must not be
/// honoured — that is the stale-pod race, where the old bytes would otherwise
/// be pinned under the new build's URL for a year.
#[tokio::test]
async fn a_launcher_image_pinned_to_the_served_build_is_immutable() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");
    add_guest_to_org(&db, org_id(&db, "local").await).await;
    let build = seeded_published_build(&db).await;
    let icon = format!("/customer-apps/local/{APP_SLUG}/icon.svg");

    let (status, cache) = get_cache_control(&format!("{icon}?v={build}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache, "public, max-age=31536000, immutable");

    let (status, cache) = get_cache_control(&format!("{icon}?v={}", Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        cache, "public, max-age=300",
        "a pin for another build was honoured"
    );

    let (status, cache) = get_cache_control(&icon).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        cache, "public, max-age=300",
        "a bare root file must revalidate"
    );
}

async fn get_cache_control(uri: &str) -> (StatusCode, String) {
    let resp = router()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    let cache = resp
        .headers()
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (resp.status(), cache)
}

/// The seeded example app's published build pk — what the launcher pins its
/// image URLs to.
async fn seeded_published_build(db: &DatabaseConnection) -> Uuid {
    let local = org_id(db, "local").await;
    Apps::find()
        .filter(apps::Column::OrgId.eq(local))
        .filter(apps::Column::Slug.eq(APP_SLUG))
        .one(db)
        .await
        .expect("query app")
        .expect("seeded app")
        .published_build_id
        .expect("the seed publishes the example app")
}

#[tokio::test]
async fn a_non_member_cannot_open_another_orgs_app() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");

    // The guest joins `local` — and only `local`. Acme's app is published, so
    // the only thing standing between this request and another tenant's app is
    // the org-membership check.
    add_guest_to_org(&db, org_id(&db, "local").await).await;

    let (status, _) = get_text(&format!("/customer-apps/acme/{APP_SLUG}/")).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a non-member reached another org's custom app"
    );
}

/// An org's home page lists the app the seed deployed into it.
///
/// The chain has four links and every one of them has silently broken at least
/// once: the app must be **published**, its `project_id` must be the workspace the
/// org's subdomain names as default, that subdomain row must be **enabled**, and
/// the visibility filter must let the viewer through. Asserting the rows separately
/// would pass while the chain is broken, so this drives the real reader
/// (`published_app_summaries`) against the real seed.
#[tokio::test]
async fn a_seeded_org_home_lists_its_published_app() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");

    // Acme only, deliberately. The `local` org's membership depends on
    // `OXY_GLOBAL_ADMINS`, which this harness strips (see the module header), so it
    // has no owner here — while Acme's seven people are seeded unconditionally.
    // `local`'s grid is already covered by `home_grid_is_scoped_to_its_own_workspace`.
    {
        let org_slug = "acme";
        let org = Organizations::find()
            .filter(organizations::Column::Slug.eq(org_slug))
            .one(&db)
            .await
            .expect("query org")
            .unwrap_or_else(|| panic!("{org_slug} seeded"));

        // The org's home resolves through its subdomain's default workspace. Without
        // an ENABLED row the host 302s to the app root, so the org would have no home
        // for the app to appear on at all.
        let sub = entity::prelude::OrgSubdomains::find()
            .filter(entity::org_subdomains::Column::OrgId.eq(org.id))
            .one(&db)
            .await
            .expect("query subdomain")
            .unwrap_or_else(|| panic!("{org_slug} should have a seeded subdomain row"));
        assert!(sub.enabled, "{org_slug}'s subdomain must be enabled");
        let home_ws = sub
            .default_workspace_id
            .unwrap_or_else(|| panic!("{org_slug}'s subdomain needs a default workspace"));

        // An owner of the org — the person whose home page this is.
        let owner = OrgMembers::find()
            .filter(org_members::Column::OrgId.eq(org.id))
            .filter(org_members::Column::Role.eq(OrgRole::Owner))
            .one(&db)
            .await
            .expect("query owner")
            .unwrap_or_else(|| panic!("{org_slug} should have an owner"));
        let user = entity::prelude::Users::find_by_id(owner.user_id)
            .one(&db)
            .await
            .expect("query user")
            .expect("owner user row");

        let viewer = workspace_custom_apps::Viewer {
            id: user.id,
            email: user.email.as_deref().unwrap_or(""),
        };
        let names: Vec<String> =
            workspace_custom_apps::published_app_summaries(&db, home_ws, Some(viewer))
                .await
                .expect("summaries")
                .into_iter()
                .map(|s| s.slug)
                .collect();

        assert!(
            names.iter().any(|s| s == APP_SLUG),
            "{org_slug}'s home page must list the seeded app, got {names:?}"
        );
    }
}

/// The restricted seeded app is invisible to an Acme person who holds no grant —
/// on the very same home page that shows them the open one.
///
/// This is the seed's whole point: without it, "restricted" is a column value no
/// screen ever demonstrates.
#[tokio::test]
async fn the_restricted_seeded_app_is_hidden_from_an_ungranted_member() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");

    let org = Organizations::find()
        .filter(organizations::Column::Slug.eq("acme"))
        .one(&db)
        .await
        .expect("query org")
        .expect("acme seeded");
    let sub = entity::prelude::OrgSubdomains::find()
        .filter(entity::org_subdomains::Column::OrgId.eq(org.id))
        .one(&db)
        .await
        .expect("query subdomain")
        .expect("acme subdomain");
    let home_ws = sub.default_workspace_id.expect("default workspace");

    // A plain member who is NOT in "Client Delivery". The seed puts persons 2, 3 and
    // 6 in no granted team, so at least one such member exists by construction.
    let members = OrgMembers::find()
        .filter(org_members::Column::OrgId.eq(org.id))
        .filter(org_members::Column::Role.eq(OrgRole::Member))
        .all(&db)
        .await
        .expect("query members");

    // The POSITIVE half first, and it is what stops this test passing vacuously.
    // Counting members who DON'T see the app would be green if the restricted
    // deployment were missing entirely — every member fails to see something that
    // doesn't exist — so "correctly hidden" and "never deployed" would look
    // identical. Proving a granted member DOES see it pins the app's existence, its
    // grant, and the team's roster in one assertion.
    // Scoped to THIS app, not just "some grant on a team in Acme". Exactly one grant
    // exists today, so the looser query happens to resolve — but a second restricted
    // seed app would let it pick that app's team, and the assertion below would then
    // fail while pointing at the wrong thing.
    let restricted_app = Apps::find()
        .filter(apps::Column::OrgId.eq(org.id))
        .filter(apps::Column::Slug.eq(RESTRICTED_APP_SLUG))
        .one(&db)
        .await
        .expect("query restricted app")
        .expect("the seed must deploy Acme's restricted app");
    let granted_team = entity::prelude::AppTeamGrants::find()
        .filter(entity::app_team_grants::Column::AppId.eq(restricted_app.id))
        .find_also_related(entity::prelude::OrgTeams)
        .one(&db)
        .await
        .expect("query team grants")
        .and_then(|(_, team)| team)
        .expect("the seed must grant Acme's restricted app to one of its teams");

    let granted_member = entity::prelude::OrgTeamMembers::find()
        .filter(entity::org_team_members::Column::TeamId.eq(granted_team.id))
        .one(&db)
        .await
        .expect("query team members")
        .unwrap_or_else(|| panic!("{} must have at least one member", granted_team.name));
    let granted_user = entity::prelude::Users::find_by_id(granted_member.user_id)
        .one(&db)
        .await
        .expect("query user")
        .expect("granted user row");

    let granted_slugs: Vec<String> = workspace_custom_apps::published_app_summaries(
        &db,
        home_ws,
        Some(workspace_custom_apps::Viewer {
            id: granted_user.id,
            email: granted_user.email.as_deref().unwrap_or(""),
        }),
    )
    .await
    .expect("summaries")
    .into_iter()
    .map(|s| s.slug)
    .collect();
    assert!(
        granted_slugs.iter().any(|s| s == RESTRICTED_APP_SLUG),
        "a member of {} must see the restricted app — without this the \"hidden\" \
         assertions below would pass even if the app were never deployed: {granted_slugs:?}",
        granted_team.name
    );

    // And the negative half: at least one Acme member is left out entirely.
    let mut checked = 0;
    for m in members {
        if m.user_id == granted_member.user_id {
            continue;
        }
        let user = entity::prelude::Users::find_by_id(m.user_id)
            .one(&db)
            .await
            .expect("query user")
            .expect("member user row");
        let viewer = workspace_custom_apps::Viewer {
            id: user.id,
            email: user.email.as_deref().unwrap_or(""),
        };
        let slugs: Vec<String> =
            workspace_custom_apps::published_app_summaries(&db, home_ws, Some(viewer))
                .await
                .expect("summaries")
                .into_iter()
                .map(|s| s.slug)
                .collect();

        // Every member sees the open app, whatever their team.
        assert!(
            slugs.iter().any(|s| s == APP_SLUG),
            "the open app must stay visible to every Acme member, got {slugs:?}"
        );
        if !slugs.iter().any(|s| s == RESTRICTED_APP_SLUG) {
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the seed must leave at least one Acme member without the restricted app — \
         otherwise the filtered-out state is never demonstrated"
    );
}

/// An extra workspace that sorts before the seeded one must not steal the app.
///
/// The app seed used to resolve its target with `ORDER BY name ASC LIMIT 1`. That
/// held only while the org had exactly the workspaces the seed made — add one named
/// `"AAA"` and the next re-seed moves the apps onto it, off the workspace the org
/// subdomain names as default. Nothing errors; the org's home page just renders an
/// empty grid. This reproduces that setup and asserts the apps stay put.
#[tokio::test]
async fn an_alphabetically_earlier_workspace_does_not_steal_the_seeded_app() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");

    let org = Organizations::find()
        .filter(organizations::Column::Slug.eq("acme"))
        .one(&db)
        .await
        .expect("query org")
        .expect("acme seeded");

    // Where the seed put the apps the first time.
    let seeded_ws = Apps::find()
        .filter(apps::Column::OrgId.eq(org.id))
        .filter(apps::Column::Slug.eq(APP_SLUG))
        .one(&db)
        .await
        .expect("query app")
        .expect("acme's app")
        .project_id;

    // A workspace that sorts ahead of "Acme Internal Analytics" under any collation.
    let intruder = Uuid::new_v4();
    entity::workspaces::ActiveModel {
        id: ActiveValue::Set(intruder),
        name: ActiveValue::Set("AAA scratch".into()),
        org_id: ActiveValue::Set(Some(org.id)),
        ..Default::default()
    }
    .insert(&db)
    .await
    .expect("insert intruder workspace");

    // Re-seed. The target must still be the workspace the seed created.
    seed::seed_demo(Some(examples_path()))
        .await
        .expect("re-seed");

    let after = Apps::find()
        .filter(apps::Column::OrgId.eq(org.id))
        .filter(apps::Column::Slug.eq(APP_SLUG))
        .one(&db)
        .await
        .expect("query app")
        .expect("acme's app")
        .project_id;
    assert_eq!(
        after, seeded_ws,
        "a re-seed moved the app onto a workspace that merely sorts earlier"
    );
    assert_ne!(after, intruder);

    // The org root has somewhere to point at all. That existence — an enabled row —
    // is what this block earns; the equality below cannot independently fail, since
    // `default_workspace_id` is written once on the first seed and `needs_default`
    // leaves it alone on every re-seed. It stays as the statement of the invariant
    // the two halves have to agree on: the workspace the seed deploys apps to is the
    // one the org root opens. Break that and the home page renders an empty grid.
    let sub = entity::prelude::OrgSubdomains::find()
        .filter(entity::org_subdomains::Column::OrgId.eq(org.id))
        .one(&db)
        .await
        .expect("query subdomain")
        .expect("acme subdomain");
    assert!(sub.enabled, "the seeded org subdomain must be enabled");
    assert_eq!(
        sub.default_workspace_id,
        Some(seeded_ws),
        "the org home's default workspace must be the one holding the apps"
    );
}
