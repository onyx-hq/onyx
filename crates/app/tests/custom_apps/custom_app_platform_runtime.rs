//! The platform's browser-side runtime, end to end against the seeded app:
//! the reserved `__oxy/` namespace, the asset manifest, the preload hints the
//! manifest produces, and the telemetry beacon.
//!
//! These are integration tests rather than unit tests because every one of them
//! is about a *seam*. Each piece has unit coverage already — the manifest
//! builder, the beacon's admission rules, the header assembly — and each passed
//! while the whole was broken in at least one of the ways below:
//!
//! - the reserved dispatch sits between the auth gate and the source dispatch,
//!   so a misplacement serves the worker to the wrong people or 404s it;
//! - the manifest is written at publish and read at serve through the object
//!   store, so the two halves can disagree about its path or its shape;
//! - the `Link` header exists only if the manifest was written, was found, and
//!   parsed — three separate ways to end up with a silently slower app that
//!   still renders perfectly.
//!
//! Auth: `BuiltInAuthenticator` falls back to a guest identity when no auth
//! method is configured. The guest is a plain user with no standing, so the
//! membership gate is exercised for real.

use crate::common::{APP_SLUG, examples_path, test_db};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::any;
use chrono::Utc;
use entity::prelude::Organizations;
use entity::{org_members, org_members::OrgRole, organizations};
use oxy_app::cli::commands::seed;
use oxy_app::server::api::custom_apps_asset_manifest::{AssetManifest, SCHEMA_VERSION};
use oxy_app::server::api::custom_apps_serve;
use oxy_auth::types::Identity;
use oxy_auth::user::UserService;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter,
};
use tower::ServiceExt;
use uuid::Uuid;

/// `any`, not `get`: the beacon is a POST on the same wildcard route the bundle
/// serves from, exactly as `serve.rs` mounts it. Mounting `get` here would make
/// every beacon test 405 for a reason that has nothing to do with the code under
/// test.
fn router() -> Router {
    Router::new().route(
        "/customer-apps/{*path}",
        any(custom_apps_serve::serve_dispatch),
    )
}

struct Sent {
    status: StatusCode,
    headers: header::HeaderMap,
    body: String,
}

async fn send(request: Request<Body>) -> Sent {
    let response = router().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    Sent {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(uri: &str) -> Sent {
    send(Request::builder().uri(uri).body(Body::empty()).unwrap()).await
}

async fn post_json(uri: &str, body: &str) -> Sent {
    send(
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

fn header_of(sent: &Sent, name: &str) -> Option<String> {
    sent.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Make the guest a member of `org_id` — the serve path authenticates as the
/// guest and `user_can_access_app` requires membership, which the seed does not
/// grant.
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

async fn seeded_local_member() -> DatabaseConnection {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");
    add_guest_to_org(&db, org_id(&db, "local").await).await;
    db
}

fn base() -> String {
    format!("/customer-apps/local/{APP_SLUG}")
}

// ── The reserved namespace ───────────────────────────────────────────────────

#[tokio::test]
async fn the_service_worker_serves_with_a_scope_wide_enough_to_be_useful() {
    let _db = seeded_local_member().await;

    let sent = get(&format!("{}/__oxy/sw.js", base())).await;
    assert_eq!(sent.status, StatusCode::OK, "body: {}", sent.body);

    // Without this header the worker's scope is its own `__oxy/` directory,
    // which contains nothing worth intercepting — `register({scope})` fails and
    // the app silently keeps the cold path forever.
    assert_eq!(
        header_of(&sent, "service-worker-allowed").as_deref(),
        Some(format!("{}/", base()).as_str()),
        "the worker must be allowed to claim the app root"
    );
    assert_eq!(
        header_of(&sent, "cache-control").as_deref(),
        Some("private, no-cache"),
        "a cached worker script is a worker that cannot be fixed"
    );
    assert!(
        header_of(&sent, "content-type").is_some_and(|v| v.contains("javascript")),
        "browsers refuse to register a worker served as anything else"
    );
    assert!(
        sent.body.contains("addEventListener"),
        "body: {}",
        sent.body
    );
}

/// The prefix is reserved, so a miss under it must not fall through to the
/// bundle — otherwise an app could serve its own file at a platform URL, and a
/// future platform endpoint would collide with whatever the app already had
/// there.
#[tokio::test]
async fn an_unknown_reserved_path_is_a_404_and_never_the_bundle() {
    let _db = seeded_local_member().await;

    let sent = get(&format!("{}/__oxy/not-a-thing", base())).await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert!(
        !sent.body.contains("<html"),
        "a reserved miss fell through to the SPA shell: {}",
        sent.body
    );
    // The set of platform endpoints grows; a client that cached "no such thing"
    // would keep believing it across the deploy that added one.
    assert_eq!(
        header_of(&sent, "cache-control").as_deref(),
        Some("no-store")
    );
}

/// The whole namespace sits behind the app's own gate. A visitor who cannot open
/// the bundle must not be able to register its worker or write to its telemetry.
#[tokio::test]
async fn the_reserved_namespace_is_behind_the_apps_own_gate() {
    let db = test_db().await;
    seed::seed_demo(Some(examples_path())).await.expect("seed");
    // The guest joins `local` only; Acme's app is published but not theirs.
    add_guest_to_org(&db, org_id(&db, "local").await).await;

    let worker = get(&format!("/customer-apps/acme/{APP_SLUG}/__oxy/sw.js")).await;
    assert_eq!(worker.status, StatusCode::FORBIDDEN);

    let beacon = post_json(
        &format!("/customer-apps/acme/{APP_SLUG}/__oxy/beacon"),
        r#"{"v":1,"events":[{"n":"oxy-pageview","p":{"path":"/"}}]}"#,
    )
    .await;
    assert_eq!(
        beacon.status,
        StatusCode::FORBIDDEN,
        "a non-member wrote into another org's app telemetry"
    );
}

// ── The asset manifest and what it produces ──────────────────────────────────

/// The seed installs the manifest through the same helper `oxyc publish` uses, so
/// this pins both halves of that seam: publish writes it at the path serve reads
/// from, in the shape serve parses.
#[tokio::test]
async fn the_build_carries_a_readable_asset_manifest() {
    let _db = seeded_local_member().await;

    let sent = get(&format!("{}/__oxy/asset-manifest.json", base())).await;
    assert_eq!(sent.status, StatusCode::OK, "body: {}", sent.body);

    let manifest: AssetManifest =
        serde_json::from_str(&sent.body).unwrap_or_else(|e| panic!("{e}: {}", sent.body));
    assert_eq!(manifest.schema_version, SCHEMA_VERSION);
    assert!(!manifest.build_id.is_empty());
    // Defaults, since the example bundle opts out of neither.
    assert!(manifest.client.service_worker && manifest.client.analytics);

    // The starter ships a hashed stylesheet + module entry under `assets/`, so
    // the manifest must name both — this is the end-to-end proof that publish
    // parsed the built `index.html` and recorded its critical path. Asserted by
    // shape (a `.css` and a `.js` under `assets/`) rather than exact filename,
    // so bumping the content hash on an edit doesn't break the test.
    let entry_paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
    assert!(
        entry_paths
            .iter()
            .any(|p| p.starts_with("assets/") && p.ends_with(".css")),
        "entries should name the hashed stylesheet: {entry_paths:?}"
    );
    assert!(
        entry_paths
            .iter()
            .any(|p| p.starts_with("assets/") && p.ends_with(".js")),
        "entries should name the hashed module: {entry_paths:?}"
    );
    // Every entry is also in the precacheable `assets` set the worker pins.
    for entry in &manifest.entries {
        assert!(
            manifest.assets.contains(&entry.path),
            "entry {} is missing from the precache set {:?}",
            entry.path,
            manifest.assets
        );
    }

    // The document the serve path reads and the build the serve path serves
    // must be the same build. This is the cross-check that catches a manifest
    // resolved from the wrong prefix — which would otherwise degrade silently
    // to "no hints" and look exactly like a bundle that has none.
    let shell = get(&format!("{}/", base())).await;
    assert_eq!(
        header_of(&shell, "x-oxy-build").as_deref(),
        Some(manifest.build_id.as_str()),
        "the shell and the manifest disagree about which build is live"
    );

    // The worker fetches this document by these exact camelCase names.
    assert!(sent.body.contains("\"schemaVersion\""), "{}", sent.body);
    assert!(sent.body.contains("\"buildId\""), "{}", sent.body);

    // Per-build state behind an auth gate: a shared cache must not store it and
    // hand a stale precache list — or the document itself — to someone who never
    // passed the gate.
    assert_eq!(
        header_of(&sent, "cache-control").as_deref(),
        Some("private, no-cache")
    );
}

/// The payoff, end to end: a real serve of the seeded build carries the preload
/// hints, the build identity, and the purge tag on the shell.
///
/// The `Link` header is the whole point of the split — the entry chunks start
/// downloading while the HTML is still in flight. This is the integration-level
/// counterpart to the unit test in `custom_apps_serve::sources`, which pins the
/// entry→header rendering; here we prove a real published build actually reaches
/// it.
#[tokio::test]
async fn the_shell_preloads_the_seeded_builds_entry_assets() {
    let _db = seeded_local_member().await;

    let sent = get(&format!("{}/", base())).await;
    assert_eq!(sent.status, StatusCode::OK);

    let link = header_of(&sent, "link")
        .unwrap_or_else(|| panic!("the shell shipped without preload hints"));
    // Absolute under the app's base, and covering both entry kinds.
    assert!(
        link.contains(&format!("<{}/assets/", base())),
        "hints must be absolute under the app base: {link}"
    );
    assert!(
        link.contains("rel=preload; as=style"),
        "stylesheet hint missing: {link}"
    );
    assert!(
        link.contains("rel=modulepreload"),
        "module hint missing: {link}"
    );

    // The running worker reads this off every navigation to notice a publish
    // without waiting to be replaced.
    assert!(
        header_of(&sent, "x-oxy-build").is_some_and(|v| !v.is_empty()),
        "the shell must name the live build"
    );
    // Inert at the origin; a tag-capable edge would purge one app with it.
    assert!(
        header_of(&sent, "cache-tag").is_some_and(|v| v.starts_with("app-")),
        "{:?}",
        header_of(&sent, "cache-tag")
    );
    // HTML must stay `private`: it carries a per-visitor tracking Set-Cookie,
    // and `no-cache` alone still lets a shared cache store it.
    assert_eq!(
        header_of(&sent, "cache-control").as_deref(),
        Some("private, no-cache")
    );
}

/// A 304 has no body, so it cannot carry the document's own `<link>` tags —
/// which makes the header matter *more* on a revalidation, not less.
#[tokio::test]
async fn a_revalidated_shell_still_carries_its_hints() {
    let _db = seeded_local_member().await;

    let first = get(&format!("{}/", base())).await;
    let etag = header_of(&first, "etag").expect("shell must carry a weak ETag");
    // The starter has entries, so `first` carries hints — otherwise the
    // equality check below would be a vacuous None == None.
    assert!(
        header_of(&first, "link").is_some(),
        "precondition: the 200 has hints to carry"
    );

    let second = send(
        Request::builder()
            .uri(format!("{}/", base()))
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(second.status, StatusCode::NOT_MODIFIED);
    assert!(second.body.is_empty());
    assert_eq!(header_of(&second, "etag").as_deref(), Some(etag.as_str()));
    // Everything the 200 carried and the empty body cannot: the build identity
    // the worker reads, the cache policy, and (for a bundle that has any) the
    // preload hints, which matter MORE on a revalidation because there is no
    // document to carry `<link>` tags.
    assert_eq!(
        header_of(&second, "x-oxy-build"),
        header_of(&first, "x-oxy-build")
    );
    assert_eq!(header_of(&second, "link"), header_of(&first, "link"));
    assert_eq!(
        header_of(&second, "cache-control").as_deref(),
        Some("private, no-cache")
    );
}

/// The client runtime is injected next to `window.__OXY_APP__`, which it reads.
/// If it stops shipping, nothing breaks visibly — the app renders, and only the
/// worker and the analytics quietly stop existing.
#[tokio::test]
async fn the_shell_ships_the_client_runtime_next_to_the_identity() {
    let _db = seeded_local_member().await;

    let sent = get(&format!("{}/", base())).await;
    assert_eq!(sent.status, StatusCode::OK);

    let identity_at = sent
        .body
        .find("window.__OXY_APP__")
        .expect("runtime identity was not injected");
    let runtime_at = sent
        .body
        .find("__oxy/sw.js")
        .expect("the client runtime was not injected");
    assert!(
        identity_at < runtime_at,
        "the runtime reads __OXY_APP__ and must come after it"
    );
    assert!(sent.body.contains("__oxy/beacon"), "no beacon endpoint");
    // The base path the runtime uses to build both URLs.
    assert!(sent.body.contains(&format!("{}/", base())), "no basePath");
}

// ── The beacon ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_beacon_accepts_a_platform_batch_and_refuses_anything_else() {
    let _db = seeded_local_member().await;
    let url = format!("{}/__oxy/beacon", base());

    let ok = post_json(
        &url,
        r#"{"v":1,"events":[
            {"n":"oxy-pageview","t":1730000000000,"p":{"path":"/orders","kind":"spa"}},
            {"n":"oxy-web-vitals","p":{"lcp":812,"cls":3}}
        ]}"#,
    )
    .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "body: {}", ok.body);

    // `oxy-*` is the platform's namespace and this route is its only writer, so
    // it takes a fixed allowlist rather than a prefix check — otherwise any
    // viewer of the app could write rows the Activity tab treats as measured.
    let foreign = post_json(&url, r#"{"v":1,"events":[{"n":"export-clicked","p":{}}]}"#).await;
    assert_eq!(foreign.status, StatusCode::BAD_REQUEST);

    let unknown_platform_name =
        post_json(&url, r#"{"v":1,"events":[{"n":"oxy-anything","p":{}}]}"#).await;
    assert_eq!(unknown_platform_name.status, StatusCode::BAD_REQUEST);

    let bad_version = post_json(&url, r#"{"v":2,"events":[]}"#).await;
    assert_eq!(bad_version.status, StatusCode::BAD_REQUEST);
}

/// A beacon is not a page load. It is a POST to a non-HTML path, so the view
/// gate excludes it twice over — but the gate is a single boolean expression
/// three commits from someone widening it, and the symptom would be an Activity
/// tab reporting four "opens" for one visit.
#[tokio::test]
async fn a_beacon_is_not_recorded_as_a_view() {
    let db = seeded_local_member().await;
    let before = entity::prelude::CustomAppViewEvent::find()
        .all(&db)
        .await
        .expect("count views")
        .len();

    for _ in 0..3 {
        let sent = post_json(
            &format!("{}/__oxy/beacon", base()),
            r#"{"v":1,"events":[{"n":"oxy-pageview","p":{"path":"/x"}}]}"#,
        )
        .await;
        assert_eq!(sent.status, StatusCode::NO_CONTENT);
    }

    let after = entity::prelude::CustomAppViewEvent::find()
        .all(&db)
        .await
        .expect("count views")
        .len();
    assert_eq!(before, after, "beacons were counted as page views");
}

/// The launcher warms an app when the pointer enters its card. Recording that
/// as an open would make "who used this app" mean "whose pointer passed over
/// it" — and would mint the visitor's tracking session at hover time, which the
/// real navigation would then inherit.
#[tokio::test]
async fn a_prefetched_shell_is_served_but_not_counted() {
    let db = seeded_local_member().await;

    let sent = send(
        Request::builder()
            .uri(format!("{}/", base()))
            .header("sec-purpose", "prefetch")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    // Served in full — the point of a prefetch is that the bytes arrive.
    assert_eq!(sent.status, StatusCode::OK);
    assert!(sent.body.contains("window.__OXY_APP__"));
    // …but no session is started for a page nobody has opened yet.
    assert!(
        sent.headers.get(header::SET_COOKIE).is_none(),
        "a prefetch minted a tracking session"
    );

    // View recording is a `tokio::spawn`, so a negative assertion has to allow
    // the spawn to have run. A generous yield is the honest way to say "we
    // waited"; the positive case below is what proves the recorder works at all.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let after_prefetch = entity::prelude::CustomAppViewEvent::find()
        .all(&db)
        .await
        .expect("count views")
        .len();
    assert_eq!(after_prefetch, 0, "a prefetch was recorded as a view");

    // The same URL without the header is a real open, and does count — so the
    // test above is measuring the header, not a broken recorder. Without this
    // half, a recorder that had stopped working entirely would pass.
    let real = get(&format!("{}/", base())).await;
    assert_eq!(real.status, StatusCode::OK);
    assert!(
        real.headers.get(header::SET_COOKIE).is_some(),
        "a real navigation must start a tracking session"
    );

    // Await the recorder's spawn rather than ending the test under it — a task
    // still writing when the process tears down is what nextest reports as a
    // leaky test, and it would make the row count above a race.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after_navigation = entity::prelude::CustomAppViewEvent::find()
        .all(&db)
        .await
        .expect("count views")
        .len();
    assert_eq!(
        after_navigation, 1,
        "a real navigation must be recorded, or the negative assertion above proves nothing"
    );
}
