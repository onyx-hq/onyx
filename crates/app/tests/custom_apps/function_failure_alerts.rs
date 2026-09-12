//! A failed custom-app function pages ops once, and only for a failure it has
//! not had in a week.
//!
//! The decision is SQL over the rows finalization writes, and the
//! at-most-once guarantee is an upsert. Both are properties of the queries
//! against Postgres, so they are tested there rather than restated in Rust.

use chrono::{DateTime, Duration, Utc};
use entity::{app_builds, app_function_invocations, apps, organizations, workspaces};
use oxy_app::server::api::custom_apps_functions::failure_alert::{
    DELIVERY_GRACE_MINUTES, FUNCTION_PAGE_WINDOW_HOURS, FailureKey, PAGES_PER_HOUR, RETENTION_DAYS,
    THRESHOLD, Verdict, claim, mark_delivered,
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    Statement,
};
use uuid::Uuid;

use crate::common::test_db;

const FUNCTION: &str = "upload-report";
const FINGERPRINT: &str = "5c1e0b8a9d2f4e71";

struct Seeded {
    app_id: Uuid,
    build_id: Uuid,
}

impl Seeded {
    fn key(&self) -> FailureKey<'static> {
        self.key_for(FUNCTION, FINGERPRINT)
    }

    fn key_for(&self, function: &'static str, fingerprint: &'static str) -> FailureKey<'static> {
        FailureKey {
            app_id: self.app_id,
            function_name: function,
            fingerprint,
        }
    }
}

/// Fingerprints have existed for longer than the lookback: no first-week hold.
fn long_ago() -> DateTime<Utc> {
    Utc::now() - Duration::days(30)
}

fn is_page(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::Page { .. })
}

async fn seed(db: &DatabaseConnection) -> Seeded {
    let org_id = Uuid::new_v4();
    organizations::ActiveModel {
        id: Set(org_id),
        name: Set("Failure Alerts Org".into()),
        slug: Set(format!("failure-alerts-{org_id}")),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed org");
    let workspace_id = Uuid::new_v4();
    workspaces::ActiveModel {
        id: Set(workspace_id),
        name: Set("Failure Alerts Workspace".into()),
        org_id: Set(Some(org_id)),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed workspace");
    let app_id = Uuid::new_v4();
    apps::ActiveModel {
        id: Set(app_id),
        slug: Set(format!("bookkeeping-{app_id}")),
        name: Set("Bookkeeping".into()),
        org_id: Set(org_id),
        project_id: Set(workspace_id),
        branch: Set("main".into()),
        source_repo: Set("failure-alerts/test".into()),
        status: Set("active".into()),
        source_type: Set("local".into()),
        source_config: Set(serde_json::json!({})),
        published_at: Set(Some(Utc::now().into())),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed app");
    let build_id = Uuid::new_v4();
    app_builds::ActiveModel {
        id: Set(build_id),
        app_id: Set(app_id),
        build_id: Set("b1".into()),
        s3_prefix: Set(format!("customer-apps/{app_id}/builds/b1/")),
        created_at: Set(Utc::now().into()),
        validation_status: Set("passed".into()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("seed build");
    Seeded { app_id, build_id }
}

/// A failed invocation row, `ago` in the past. `fingerprint: None` is a row
/// from before fingerprints existed.
async fn failed_as(
    db: &DatabaseConnection,
    s: &Seeded,
    function: &str,
    fingerprint: Option<&str>,
    ago: Duration,
) {
    app_function_invocations::ActiveModel {
        id: Set(Uuid::new_v4()),
        app_id: Set(s.app_id),
        build_id: Set(s.build_id),
        function_name: Set(function.into()),
        mode: Set("route".into()),
        user_id: Set(None),
        status: Set("error".into()),
        duration_ms: Set(Some(12)),
        error: Set(Some(
            "function threw: Error: warehouse insert failed".into(),
        )),
        cancel_requested_at: Set(None),
        created_at: Set((Utc::now() - ago).into()),
        idempotency_key: Set(None),
        result_body: Set(None),
        result_status: Set(None),
        request_hash: Set(None),
        failure_fingerprint: Set(fingerprint.map(str::to_string)),
    }
    .insert(db)
    .await
    .expect("seed invocation");
}

async fn failed(db: &DatabaseConnection, s: &Seeded, ago: Duration) {
    failed_as(db, s, FUNCTION, Some(FINGERPRINT), ago).await;
}

/// An alerts row written directly — history the test needs without replaying it.
async fn alert_row(
    db: &DatabaseConnection,
    key: FailureKey<'_>,
    claimed_ago: Duration,
    outcome: &str,
) {
    let at = (Utc::now() - claimed_ago).fixed_offset();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO app_function_failure_alerts \
           (app_id, function_name, failure_fingerprint, claimed_at, delivered_at, outcome) \
         VALUES ($1, $2, $3, $4, $4, $5)",
        [
            key.app_id.into(),
            key.function_name.into(),
            key.fingerprint.into(),
            at.into(),
            outcome.into(),
        ],
    ))
    .await
    .expect("seed alert row");
}

#[tokio::test]
async fn a_new_failure_pages_at_the_threshold_and_then_not_again() {
    let db = test_db().await;
    let s = seed(&db).await;

    for _ in 1..THRESHOLD {
        failed(&db, &s, Duration::minutes(2)).await;
    }
    let under = claim(&db, s.key(), Utc::now(), long_ago()).await.unwrap();
    assert_eq!(under, Verdict::Quiet, "a failure under the threshold waits");

    failed(&db, &s, Duration::minutes(1)).await;
    let page = claim(&db, s.key(), Utc::now(), long_ago()).await.unwrap();
    assert!(
        is_page(&page),
        "the occurrence that reaches the threshold pages: {page:?}"
    );

    // A second replica finalizing the same failure moments later.
    let again = claim(&db, s.key(), Utc::now(), long_ago()).await.unwrap();
    assert_eq!(
        again,
        Verdict::Quiet,
        "one page per failure, not per replica"
    );

    mark_delivered(&db, s.key(), Utc::now()).await.unwrap();
    failed(&db, &s, Duration::zero()).await;
    let later = Utc::now() + Duration::minutes(DELIVERY_GRACE_MINUTES + 1);
    let after_delivery = claim(&db, s.key(), later, long_ago()).await.unwrap();
    assert_eq!(
        after_delivery,
        Verdict::Quiet,
        "a delivered page is not resent"
    );
}

#[tokio::test]
async fn a_failure_the_function_already_had_this_week_does_not_page() {
    let db = test_db().await;
    let s = seed(&db).await;

    failed(&db, &s, Duration::days(3)).await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let known = claim(&db, s.key(), Utc::now(), long_ago()).await.unwrap();
    assert_eq!(
        known,
        Verdict::Quiet,
        "this failure is the function's, not new"
    );
}

#[tokio::test]
async fn another_function_or_fingerprint_is_a_separate_signal() {
    let db = test_db().await;
    let s = seed(&db).await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let now = Utc::now();

    let other_function = s.key_for("submit-receiving-report", FINGERPRINT);
    assert_eq!(
        claim(&db, other_function, now, long_ago()).await.unwrap(),
        Verdict::Quiet
    );
    let other_fingerprint = s.key_for(FUNCTION, "0000000000000000");
    assert_eq!(
        claim(&db, other_fingerprint, now, long_ago())
            .await
            .unwrap(),
        Verdict::Quiet
    );
    assert!(is_page(
        &claim(&db, s.key(), now, long_ago()).await.unwrap()
    ));
}

#[tokio::test]
async fn a_page_nobody_sent_goes_stale_and_is_claimed_again() {
    let db = test_db().await;
    let s = seed(&db).await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let now = Utc::now();
    assert!(is_page(
        &claim(&db, s.key(), now, long_ago()).await.unwrap()
    ));

    let within_grace = now + Duration::minutes(DELIVERY_GRACE_MINUTES - 1);
    let still_claimed = claim(&db, s.key(), within_grace, long_ago()).await.unwrap();
    assert_eq!(
        still_claimed,
        Verdict::Quiet,
        "the claimant may still be sending"
    );

    // The replica that claimed it died before posting.
    let past_grace = now + Duration::minutes(DELIVERY_GRACE_MINUTES + 1);
    let retaken = claim(&db, s.key(), past_grace, long_ago()).await.unwrap();
    assert!(
        is_page(&retaken),
        "an undelivered page is retried: {retaken:?}"
    );
}

#[tokio::test]
async fn a_failure_that_comes_back_after_a_quiet_week_pages_again() {
    let db = test_db().await;
    let s = seed(&db).await;
    // Paged eight days ago; the failure then stopped.
    failed(&db, &s, Duration::days(8)).await;
    alert_row(&db, s.key(), Duration::days(8), "paged").await;

    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let verdict = claim(&db, s.key(), Utc::now(), long_ago()).await.unwrap();
    assert!(is_page(&verdict), "a week-old page re-arms: {verdict:?}");
}

#[tokio::test]
async fn failures_from_before_fingerprints_hold_the_first_week_quiet() {
    let db = test_db().await;
    let s = seed(&db).await;
    let fingerprints_since = Utc::now() - Duration::hours(1);
    // A chronic failure, recorded before fingerprints existed.
    failed_as(&db, &s, FUNCTION, None, Duration::days(2)).await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }

    let verdict = claim(&db, s.key(), Utc::now(), fingerprints_since)
        .await
        .unwrap();
    assert_eq!(
        verdict,
        Verdict::Suppressed("suppressed:unfingerprinted_history")
    );
    failed(&db, &s, Duration::zero()).await;
    let next = claim(&db, s.key(), Utc::now(), fingerprints_since)
        .await
        .unwrap();
    assert_eq!(next, Verdict::Quiet, "held back once, then recorded");

    // A function with no failures before fingerprints still pages that week.
    for _ in 0..THRESHOLD {
        failed_as(
            &db,
            &s,
            "submit-receiving-report",
            Some(FINGERPRINT),
            Duration::minutes(1),
        )
        .await;
    }
    let clean = s.key_for("submit-receiving-report", FINGERPRINT);
    assert!(is_page(
        &claim(&db, clean, Utc::now(), fingerprints_since)
            .await
            .unwrap()
    ));
}

#[tokio::test]
async fn a_chronic_5xx_from_before_fingerprints_is_held_too() {
    let db = test_db().await;
    let s = seed(&db).await;
    let fingerprints_since = Utc::now() - Duration::hours(1);
    // A scheduled run that answered 500 before fingerprints existed: a
    // `success` with its status stored.
    app_function_invocations::ActiveModel {
        id: Set(Uuid::new_v4()),
        app_id: Set(s.app_id),
        build_id: Set(s.build_id),
        function_name: Set(FUNCTION.into()),
        mode: Set("schedule".into()),
        user_id: Set(None),
        status: Set("success".into()),
        duration_ms: Set(Some(12)),
        error: Set(None),
        cancel_requested_at: Set(None),
        created_at: Set((Utc::now() - Duration::days(2)).into()),
        idempotency_key: Set(None),
        result_body: Set(Some("{}".into())),
        result_status: Set(Some(500)),
        request_hash: Set(None),
        failure_fingerprint: Set(None),
    }
    .insert(&db)
    .await
    .expect("seed 5xx invocation");
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let verdict = claim(&db, s.key(), Utc::now(), fingerprints_since)
        .await
        .unwrap();
    assert_eq!(
        verdict,
        Verdict::Suppressed("suppressed:unfingerprinted_history")
    );
}

#[tokio::test]
async fn a_function_pages_at_most_once_per_window() {
    let db = test_db().await;
    let s = seed(&db).await;
    alert_row(
        &db,
        s.key_for(FUNCTION, "aaaaaaaaaaaaaaaa"),
        Duration::hours(1),
        "paged",
    )
    .await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let now = Utc::now();
    let verdict = claim(&db, s.key(), now, long_ago()).await.unwrap();
    assert_eq!(verdict, Verdict::Suppressed("suppressed:function_rate"));

    let within_window = now + Duration::hours(FUNCTION_PAGE_WINDOW_HOURS - 1);
    let held = claim(&db, s.key(), within_window, long_ago())
        .await
        .unwrap();
    assert_eq!(held, Verdict::Quiet, "the hold stands for its window");
}

#[tokio::test]
async fn a_rate_held_failure_pages_once_its_window_passes_however_late() {
    let db = test_db().await;
    let s = seed(&db).await;
    alert_row(
        &db,
        s.key_for(FUNCTION, "aaaaaaaaaaaaaaaa"),
        Duration::hours(1),
        "paged",
    )
    .await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let now = Utc::now();
    assert_eq!(
        claim(&db, s.key(), now, long_ago()).await.unwrap(),
        Verdict::Suppressed("suppressed:function_rate")
    );

    // Still failing well past the window — and past the day in which it counted
    // as new. A failure held back for being one too many must not be silenced
    // for arriving late.
    failed(&db, &s, Duration::zero()).await;
    let day_and_a_half_later = now + Duration::hours(36);
    let verdict = claim(&db, s.key(), day_and_a_half_later, long_ago())
        .await
        .unwrap();
    assert!(is_page(&verdict), "the held failure pages: {verdict:?}");
}

#[tokio::test]
async fn the_platform_pages_at_most_so_many_times_an_hour() {
    let db = test_db().await;
    let s = seed(&db).await;
    const OTHERS: [&str; 10] = ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9"];
    assert_eq!(OTHERS.len() as i64, PAGES_PER_HOUR);
    for other in OTHERS {
        alert_row(
            &db,
            s.key_for(other, FINGERPRINT),
            Duration::minutes(10),
            "paged",
        )
        .await;
    }
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }
    let now = Utc::now();
    let verdict = claim(&db, s.key(), now, long_ago()).await.unwrap();
    assert_eq!(verdict, Verdict::Suppressed("suppressed:platform_rate"));

    let within_hour = now + Duration::minutes(30);
    assert_eq!(
        claim(&db, s.key(), within_hour, long_ago()).await.unwrap(),
        Verdict::Quiet,
        "the hold stands for its hour"
    );
    // The other pages were ten minutes old at `now`; an hour on, they no
    // longer count, and the held failure goes out.
    let past_hour = now + Duration::minutes(61);
    let verdict = claim(&db, s.key(), past_hour, long_ago()).await.unwrap();
    assert!(is_page(&verdict), "the held failure pages: {verdict:?}");
}

#[tokio::test]
async fn a_page_prunes_alert_rows_past_retention() {
    let db = test_db().await;
    let s = seed(&db).await;
    let ancient = s.key_for("retired-function", FINGERPRINT);
    alert_row(&db, ancient, Duration::days(RETENTION_DAYS + 1), "paged").await;
    for _ in 0..THRESHOLD {
        failed(&db, &s, Duration::minutes(1)).await;
    }

    assert!(is_page(
        &claim(&db, s.key(), Utc::now(), long_ago()).await.unwrap()
    ));
    let left = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM app_function_failure_alerts WHERE function_name = $1",
            ["retired-function".into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(left, 0, "a row past retention is gone after the next page");
}
