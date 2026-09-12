//! Send the page `failure_alert` decides on: the finalization hook, the
//! message, and the ops Slack channel it goes to.
//!
//! With no `OXY_OPS_SLACK_BOT_TOKEN` / `OXY_OPS_SLACK_CHANNEL` (the pair
//! Workspace Health pages with) nothing is claimed or sent; the WARN line from
//! `failure_signal` still goes out.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use uuid::Uuid;

use super::failure_alert::{FailureKey, LOOKBACK_DAYS, THRESHOLD, Verdict, claim, mark_delivered};
use super::failure_signal::Failure;

/// The migration that added `failure_fingerprint`. Invocations from before it
/// was applied carry none.
const FINGERPRINT_MIGRATION: &str = "m20260911_000001_function_failure_alerts";

/// How long the Slack post may take before it is abandoned for a later retry.
const POST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The finalization hook: page ops if `failure` is new for this function.
pub(super) async fn observe(
    db: &DatabaseConnection,
    app: &entity::apps::Model,
    function_name: &str,
    invocation_id: Uuid,
    failure: &Failure,
) {
    let Some((token, channel)) = ops_slack_target() else {
        return;
    };
    let key = FailureKey {
        app_id: app.id,
        function_name,
        fingerprint: &failure.fingerprint,
    };
    let first_seen = match claim(db, key, Utc::now(), history_since(db).await).await {
        Ok(Verdict::Page { first_seen }) => first_seen,
        Ok(Verdict::Quiet) => return,
        Ok(Verdict::Suppressed(reason)) => {
            tracing::info!(target: "oxy::app_function", reason, "failure alert: held back");
            return;
        }
        Err(e) => {
            tracing::warn!(target: "oxy::app_function", error = %e, "failure alert: claim failed");
            return;
        }
    };
    let text = message(app, key, invocation_id, failure.kind, first_seen);
    let (db, app_id) = (db.clone(), app.id);
    let (function_name, fingerprint) = (function_name.to_string(), failure.fingerprint.clone());
    // Spawned, not a TaskSpec (a deliberate departure from
    // `oxy-task-spec-default`): one capped post whose delivery the claim row
    // already tracks. A replica that dies here leaves the claim unsent; it goes
    // stale and the next failure sends it.
    tokio::spawn(async move {
        let client = oxy_slack_client::SlackClient::new();
        let post = client.chat_post_message(&token, &channel, &text, None);
        let error = match tokio::time::timeout(POST_TIMEOUT, post).await {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some(format!("timed out after {POST_TIMEOUT:?}")),
        };
        if let Some(error) = error {
            tracing::warn!(
                target: "oxy::app_function",
                error,
                "failure alert: Slack post failed; the next failure retries once the claim is stale"
            );
            return;
        }
        let key = FailureKey {
            app_id,
            function_name: &function_name,
            fingerprint: &fingerprint,
        };
        if let Err(e) = mark_delivered(&db, key, Utc::now()).await {
            tracing::warn!(target: "oxy::app_function", error = %e, "failure alert: mark delivered failed");
        }
    });
}

/// When invocations started carrying a fingerprint: the migration's
/// `applied_at`, read once per process. Unknown reads as now — the whole
/// lookback treated as unfingerprinted, quiet rather than noisy — and is not
/// cached, so a transient read error does not stick.
async fn history_since(db: &DatabaseConnection) -> DateTime<Utc> {
    static SINCE: tokio::sync::OnceCell<DateTime<Utc>> = tokio::sync::OnceCell::const_new();
    if let Some(since) = SINCE.get() {
        return *since;
    }
    let applied = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT applied_at FROM seaql_migrations WHERE version = $1",
            [FINGERPRINT_MIGRATION.into()],
        ))
        .await
        .ok()
        .flatten()
        .and_then(|row| row.try_get::<i64>("", "applied_at").ok())
        .and_then(|secs| DateTime::from_timestamp(secs, 0));
    match applied {
        Some(since) => *SINCE.get_or_init(|| async { since }).await,
        None => Utc::now(),
    }
}

/// The page. Names the function and the shape of the failure, never its
/// message: that stays in `app_function_invocations.error`, where reading it
/// takes the same assume-role session as any other tenant data.
fn message(
    app: &entity::apps::Model,
    key: FailureKey<'_>,
    invocation_id: Uuid,
    kind: &str,
    first_seen: DateTime<Utc>,
) -> String {
    format!(
        ":rotating_light: Custom-app function `{slug}/{function}` is failing in a way it has \
         not in {LOOKBACK_DAYS} days: {THRESHOLD}+ failed invocations since {first} UTC.\n\
         • kind `{kind}` · fingerprint `{fingerprint}`\n\
         • app `{app_id}` in org `{org_id}` · latest invocation `{invocation_id}`\n\
         The message is in `app_function_invocations.error`; the same fingerprint on other \
         functions points at the platform rather than the app.",
        slug = app.slug,
        function = key.function_name,
        first = first_seen.format("%Y-%m-%d %H:%M"),
        fingerprint = key.fingerprint,
        app_id = app.id,
        org_id = app.org_id,
    )
}

/// The ops Slack bot token and channel — the env pair Workspace Health pages
/// with, read here too because that module sits outside the custom-apps
/// boundary. Unset or empty turns failure alerts off.
fn ops_slack_target() -> Option<(String, String)> {
    let var = |name| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    Some((
        var("OXY_OPS_SLACK_BOT_TOKEN")?,
        var("OXY_OPS_SLACK_CHANNEL")?,
    ))
}
