//! `POST /customer-apps/<org>/<slug>/fn/<name>` — Oxy Functions route
//! invocation.
//!
//! See `internal-docs/customer-apps-functions.md`. This
//! handler covers the **route** invocation mode (§2); schedule and Airway
//! triggers reuse [`crate::server::api::custom_apps_functions::runtime::run`]
//! from their own call sites and are not wired here.
//!
//! **Three run concepts, easy to conflate** (see the "Run concepts" section of
//! `internal-docs/customer-apps-functions.md`): the **V8 isolate
//! run** (`runtime::run`) is the raw JS execution; an **invocation**
//! (`app_function_invocations`, `mode`) is the audit row for *every* run —
//! including a plain request-time **route** call; a **job** (`agentic_runs`,
//! `source_type="app_function"`) is the *durable, orchestrated, monitored*
//! background run (schedule/manual) that also produces an invocation. A route
//! call is an invocation only — no durable run, not in the coordinator. A job
//! runs the SAME isolate, wrapped in the queue/monitoring/trigger machinery.

/// The audit record for every data-plane write a function makes (session
/// tag, statement trailer, `audit_events` row).
#[cfg(feature = "custom-app-functions")]
mod data_audit;
/// Whether a failed function should page ops: new in a week, capped, once.
pub mod failure_alert;
/// The page `failure_alert` decides on: the finalization hook and Slack post.
mod failure_page;
/// A failed invocation's kind and fingerprint — the failure without its payload.
mod failure_signal;
#[cfg(feature = "custom-app-functions")]
pub mod host;
/// What a host op may say about itself on the platform trace (shape, never
/// payload) — the helpers behind `runtime`'s per-op spans.
#[cfg(feature = "custom-app-functions")]
mod host_call_attrs;
mod result_cache;
#[cfg(feature = "custom-app-functions")]
pub mod runtime;
/// The always-compiled dependency-inversion seam (see `seam.rs`).
pub mod seam;
/// Per-invocation registry of open `ctx.tx()` transactions.
#[cfg(feature = "custom-app-functions")]
mod tx;
/// Which warehouses `ctx.warehouse.upsert` can reach, refused by name.
#[cfg(feature = "custom-app-functions")]
mod upsert_support;

/// Named so `ProjectFunctionHost::new` can be called from outside the crate —
/// the engine-backed tests in `tests/custom_apps/` build a real host.
#[cfg(feature = "custom-app-functions")]
pub use data_audit::InvocationIdentity;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
#[cfg(feature = "custom-app-functions")]
use entity::prelude::AppFunctionInvocations;
use entity::prelude::{AppBuilds, AppFunctions};
use entity::{app_function_invocations, app_functions};
use oxy::database::client::establish_connection;
use oxy_shared::utils::request_id::from_headers as request_id_from_headers;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::Deserialize;
use tracing::error;
use uuid::Uuid;

use super::custom_apps_auth::authenticate_and_authorize;
use super::custom_apps_build_store;

// ── Rate limit: 60/min/(user, app, function) by default ────────────────────
// Mirrors the (user, app) `RateBucket` in `custom_apps_activity.rs`
// (design doc §11.6), extended with the function name.
//
// TODO(scaling): this table is per-process, so in the multi-instance worker
// fleet (see oxy-scaling-design) the effective limit is `limit * N` instances,
// not `limit`. Acceptable for the MVP; back this with a shared counter (e.g.
// a Postgres-backed sliding window) before relying on it as a hard cap.

const DEFAULT_RATE_PER_MIN: u64 = 60;
const RATE_BUCKET_TTL: Duration = Duration::from_secs(120);

#[derive(Debug)]
struct RateBucket {
    window_start: Instant,
    count: u64,
}

fn rate_table() -> &'static Mutex<HashMap<(Uuid, Uuid, String), RateBucket>> {
    static TABLE: std::sync::OnceLock<Mutex<HashMap<(Uuid, Uuid, String), RateBucket>>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn would_exceed_rate(user_id: Uuid, app_id: Uuid, function: &str, limit_per_min: u64) -> bool {
    let now = Instant::now();
    let mut table = rate_table().lock().unwrap();
    table.retain(|_, b| now.duration_since(b.window_start) < RATE_BUCKET_TTL);

    let key = (user_id, app_id, function.to_string());
    let bucket = table.entry(key).or_insert_with(|| RateBucket {
        window_start: now,
        count: 0,
    });
    if now.duration_since(bucket.window_start).as_secs() >= 60 {
        bucket.window_start = now;
        bucket.count = 0;
    }
    bucket.count += 1;
    bucket.count > limit_per_min
}

// ── Manifest entry shape (subset relevant to invocation) ────────────────────

#[derive(Debug, Deserialize, Default)]
struct FunctionManifestEntry {
    #[serde(default)]
    route: Option<bool>,
    #[serde(default, rename = "timeoutSeconds")]
    timeout_seconds: Option<u32>,
    #[serde(default, rename = "rateLimitPerMinute")]
    rate_limit_per_minute: Option<u64>,
    /// Raise this function's `ctx.fetch` response ceiling above the 10 MiB
    /// default. Opt-in per function rather than a global env var, because the
    /// cap protects a SHARED process: one serve instance hosts many apps, and a
    /// response is read fully into memory and then into a JS string (×1.33 when
    /// base64). One app needing a large document should not raise the ceiling
    /// for every app beside it.
    #[serde(default)]
    fetch: Option<FetchSpec>,
    /// Opt-in result caching. Absent → never cached (the safe default for a
    /// side-effectful function). Present with a positive `ttlSeconds` → a route
    /// invocation's result is cached per (build, function, user, body) for that
    /// window. Only declare this for read-only / idempotent functions.
    #[serde(default)]
    cache: Option<CacheSpec>,
    /// Databases this function's `ctx.warehouse.*` writes may target. Absent or
    /// empty → the function may NOT write to any database (fail-closed): a write
    /// is rejected before any connector — and therefore any ephemeral credential
    /// — is built. Read-only functions omit it. This is the §11.3 app-scoped
    /// destination allowlist; without it a function could `ctx.warehouse.exec`
    /// arbitrary DDL/DML against the project's source warehouse.
    #[serde(default)]
    destinations: Option<Vec<String>>,
    /// Opt-in capability to write app-scoped secrets via `ctx.secrets.set`
    /// (fail-closed: absent → writes rejected). Only the app's own
    /// `apps/<app_id>/` namespace is writable. Declare for functions that
    /// persist state (e.g. a scheduled token-refresher).
    #[serde(default)]
    secrets: Option<SecretsSpec>,
    /// Opt-in capability to send email via `ctx.email.send` (fail-closed: absent
    /// → sends rejected). The platform controls the `from` address; the function
    /// may set `replyTo` only. Declare for functions that email end-users.
    #[serde(default)]
    email: Option<EmailSpec>,
    /// Opt-in capability for `ctx.storage` (fail-closed: absent → all storage ops
    /// rejected). `read` permits getDownloadUrl/list/get; `write` permits
    /// getUploadUrl/put. Scoped to the app's own `customer-app-storage/<app_id>/`
    /// silo. Declare for functions that accept uploads or serve stored files.
    #[serde(default)]
    storage: Option<StorageSpec>,
    /// Opt-in capability for `ctx.oltp` — read/write the app's OWN per-org OLTP
    /// schema on the managed Postgres tenant, and nothing else (fail-closed:
    /// absent → every `ctx.oltp` call rejected). A pure GATE: the target schema
    /// is `app_<writer>` where the writer is DERIVED from the app's own slug
    /// host-side, never taken from here — so a manifest cannot point `ctx.oltp`
    /// at another app's schema. The resolved role has DML rights on that one
    /// schema, reaching neither another app's data nor the analyst-visible
    /// `raw_*` schemas. The writer must be provisioned first.
    #[serde(default)]
    oltp: Option<OltpSpec>,
    #[serde(default)]
    webhook: Option<WebhookSpec>,
    /// Opt-in capability for `ctx.org.people()` — the org's people directory
    /// (fail-closed: absent → the call is rejected).
    ///
    /// Exists because an app that models work assigned to humans currently has
    /// no way to name one. A function cannot reach `GET /api/orgs/{org}/members`,
    /// so apps invent their own user ids and hold no human name at all — which
    /// is why one of them drops a "who did this" column rather than guess.
    ///
    /// **Read-only, and deliberately not a contact list.** It answers with
    /// display names and roles — not locations, which the platform does not hold
    /// for a member. It DOES include frontline workers holding a grant on this
    /// app, tagged `kind`, because the directory names whoever can reach the app.
    /// It does not return email addresses or
    /// phone numbers. Naming somebody on a roster is a different need from being
    /// able to message them off-platform, and only the first one is what the
    /// missing capability was blocking. Widening it is a product decision, not a
    /// follow-up.
    #[serde(default)]
    org: Option<OrgSpec>,
    /// Retry policy for **background** runs (scheduled or manual job triggers) —
    /// absent → a job run is attempted once. Route invocations are request-scoped
    /// and never retried. Maps to the durable queue's `RetryPolicy` so a transient
    /// failure re-runs the whole isolate with exponential backoff. See the Oxy
    /// Function Jobs design (2026-07-10).
    #[serde(default)]
    retries: Option<RetriesSpec>,
}

/// Manifest retry block for background function jobs. `maxAttempts` counts the
/// first try, so `maxAttempts: 3` = up to 2 retries. Backoff is exponential
/// (doubling) between `minTimeoutMs` and `maxTimeoutMs`.
#[derive(Debug, Deserialize, Default, Clone)]
struct RetriesSpec {
    #[serde(rename = "maxAttempts")]
    max_attempts: Option<u32>,
    #[serde(rename = "minTimeoutMs")]
    min_timeout_ms: Option<u64>,
    #[serde(rename = "maxTimeoutMs")]
    max_timeout_ms: Option<u64>,
}

/// Hard cap on retries a manifest can request, so a typo (`maxAttempts: 9999`)
/// can't wedge the queue re-running a persistently-failing function forever.
const MAX_JOB_RETRIES: u32 = 10;

#[derive(Debug, Deserialize, Default, Clone)]
struct CacheSpec {
    #[serde(rename = "ttlSeconds")]
    ttl_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct FetchSpec {
    #[serde(default, rename = "maxResponseBytes")]
    max_response_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct SecretsSpec {
    #[serde(default)]
    write: Option<bool>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct EmailSpec {
    #[serde(default)]
    send: Option<bool>,
}

/// `"org": { "read": true }` — opt in to the people directory.
///
/// One flag, not a read/write pair like `StorageSpec`: there is no write. An app
/// naming a person is a reader of the org's roster, and letting a bundle EDIT
/// the directory would put tenant membership behind an app's manifest.
#[derive(Debug, Deserialize, Default, Clone)]
struct OrgSpec {
    #[serde(default)]
    read: Option<bool>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct StorageSpec {
    #[serde(default)]
    read: Option<bool>,
    #[serde(default)]
    write: Option<bool>,
}

/// `webhook:` — makes a function reachable by an UNAUTHENTICATED POST from a
/// third party, with the platform verifying the sender before app code runs.
///
/// Declaring this is what opens the door; a function without it is not
/// reachable on the webhook route at all. And declaring it without a resolvable
/// secret is a 401, never an open endpoint: an unauthenticated route with no
/// signing secret lets anyone forge events against a workspace, which is the
/// same reasoning `webhooks::toast` documents for failing closed.
///
/// Verification is the PLATFORM's job rather than the function's, deliberately.
/// A function could verify — `x-*` headers reach app code and `req.body` is the
/// raw bytes — but then an unverified request has already reached app code, and
/// a forgotten check is silent. Here it cannot be forgotten.
#[derive(Debug, Deserialize, Default, Clone)]
struct WebhookSpec {
    /// Workspace-secret key holding the signing key(s).
    ///
    /// **Comma-separated for rotation.** Providers that support two live keys
    /// (Uber's `BASIC_HMAC` issues a pair) need both accepted at once, or a
    /// rotation drops every event signed with the key you have not adopted yet.
    /// Any match passes.
    #[serde(rename = "secretVar")]
    secret_var: Option<String>,
    /// Header carrying the signature, e.g. `x-uber-signature`.
    ///
    /// Named per function rather than inferred: providers disagree
    /// (`x-hub-signature-256`, `stripe-signature`, `x-slack-signature`), and
    /// guessing would make an unrecognised provider fail as "no signature"
    /// instead of "you did not tell me where to look".
    #[serde(rename = "signatureHeader")]
    signature_header: Option<String>,
    /// How the digest is encoded: `hex` (default) or `base64`.
    ///
    /// Uber sends lowercase hex; Toast sends base64. Both are HMAC-SHA256 over
    /// the raw body, so encoding is the only axis that varies here.
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct OltpSpec {
    /// Whether `ctx.oltp` is permitted. A pure GATE — it does NOT name the
    /// target. The writer is derived host-side from the app's own slug
    /// (`oxy_oltp::schema::app_writer_name`), so a manifest cannot point
    /// `ctx.oltp` at another app's schema.
    #[serde(default)]
    enabled: Option<bool>,
}

impl FunctionManifestEntry {
    /// The cache TTL if opted in with a positive value, else `None`.
    fn cache_ttl(&self) -> Option<Duration> {
        self.cache
            .as_ref()
            .and_then(|c| c.ttl_seconds)
            .filter(|&s| s > 0)
            // Cap the TTL so a typo can't cache for years; a day is plenty for
            // the "don't recompute per page-load" use case.
            .map(|s| Duration::from_secs(s.min(86_400)))
    }

    /// Database names this function may write to via `ctx.warehouse` (empty →
    /// none; writes fail-closed). The §11.3 destination allowlist.
    fn write_destinations(&self) -> Vec<String> {
        self.destinations.clone().unwrap_or_default()
    }

    /// Whether `ctx.secrets.set` is permitted (fail-closed default: false).
    fn secrets_write(&self) -> bool {
        self.secrets.as_ref().and_then(|s| s.write).unwrap_or(false)
    }

    /// Whether `ctx.org.people()` is permitted (fail-closed default: false).
    fn org_read(&self) -> bool {
        self.org.as_ref().and_then(|o| o.read).unwrap_or(false)
    }

    /// Whether `ctx.email.send` is permitted (fail-closed default: false).
    fn email_send(&self) -> bool {
        self.email.as_ref().and_then(|e| e.send).unwrap_or(false)
    }

    /// Whether `ctx.storage` reads (getDownloadUrl/get/head/list, and copy's
    /// source) are permitted (fail-closed default: false). The per-op mapping is
    /// `host::check_storage_capability`.
    fn storage_read(&self) -> bool {
        self.storage.as_ref().and_then(|s| s.read).unwrap_or(false)
    }

    /// Whether `ctx.storage` writes (getUploadUrl/put/delete, and copy's
    /// destination) are permitted (fail-closed default: false).
    fn storage_write(&self) -> bool {
        self.storage.as_ref().and_then(|s| s.write).unwrap_or(false)
    }

    /// Whether `ctx.oltp` is permitted for this function (fail-closed default:
    /// false). Only a GATE — the writer it targets is derived from the app's own
    /// slug, never from the manifest, so this cannot name another app's schema.
    fn oltp_enabled(&self) -> bool {
        self.oltp.as_ref().and_then(|o| o.enabled).unwrap_or(false)
    }

    /// Build a `RetryPolicy` for background runs from the manifest's `retries`
    /// block. `None` when retries aren't requested (no block, or `maxAttempts`
    /// ≤ 1) — the job then runs exactly once.
    fn retry_policy(&self) -> Option<agentic_core::delegation::RetryPolicy> {
        use agentic_core::delegation::{BackoffStrategy, RetryPolicy};
        let spec = self.retries.as_ref()?;
        let max_attempts = spec.max_attempts?;
        if max_attempts <= 1 {
            return None;
        }
        let max_retries = (max_attempts - 1).min(MAX_JOB_RETRIES);
        // Exponential backoff (the queue doubles per attempt) clamped to a sane
        // window; a bad manifest can't set a zero or inverted delay.
        let initial = spec.min_timeout_ms.unwrap_or(1_000).max(1);
        let max_delay = spec.max_timeout_ms.unwrap_or(30_000).max(initial);
        Some(RetryPolicy {
            max_retries,
            backoff: BackoffStrategy::Exponential {
                initial_delay_ms: initial,
                max_delay_ms: max_delay,
            },
            retry_on: Vec::new(),
        })
    }
}

/// Resolve the durable-queue `TaskPolicy` for a background function job from its
/// raw manifest JSON (the `app_functions.manifest_json` value). `None` when the
/// function requests no retries. Single construction site for both the scheduled
/// fire (serialized into the schedule's `variables` at publish) and the manual
/// trigger (built in-host) — so the two paths can't drift.
pub(crate) fn function_task_policy(
    manifest: &serde_json::Value,
) -> Option<agentic_core::delegation::TaskPolicy> {
    let entry: FunctionManifestEntry = serde_json::from_value(manifest.clone()).ok()?;
    let retry = entry.retry_policy()?;
    Some(agentic_core::delegation::TaskPolicy {
        retry: Some(retry),
        fallback_targets: Vec::new(),
    })
}

/// The webhook contract a function declares, from its raw `manifest_json`.
///
/// `None` means the function did not declare one, which the route treats as
/// "not a webhook endpoint" — a 404, not a 401. Telling an anonymous caller
/// apart "wrong signature" from "no such endpoint" is a small thing, but the
/// route is unauthenticated, so it is the only thing separating a probe from a
/// map of every function an app has.
pub(crate) fn function_webhook_contract(
    manifest: &serde_json::Value,
) -> Option<(String, String, String)> {
    let entry: FunctionManifestEntry = serde_json::from_value(manifest.clone()).ok()?;
    let w = entry.webhook?;
    Some((
        w.secret_var?,
        w.signature_header?,
        w.encoding.unwrap_or_else(|| "hex".to_string()),
    ))
}

/// Why a one-off function job was enqueued.
///
/// An enum rather than a `&str` because the label is not free text: it is
/// replayed by `app_function_executor` as the invocation `mode`, and any value
/// that executor does not recognise silently becomes `schedule` — a function
/// branching on `mode` would then be told a webhook was a cron fire. Making the
/// set closed here means a new trigger cannot be added without visiting that
/// match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FunctionJobTrigger {
    /// Admin console **Run now**, or the API job trigger.
    Manual,
    /// A verified inbound webhook from a third party.
    Webhook,
}

impl FunctionJobTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Webhook => "webhook",
        }
    }
}

/// Trigger a **one-off background run** of a custom-app function as a job — the
/// manual/API "run now" that isn't tied to a cron schedule. Validates the app +
/// function exist in the active build, resolves the function's retry policy from
/// its manifest, then seeds a run + enqueues a durable `app_function` task on the
/// global fleet (`agentic_pipeline::scheduler::enqueue_app_function_job`).
/// Returns the `run_id` to watch in the orchestrator dashboard. The isolate does
/// NOT run inline — a worker picks the task up, exactly like a scheduled fire, so
/// this survives instance death and inherits the queue's status/retry.
///
/// **Any function in the build can be triggered this way — including a
/// route-only one.** It runs on the same **background/system path** a scheduled
/// fire uses: the **org-owner identity** and whatever `input` this trigger was
/// given as the request body — empty only when the caller supplied none — not an
/// HTTP request. This is intentional (the manual/API trigger is universal — a
/// job need not declare a `schedule`), but a function written to require a
/// request body or caller will observe an empty body. Author functions meant to
/// run as jobs to tolerate a system invocation (read from `ctx`/secrets, not the
/// request body).
pub(crate) async fn trigger_function_job(
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    function_name: &str,
    input: Option<serde_json::Value>,
    // How this run was triggered. Stamped on the run's `metadata.trigger` and
    // replayed by `app_function_executor` as the invocation `mode`, so the
    // history distinguishes an admin's **Run now** (`"manual"`) from a provider
    // webhook (`"webhook"`). A label the executor does not know falls back to
    // `schedule`, so add one there in the same change.
    trigger: FunctionJobTrigger,
) -> Result<String, String> {
    let app = entity::apps::Entity::find_by_id(app_id)
        .one(db)
        .await
        .map_err(|e| format!("app lookup failed: {e}"))?
        .ok_or_else(|| format!("app {app_id} not found"))?;
    let build_id = app
        .published_build_id
        .or(app.draft_build_id)
        .ok_or_else(|| "app has no build".to_string())?;
    // Validate the function exists in the active build before enqueuing, so a
    // typo produces a 400 now rather than a failed run later.
    let func_row = AppFunctions::find()
        .filter(app_functions::Column::BuildId.eq(build_id))
        .filter(app_functions::Column::Name.eq(function_name))
        .one(db)
        .await
        .map_err(|e| format!("app_functions lookup failed: {e}"))?
        .ok_or_else(|| format!("function '{function_name}' not found in the app's active build"))?;
    let policy = func_row
        .manifest_json
        .as_ref()
        .and_then(function_task_policy);
    agentic_pipeline::scheduler::enqueue_app_function_job(
        db,
        &app_id.to_string(),
        function_name,
        app.project_id,
        policy,
        trigger.as_str(),
        input,
        oxy_telemetry::propagation::current_traceparent(),
    )
    .await
    .map_err(|e| format!("failed to enqueue function job: {e:?}"))
}

/// Per-mode default `timeoutSeconds` (design doc §11.11). Route invocations
/// default to 10s; the manifest's `timeoutSeconds`, if set, is a ceiling
/// capped at 300 regardless of mode.
const ROUTE_DEFAULT_TIMEOUT_SECS: u64 = 10;
const TIMEOUT_CEILING_SECS: u64 = 300;

/// Platform ceiling for a per-function `ctx.fetch` response cap. Ops-tunable
/// so a tenant with unusually large source documents can be accommodated
/// without a release — mirroring `OXY_CUSTOMER_APPS_STORAGE_MAX_UPLOAD_BYTES`,
/// which the storage path has had all along. The two limits are the same class
/// and it was an accident that only one of them was configurable.
fn fetch_ceiling_bytes() -> u64 {
    const DEFAULT_CEILING: u64 = 100 * 1024 * 1024;
    std::env::var("OXY_CUSTOMER_APPS_FETCH_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_CEILING)
}

/// `None` → the host's built-in default (10 MiB). A declared value is clamped to
/// [`fetch_ceiling_bytes`], so a manifest cannot talk its way past the platform.
fn resolve_fetch_max_bytes(entry: &FunctionManifestEntry) -> Option<u64> {
    entry
        .fetch
        .as_ref()
        .and_then(|f| f.max_response_bytes)
        .filter(|n| *n > 0)
        .map(|n| n.min(fetch_ceiling_bytes()))
}

fn resolve_timeout(entry: &FunctionManifestEntry) -> Duration {
    let secs = entry
        .timeout_seconds
        .map(u64::from)
        .unwrap_or(ROUTE_DEFAULT_TIMEOUT_SECS)
        .min(TIMEOUT_CEILING_SECS);
    Duration::from_secs(secs)
}

// ── SSE helpers ──────────────────────────────────────────────────────────

fn sse_event(event: &str, data: &serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn sse_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .unwrap()
        .into_response()
}

/// Frame a successful function result body as the terminal `data` + `done` SSE
/// stream the SDK consumes. The body is emitted as a parsed JSON *value* so the
/// SDK's single `JSON.parse(data)` yields the object (not a double-encoded
/// string); a non-JSON body falls back to a JSON string. Shared by the fresh-run
/// and cache-hit paths so they frame identically.
fn success_sse_body(body_text: &str, http_status: u16) -> String {
    let body_value = serde_json::from_str::<serde_json::Value>(body_text)
        .unwrap_or_else(|_| serde_json::Value::String(body_text.to_string()));
    format!(
        "{}{}",
        sse_event("data", &body_value),
        // The status the function actually returned. This was hardcoded 200,
        // so a handler answering 403 reported success and the SDK resolved it.
        sse_event("done", &serde_json::json!({ "status": http_status }))
    )
}

// ── Idempotency, reaper & invocation-slot acquisition ──────────────────────

/// Seconds after which a still-`running` invocation row is treated as orphaned
/// (an instance crash / thread detach left it stuck) — well beyond the 300s max
/// function timeout + grace.
const REAP_AFTER_SECS: i64 = 900;
/// Reap at most once per this interval per process, so the sweep isn't an
/// UPDATE-scan on every single invocation (it clears orphans, which appear
/// rarely). The design doc's periodic global-driver sweep supersedes this.
const REAP_INTERVAL_SECS: u64 = 60;

/// Process-local throttle so `reap_stuck_invocations` runs at most every
/// [`REAP_INTERVAL_SECS`], not once per request.
fn should_reap_now() -> bool {
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    let now = Instant::now();
    match *last {
        Some(t) if now.duration_since(t).as_secs() < REAP_INTERVAL_SECS => false,
        _ => {
            *last = Some(now);
            true
        }
    }
}

/// Best-effort: transition orphaned `running` rows to `timeout` so the audit
/// trail is truthful and they stop looking in-flight to cancellation/idempotency.
/// Throttled — see [`should_reap_now`].
async fn reap_stuck_invocations(db: &sea_orm::DatabaseConnection) {
    if !should_reap_now() {
        return;
    }
    use sea_orm::sea_query::Expr;
    let cutoff: chrono::DateTime<chrono::FixedOffset> =
        (chrono::Utc::now() - chrono::Duration::seconds(REAP_AFTER_SECS)).into();
    if let Err(e) = app_function_invocations::Entity::update_many()
        .col_expr(
            app_function_invocations::Column::Status,
            Expr::value("timeout"),
        )
        .col_expr(
            app_function_invocations::Column::Error,
            Expr::value("reaped: still running past max lifetime (instance died or detached)"),
        )
        .col_expr(
            app_function_invocations::Column::FailureFingerprint,
            Expr::value(failure_signal::Failure::timeout_fingerprint()),
        )
        .filter(app_function_invocations::Column::Status.eq("running"))
        .filter(app_function_invocations::Column::CreatedAt.lt(cutoff))
        .exec(db)
        .await
    {
        error!("reaping stuck invocations failed: {e}");
    }
}

/// Stable i64 hash of a request body, stored on a keyed invocation so a key
/// reused with a *different* body is rejected rather than silently replayed.
fn request_body_hash(body: &[u8]) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    h.finish() as i64
}

/// Outcome of trying to acquire an invocation slot for a (possibly keyed) call.
enum Acquire {
    /// Run the isolate under this invocation-row id (freshly inserted or a
    /// reclaimed prior-failure row).
    Run(Uuid),
    /// Return this response without running (replay / 409 in-progress / 422
    /// body-mismatch). Boxed to keep the enum small.
    Return(Box<Response>),
}

fn json_error(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "error": error, "message": message })),
    )
        .into_response()
}

async fn find_keyed_invocation(
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    function_name: &str,
    user_id: Uuid,
    key: &str,
) -> Option<app_function_invocations::Model> {
    app_function_invocations::Entity::find()
        .filter(app_function_invocations::Column::AppId.eq(app_id))
        .filter(app_function_invocations::Column::FunctionName.eq(function_name))
        .filter(app_function_invocations::Column::UserId.eq(Some(user_id)))
        .filter(app_function_invocations::Column::IdempotencyKey.eq(key))
        .one(db)
        .await
        .unwrap_or(None)
}

#[allow(clippy::too_many_arguments)]
async fn insert_running_invocation(
    db: &sea_orm::DatabaseConnection,
    id: Uuid,
    app_id: Uuid,
    build_id: Uuid,
    function_name: &str,
    user_id: Uuid,
    key: Option<&str>,
    request_hash: Option<i64>,
) -> Result<(), sea_orm::DbErr> {
    app_function_invocations::ActiveModel {
        id: Set(id),
        app_id: Set(app_id),
        build_id: Set(build_id),
        function_name: Set(function_name.to_string()),
        mode: Set("route".to_string()),
        user_id: Set(Some(user_id)),
        status: Set("running".to_string()),
        duration_ms: Set(None),
        error: Set(None),
        cancel_requested_at: Set(None),
        created_at: Set(chrono::Utc::now().into()),
        idempotency_key: Set(key.map(str::to_string)),
        result_body: Set(None),
        result_status: Set(None),
        request_hash: Set(request_hash),
        failure_fingerprint: Set(None),
    }
    .insert(db)
    .await
    .map(|_| ())
}

/// Atomically re-claim a settled/stale keyed row for a retry: flip it back to
/// `running` **only if** it isn't already running, so two concurrent retries
/// can't both execute. Winning → run under this id; losing the race → 409.
async fn reclaim_or_conflict(
    db: &sea_orm::DatabaseConnection,
    id: Uuid,
    stale_cutoff: chrono::DateTime<chrono::FixedOffset>,
) -> Acquire {
    use sea_orm::sea_query::Expr;
    let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().into();
    let res = app_function_invocations::Entity::update_many()
        .col_expr(
            app_function_invocations::Column::Status,
            Expr::value("running"),
        )
        .col_expr(
            app_function_invocations::Column::Error,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            app_function_invocations::Column::ResultBody,
            Expr::value(Option::<String>::None),
        )
        // Cleared WITH the body. Unreachable today — the replay arm requires
        // `Some(body)` and both columns are always written together — but a row
        // carrying a status for a body that was deliberately cleared is
        // precisely the desync this column exists to prevent, and the pair
        // should be atomic wherever either moves.
        .col_expr(
            app_function_invocations::Column::ResultStatus,
            Expr::value(Option::<i16>::None),
        )
        // Cleared with `error`: a retry that is running again must not keep
        // counting as a failure towards a page (`failure_alert`) — and, with
        // `created_at` reset below, it would count as a recent one.
        .col_expr(
            app_function_invocations::Column::FailureFingerprint,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            app_function_invocations::Column::CreatedAt,
            Expr::value(now),
        )
        .filter(app_function_invocations::Column::Id.eq(id))
        // Reclaim a settled row (`status != 'running'`) OR an orphaned/stale
        // `running` row (created before the staleness cutoff — a hard crash left
        // it literally "running"). Race-safe: Postgres re-evaluates this WHERE
        // under the row lock, so a concurrent winner that resets
        // status='running' + created_at=now makes every loser match neither
        // clause → 0 rows → 409 (exactly one retry re-runs).
        .filter(
            sea_orm::Condition::any()
                .add(app_function_invocations::Column::Status.ne("running"))
                .add(app_function_invocations::Column::CreatedAt.lt(stale_cutoff)),
        )
        .exec(db)
        .await;
    match res {
        Ok(r) if r.rows_affected == 1 => Acquire::Run(id),
        _ => Acquire::Return(Box::new(json_error(
            StatusCode::CONFLICT,
            "IdempotencyKeyInProgress",
            "a request with this Idempotency-Key is already in progress",
        ))),
    }
}

/// Decide the fate of an existing keyed row: replay a matching success, reject a
/// body mismatch (422) or a concurrent in-flight (409), or reclaim a settled
/// failure / orphaned-running row so the caller can retry under the same key.
async fn resolve_keyed_row(
    db: &sea_orm::DatabaseConnection,
    row: app_function_invocations::Model,
    request_hash: i64,
) -> Acquire {
    if row.request_hash != Some(request_hash) {
        return Acquire::Return(Box::new(json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "IdempotencyKeyReused",
            "Idempotency-Key was reused with a different request body",
        )));
    }
    let cutoff: chrono::DateTime<chrono::FixedOffset> =
        (chrono::Utc::now() - chrono::Duration::seconds(REAP_AFTER_SECS)).into();
    let stale = row.created_at < cutoff;
    match (row.status.as_str(), row.result_body) {
        // A completed success replays its stored result AND the status it
        // returned with. This is why the status is a column: replaying the body
        // of a 409 under a hardcoded 200 would make a retry disagree with the
        // call it is replaying — the first press rejected, the second
        // apparently accepted, same invocation.
        //
        // `None` is a row written before the column existed. Read as 200, which
        // is what those rows were already being reported as; stamping them in a
        // backfill would assert something nobody recorded.
        ("success", Some(body)) => Acquire::Return(Box::new(sse_response(success_sse_body(
            &body,
            row.result_status.unwrap_or(200) as u16,
        )))),
        // Genuinely in flight → duplicate in progress.
        ("running", _) if !stale => Acquire::Return(Box::new(json_error(
            StatusCode::CONFLICT,
            "IdempotencyKeyInProgress",
            "a request with this Idempotency-Key is already in progress",
        ))),
        // Settled failure, or a stale/orphaned `running` row → retryable: the
        // same `cutoff` makes the reclaim UPDATE match the stale-running case
        // that `Status.ne("running")` alone would miss.
        _ => reclaim_or_conflict(db, row.id, cutoff).await,
    }
}

/// Acquire an invocation slot. Keyless → a fresh row (no dedup). Keyed →
/// exactly-once **with retry**: replay a prior success, reject a concurrent
/// duplicate / body mismatch, or reclaim a prior failure so it can run again.
#[allow(clippy::too_many_arguments)]
async fn acquire_invocation(
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    build_id: Uuid,
    function_name: &str,
    user_id: Uuid,
    key: Option<&str>,
    request_hash: i64,
) -> Acquire {
    let Some(key) = key else {
        let id = Uuid::new_v4();
        if let Err(e) =
            insert_running_invocation(db, id, app_id, build_id, function_name, user_id, None, None)
                .await
        {
            error!("failed to write app_function_invocations row: {e}");
        }
        return Acquire::Run(id);
    };
    // Fast path: an existing keyed row decides the outcome.
    if let Some(row) = find_keyed_invocation(db, app_id, function_name, user_id, key).await {
        return resolve_keyed_row(db, row, request_hash).await;
    }
    // No row yet: claim by inserting. A lost race (concurrent first call) hits
    // the unique index; resolve against the winner.
    let id = Uuid::new_v4();
    match insert_running_invocation(
        db,
        id,
        app_id,
        build_id,
        function_name,
        user_id,
        Some(key),
        Some(request_hash),
    )
    .await
    {
        Ok(()) => Acquire::Run(id),
        Err(_) => match find_keyed_invocation(db, app_id, function_name, user_id, key).await {
            Some(row) => resolve_keyed_row(db, row, request_hash).await,
            // Not a uniqueness conflict (some other DB error): run without a row.
            None => Acquire::Run(id),
        },
    }
}

// ── Handler ──────────────────────────────────────────────────────────────

/// Entry point called from `custom_apps_serve::serve_pretty` when `rest`
/// begins with `fn/`.
pub async fn handle_function_request(
    org_slug: &str,
    app_slug: &str,
    function_name: &str,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
    refresh: bool,
    query_exec: std::sync::Arc<dyn seam::FunctionQueryExecutor>,
    // Layer-1 preagg cache + renewal threshold, injected at the serve router.
    // Default (both `None`) means no rollup short-circuit, so `ctx.semantic`
    // compiles to warehouse SQL.
    preagg: crate::server::api::middlewares::workspace_context::PreaggCacheCtx,
) -> Response {
    // §11.10 — POST-only, before any gate/runtime work.
    if method != Method::POST {
        return json_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "functions are invoked with POST",
        );
    }

    let outcome = match authenticate_and_authorize(&headers, org_slug, app_slug).await {
        Ok(o) => o,
        Err(status) => return status.into_response(),
    };
    let app = outcome.app;

    let db = match establish_connection().await {
        Ok(db) => db,
        Err(e) => {
            error!("db connection failed: {e}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "DatabaseUnavailable",
                "could not reach the database; retry shortly",
            );
        }
    };

    let Some(build_id) = app.published_build_id.or(app.draft_build_id) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "AppNotPublished",
            "this app has no published or draft build; run `oxyc publish` first",
        );
    };

    let Some(func_row) = (match AppFunctions::find()
        .filter(app_functions::Column::BuildId.eq(build_id))
        .filter(app_functions::Column::Name.eq(function_name))
        .one(&db)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            error!("app_functions lookup failed: {e}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "FunctionLookupFailed",
                "could not read the function registry; retry shortly",
            );
        }
    }) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "FunctionNotFound",
            &format!(
                "no function named '{function_name}' in the live build; check the \
                 `functions` entry in oxy-app.json and re-publish"
            ),
        );
    };

    let manifest: FunctionManifestEntry = func_row
        .manifest_json
        .as_ref()
        .and_then(|j| serde_json::from_value(j.clone()).ok())
        .unwrap_or_default();
    // `route` defaults to active unless schedule/airwayStep are the only
    // declared surfaces — the SDK validator already enforced "at least one
    // surface active" at publish time, so a missing/true `route` here means
    // this row is route-invocable.
    if manifest.route == Some(false) {
        return json_error(
            StatusCode::NOT_FOUND,
            "FunctionRouteDisabled",
            &format!(
                "function '{function_name}' exists but declares `route: false` — it is \
                 only invocable as a schedule or Airway step"
            ),
        );
    }

    // Caller-supplied idempotency key (route mode): enables exactly-once, with
    // retry, for side-effectful writes. Bounded length; empty → treated as
    // absent. The body is hashed so a key reused with a different body is
    // rejected rather than silently replaying the first result. Replay / retry /
    // conflict are all decided by `acquire_invocation` below.
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 200)
        .map(str::to_string);
    let request_hash = request_body_hash(&body);

    // Opt-in result cache (manifest `cache.ttlSeconds`). A hit skips the isolate
    // run entirely; `?refresh` bypasses it. Side-effectful functions don't
    // declare a cache and never reach this. Keyed per (build, function, user,
    // body), so it can't leak across users or survive a redeploy.
    let cache_ttl = manifest.cache_ttl();
    if cache_ttl.is_some()
        && !refresh
        && let Some(cached) = result_cache::get(build_id, function_name, outcome.user_id, &body)
    {
        // 200, and that is now a fact rather than a default: only a 2xx result
        // is ever cached (see the `put` below), and the status is not stored
        // alongside it because a 201 or 204 body replayed as 200 is a smaller
        // wrong than a second column that can drift from the body it describes.
        return sse_response(success_sse_body(&cached, 200));
    }

    // §11.6 — rate limit per (user, app, function).
    let limit = manifest
        .rate_limit_per_minute
        .unwrap_or(DEFAULT_RATE_PER_MIN);
    if would_exceed_rate(outcome.user_id, app.id, function_name, limit) {
        // Return JSON, not an SSE frame: the client throws on any non-2xx
        // *before* it starts reading the event stream and JSON-parses the body,
        // so an `event: error\ndata: …` blob would surface as a garbled error.
        // Every other pre-stream error here is a bare status / JSON — the 429
        // was the lone SSE-framed outlier.
        return (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(
                serde_json::json!({ "error": "RateLimitExceeded", "message": "too many requests" }),
            ),
        )
            .into_response();
    }

    // Resolve the build's string `build_id` (build store key) from its uuid.
    let Some(build) = (match AppBuilds::find_by_id(build_id).one(&db).await {
        Ok(b) => b,
        Err(e) => {
            error!("app_builds lookup failed: {e}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BuildLookupFailed",
                "could not read the app build record; retry shortly",
            );
        }
    }) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "BuildNotFound",
            "the build this app points at no longer exists; re-publish the app",
        );
    };

    let artifact_rel = format!("functions/{function_name}.js");
    let artifact =
        match custom_apps_build_store::get_object(app.id, &build.build_id, &artifact_rel).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return json_error(
                    StatusCode::NOT_FOUND,
                    "FunctionArtifactMissing",
                    &format!(
                        "'{artifact_rel}' is absent from the build store for this build; \
                         re-publish the app"
                    ),
                );
            }
            Err(e) => {
                error!("build store fetch failed: {e}");
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BuildStoreUnavailable",
                    "could not fetch the function artifact from the build store; retry shortly",
                );
            }
        };
    let artifact_js = String::from_utf8_lossy(&artifact).into_owned();

    // Lazy crash reaper (throttled): transition rows left `running` past any
    // function's max lifetime (orphaned by an instance death / thread detach)
    // so the audit trail is truthful and they stop looking in-flight.
    reap_stuck_invocations(&db).await;

    // §11.12 / §11.14 — acquire the invocation row (written up front so
    // cancellation has something to poll). For a keyed call this replays a prior
    // success, rejects a concurrent duplicate (409) or body mismatch (422), or
    // reclaims a prior failure so it can retry — see `acquire_invocation`.
    let invocation_id = match acquire_invocation(
        &db,
        app.id,
        build_id,
        function_name,
        outcome.user_id,
        idempotency_key.as_deref(),
        request_hash,
    )
    .await
    {
        Acquire::Run(id) => id,
        Acquire::Return(resp) => return *resp,
    };

    let timeout = resolve_timeout(&manifest);
    let started = Instant::now();

    // Shared log buffer: the isolate appends `console.*`/`ctx.log` during the
    // run; we drain it after and send it back with the response so a developer
    // never has to open the oxy server logs to see what a function printed.
    let logs: std::sync::Arc<Mutex<Vec<LogLine>>> = std::sync::Arc::new(Mutex::new(Vec::new()));

    #[cfg(feature = "custom-app-functions")]
    let (status_str, error_msg, body_text, http_status) = run_with_runtime(RunArgs {
        db: &db,
        query_exec,
        app: &app,
        artifact_js,
        body: body.to_vec(),
        method: method.as_str().to_string(),
        headers: sanitize_request_headers(&headers),
        user_id: outcome.user_id,
        user_email: outcome.user_email.clone(),
        user_name: Some(outcome.user_name.clone()),
        user_picture: outcome.user_picture.clone(),
        identity_kind: runtime::CtxIdentityKind::User,
        org_id: app.org_id,
        invocation_id,
        function_name,
        mode: "route",
        build_id,
        // The one path with a real request behind it. Read from the header the
        // outer middleware stamped rather than minted here, so the id on this
        // span is the same one the caller got back in the response.
        request_id: request_id_from_headers(&headers),
        timeout,
        write_destinations: manifest.write_destinations(),
        caps: host::FunctionCapabilities {
            secrets_write: manifest.secrets_write(),
            email_send: manifest.email_send(),
            org_read: manifest.org_read(),
            storage_read: manifest.storage_read(),
            storage_write: manifest.storage_write(),
            // DERIVED from the invoking app's slug, gated by the manifest — never
            // named by the manifest, so one app cannot reach another's schema.
            // A slug that can't back a schema is a distinct fail-closed reason so
            // the host diagnoses it as such, not as "capability missing".
            oltp: host::OltpCapability::resolve(manifest.oltp_enabled(), &app.slug),
            fetch_max_bytes: resolve_fetch_max_bytes(&manifest),
            storage_retention: super::custom_apps_manifest::retention_policy_from_build_manifest(
                build.manifest_json.as_ref(),
                app.id,
            ),
        },
        logs: logs.clone(),
        // Route path: cancellation is the `cancel_requested_at` DB flag (set on
        // client-gone / dashboard cancel), so a never-fired token suffices here.
        cancel: tokio_util::sync::CancellationToken::new(),
        preagg,
    })
    .await;

    #[cfg(not(feature = "custom-app-functions"))]
    let (status_str, error_msg, body_text, http_status) = {
        let _ = (&artifact_js, &body, timeout, &query_exec);
        (
            "error",
            Some("custom-app-functions feature not enabled".to_string()),
            String::new(),
            // No response, so no status — matching the error arms in
            // `run_with_runtime`. The error framing carries this outcome, so
            // the value is never read.
            0u16,
        )
    };

    let duration_ms = started.elapsed().as_millis() as i64;
    let mut update: app_function_invocations::ActiveModel = app_function_invocations::ActiveModel {
        id: Set(invocation_id),
        ..Default::default()
    };
    update.status = Set(status_str.to_string());
    update.duration_ms = Set(Some(duration_ms));
    update.error = Set(error_msg.clone());
    let failure = failure_signal::Failure::of(status_str, http_status, error_msg.as_deref());
    update.failure_fingerprint = Set(failure.as_ref().map(|f| f.fingerprint.clone()));
    // Persist the result for idempotent replay — only when a key was supplied,
    // so the audit table isn't bloated with every function's output.
    if idempotency_key.is_some() && status_str == "success" {
        update.result_body = Set(Some(body_text.clone()));
        update.result_status = Set(Some(http_status as i16));
    }
    if let Err(e) = update.update(&db).await {
        error!("failed to update app_function_invocations row: {e}");
    } else if let Some(failure) = &failure {
        failure_page::observe(&db, &app, function_name, invocation_id, failure).await;
    }
    let request_id = request_id_from_headers(&headers);
    super::custom_apps_telemetry::record_function(super::custom_apps_telemetry::FunctionEvent {
        org_id: app.org_id,
        app_id: app.id,
        build_id,
        request_id,
        user_id: outcome.user_id,
        function_name,
        mode: "route",
        status_label: status_str,
        http_status,
        duration_ms: duration_ms.clamp(0, u32::MAX as i64) as u32,
        error: error_msg.as_deref(),
    });

    // Cache the successful result for functions that opted into `cache`.
    //
    // `status_str == "success"` only means the isolate RETURNED — a handler
    // answering 403 reaches here too. Caching that would replay a rejection to
    // everyone for the TTL, and the cache has no status to replay it with, so
    // the hit would report 200 for a body that says "forbidden". A non-2xx is
    // not a result worth remembering anyway: it is a statement about this
    // caller and this request.
    if status_str == "success"
        && (200..300).contains(&http_status)
        && let Some(ttl) = cache_ttl
    {
        result_cache::put(
            build_id,
            function_name,
            outcome.user_id,
            &body,
            body_text.clone(),
            ttl,
        );
    }

    // Drain captured logs and send them as `log` frames ahead of the terminal
    // frame (collected during the run and flushed together with the response —
    // batched, not live-tailed), so the app can show what the function printed —
    // on success and, crucially, on error (the console output before the throw).
    let captured: Vec<LogLine> = logs
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default();
    // Persist them too. Until this existed, a route call's logs lived ONLY in
    // the SSE frames below — so a browser that had already navigated away took
    // them with it, while a scheduled run's logs survived as
    // `agentic_run_events`. The two modes disagreed about whether a log line
    // outlives its run; now neither does.
    super::custom_apps_telemetry::record_function_logs(
        app.org_id,
        app.id,
        build_id,
        invocation_id,
        request_id,
        function_name,
        "route",
        &captured
            .iter()
            .map(|l| (l.level.clone(), l.message.clone()))
            .collect::<Vec<_>>(),
    );
    let mut sse_body = String::new();
    for line in &captured {
        sse_body.push_str(&sse_event(
            "log",
            &serde_json::json!({ "level": line.level, "message": line.message }),
        ));
    }
    sse_body.push_str(&match error_msg {
        Some(msg) => sse_event(
            "error",
            &serde_json::json!({ "error": status_str, "message": msg }),
        ),
        None => success_sse_body(&body_text, http_status),
    });
    sse_response(sse_body)
}

/// Outcome shared by both feature arms: `(status_str, error, body, http_status)`.
///
/// FOUR elements, and the count is load-bearing across a `cfg`. The
/// feature-off arm builds this tuple by hand, so widening it here without
/// widening there compiles under the default features and fails only under
/// `--no-default-features` — which is a supported configuration
/// (`oxy-api-github` takes `oxy-app` that way so a V8-less build really drops
/// V8), and which a `--workspace` build unifies back on. That is exactly how
/// the fourth element shipped broken the first time.
///
/// The fourth element is the status the FUNCTION returned, which used to be
/// dropped here — `run_with_runtime` matched on `Ok(resp)` and kept only
/// `resp.body`, and the SSE framing hardcoded 200. So every rejection a
/// function expressed arrived as a success and clients had to infer it from the
/// body's shape.
type RunOutcome = (&'static str, Option<String>, String, u16);

/// Max `function_log` events emitted per run. Bounds the run's event log (the
/// n8n/Windmill/Hatchet DB-bloat lesson — treat the event store as a buffer, not
/// an unbounded sink). Overflow is reported, never silently dropped.
#[cfg(feature = "custom-app-functions")]
const MAX_LOG_EVENTS: usize = 500;

/// A sink for run-lifecycle events (`function_log`, `app_function_completed`)
/// emitted at the END of a background function run (logs are drained in one batch
/// once the isolate returns, not live-tailed). In the queued path this is
/// `AppFunctionTaskExecutor`'s worker event channel — events are persisted to
/// `agentic_run_events` via the coordinator, so a scheduled/manual run's logs
/// survive the isolate and render in the orchestrator dashboard. `None` for
/// callers that don't observe events.
#[cfg(feature = "custom-app-functions")]
pub(crate) type RunEventSink = tokio::sync::mpsc::Sender<(String, serde_json::Value)>;

/// Run a custom-app function on the SYSTEM path (a schedule fire or a manual
/// job trigger) — bypassing auth / rate-limit / idempotency / cache. Resolves the
/// app's active build + the function's artifact + manifest, runs the isolate under
/// the **org owner's** identity (apps have no owner field; the org owner is the
/// natural actor — the same data + secret access), records an
/// `app_function_invocations` row with the given `mode` (`"schedule"` for a cron
/// fire, `"manual"` for a run-now / API trigger, `"webhook"` for a verified
/// inbound provider event — so the invocation history agrees with the run's
/// `metadata.trigger`), `user_id=None`, and — when `events`
/// is set — drains the isolate's log buffer into `function_log` events so the
/// run's output is persisted and observable. Returns the response body on success.
#[cfg(feature = "custom-app-functions")]
pub(crate) async fn run_scheduled_function(
    db: &sea_orm::DatabaseConnection,
    app_id: Uuid,
    function_name: &str,
    mode: &str,
    // Request body handed to the isolate as `req` — the function's input params
    // (JSON), same shape a route invocation receives. Empty for a bare cron fire.
    input: Vec<u8>,
    cancel: tokio_util::sync::CancellationToken,
    events: Option<RunEventSink>,
    query_exec: std::sync::Arc<dyn seam::FunctionQueryExecutor>,
    // Layer-1 preagg cache from the node draining the queue, so a scheduled
    // `ctx.semantic` resolves rollups like its HTTP-invoked twin.
    preagg: crate::server::api::middlewares::workspace_context::PreaggCacheCtx,
) -> Result<String, String> {
    let app = entity::apps::Entity::find_by_id(app_id)
        .one(db)
        .await
        .map_err(|e| format!("app lookup failed: {e}"))?
        .ok_or_else(|| format!("app {app_id} not found"))?;

    let build_id = app
        .published_build_id
        .or(app.draft_build_id)
        .ok_or_else(|| "app has no build".to_string())?;

    let func_row = AppFunctions::find()
        .filter(app_functions::Column::BuildId.eq(build_id))
        .filter(app_functions::Column::Name.eq(function_name))
        .one(db)
        .await
        .map_err(|e| format!("app_functions lookup failed: {e}"))?
        .ok_or_else(|| format!("function '{function_name}' not found in build"))?;

    let manifest: FunctionManifestEntry = func_row
        .manifest_json
        .as_ref()
        .and_then(|j| serde_json::from_value(j.clone()).ok())
        .unwrap_or_default();

    let build = AppBuilds::find_by_id(build_id)
        .one(db)
        .await
        .map_err(|e| format!("app_builds lookup failed: {e}"))?
        .ok_or_else(|| "build not found".to_string())?;

    let artifact_rel = format!("functions/{function_name}.js");
    let artifact = custom_apps_build_store::get_object(app.id, &build.build_id, &artifact_rel)
        .await
        .map_err(|e| format!("build store fetch failed: {e}"))?
        .ok_or_else(|| format!("artifact '{artifact_rel}' not found"))?;
    let artifact_js = String::from_utf8_lossy(&artifact).into_owned();

    // Identity/actor: the org owner (apps have no owner; a scheduled run has no
    // invoking user). Threads into build_project_context (Airhouse role),
    // `ctx.user.id`, and the `set_app_secret` created_by (non-null FK).
    let owner = entity::org_members::Entity::find()
        .filter(entity::org_members::Column::OrgId.eq(app.org_id))
        .filter(entity::org_members::Column::Role.eq(entity::org_members::OrgRole::Owner))
        .one(db)
        .await
        .map_err(|e| format!("org owner lookup failed: {e}"))?
        .ok_or_else(|| format!("no owner for org {}", app.org_id))?;

    let invocation_id = Uuid::new_v4();
    if let Err(e) = (app_function_invocations::ActiveModel {
        id: Set(invocation_id),
        app_id: Set(app.id),
        build_id: Set(build_id),
        function_name: Set(function_name.to_string()),
        mode: Set(mode.to_string()),
        user_id: Set(None),
        status: Set("running".to_string()),
        duration_ms: Set(None),
        error: Set(None),
        cancel_requested_at: Set(None),
        created_at: Set(chrono::Utc::now().into()),
        idempotency_key: Set(None),
        result_body: Set(None),
        result_status: Set(None),
        request_hash: Set(None),
        failure_fingerprint: Set(None),
    })
    .insert(db)
    .await
    {
        error!("failed to insert scheduled invocation row: {e}");
    }

    let timeout = resolve_timeout(&manifest);
    let started = Instant::now();
    let logs: std::sync::Arc<Mutex<Vec<LogLine>>> = std::sync::Arc::new(Mutex::new(Vec::new()));

    let (status_str, error_msg, body_text, http_status) = run_with_runtime(RunArgs {
        db,
        query_exec,
        app: &app,
        artifact_js,
        body: input,
        // No real request behind a schedule; mirrors `FnRequest::from_body`.
        method: "POST".to_string(),
        headers: std::collections::BTreeMap::new(),
        user_id: owner.user_id,
        user_email: Some(format!("schedule+{function_name}@system.oxy")),
        // No caller to attribute this run to — and note this path serves the
        // console's manual `Run now` as well as a cron tick, so a person may well
        // have clicked. The `user_id` above is the org owner's (the invocation row
        // needs a non-null FK and `ctx.secrets` a `created_by`), but every caller
        // field stays empty so a function can't mistake either trigger for the
        // owner using the app.
        user_name: None,
        user_picture: None,
        identity_kind: runtime::CtxIdentityKind::System,
        org_id: app.org_id,
        invocation_id,
        function_name,
        mode,
        build_id,
        // No HTTP request behind a cron tick, an Airway step or a manual job
        // run. See the field docs on `RunArgs` for why this stays `None`.
        request_id: None,
        timeout,
        write_destinations: manifest.write_destinations(),
        caps: host::FunctionCapabilities {
            secrets_write: manifest.secrets_write(),
            email_send: manifest.email_send(),
            org_read: manifest.org_read(),
            storage_read: manifest.storage_read(),
            storage_write: manifest.storage_write(),
            // DERIVED from the invoking app's slug, gated by the manifest — never
            // named by the manifest, so one app cannot reach another's schema.
            // A slug that can't back a schema is a distinct fail-closed reason so
            // the host diagnoses it as such, not as "capability missing".
            oltp: host::OltpCapability::resolve(manifest.oltp_enabled(), &app.slug),
            fetch_max_bytes: resolve_fetch_max_bytes(&manifest),
            storage_retention: super::custom_apps_manifest::retention_policy_from_build_manifest(
                build.manifest_json.as_ref(),
                app.id,
            ),
        },
        logs: logs.clone(),
        cancel,
        preagg,
    })
    .await;

    let duration_ms = started.elapsed().as_millis() as i64;
    let mut update = app_function_invocations::ActiveModel {
        id: Set(invocation_id),
        ..Default::default()
    };
    update.status = Set(status_str.to_string());
    update.duration_ms = Set(Some(duration_ms));
    update.error = Set(error_msg.clone());
    let failure = failure_signal::Failure::of(status_str, http_status, error_msg.as_deref());
    update.failure_fingerprint = Set(failure.as_ref().map(|f| f.fingerprint.clone()));
    if status_str == "success" {
        update.result_body = Set(Some(body_text.clone()));
        update.result_status = Set(Some(http_status as i16));
    }
    if let Err(e) = update.update(db).await {
        error!("failed to update scheduled invocation row: {e}");
    } else if let Some(failure) = &failure {
        failure_page::observe(db, &app, function_name, invocation_id, failure).await;
    }
    super::custom_apps_telemetry::record_function(super::custom_apps_telemetry::FunctionEvent {
        org_id: app.org_id,
        app_id: app.id,
        build_id,
        // No HTTP request behind a cron tick or an Airway step.
        request_id: None,
        user_id: owner.user_id,
        function_name,
        mode,
        status_label: status_str,
        http_status,
        duration_ms: duration_ms.clamp(0, u32::MAX as i64) as u32,
        error: error_msg.as_deref(),
    });

    // Persist the isolate's log output. The buffer was filled during the run but
    // is otherwise dropped on the system path (route mode drains it to SSE; the
    // schedule/manual path had no observer). Drain it — as one batch, AFTER the
    // isolate returns (not live-tailed mid-run) — into `function_log` events the
    // coordinator persists to `agentic_run_events`. The run's whole output then
    // lands together and surfaces on the next dashboard poll / SSE catch-up.
    //
    // Drained UNCONDITIONALLY since 2026-09. It used to happen only inside the
    // `if let Some(tx)` below, so a run with no event sink — an Airway step, a
    // fleet worker without the coordinator channel — dropped its entire log
    // output on the floor with nothing recording that it had. The ClickHouse
    // sink wants those lines whether or not anyone is watching the run live.
    //
    // Take the lines out under the lock, then release it before awaiting the
    // sends (never hold a std Mutex across `.await`).
    let drained: Vec<LogLine> = {
        let mut guard = logs.lock().unwrap_or_else(|poison| poison.into_inner());
        std::mem::take(&mut *guard)
    };
    super::custom_apps_telemetry::record_function_logs(
        app.org_id,
        app.id,
        build_id,
        invocation_id,
        None,
        function_name,
        mode,
        &drained
            .iter()
            .map(|l| (l.level.clone(), l.message.clone()))
            .collect::<Vec<_>>(),
    );
    if let Some(tx) = events.as_ref() {
        let total = drained.len();
        for (idx, line) in drained.into_iter().take(MAX_LOG_EVENTS).enumerate() {
            let _ = tx
                .send((
                    "function_log".to_string(),
                    serde_json::json!({
                        "level": line.level,
                        "message": line.message,
                        "idx": idx,
                    }),
                ))
                .await;
        }
        if total > MAX_LOG_EVENTS {
            let _ = tx
                .send((
                    "function_log".to_string(),
                    serde_json::json!({
                        "level": "warn",
                        "message": format!(
                            "… {} more log line(s) truncated (cap {MAX_LOG_EVENTS})",
                            total - MAX_LOG_EVENTS
                        ),
                        "idx": MAX_LOG_EVENTS,
                    }),
                ))
                .await;
        }
        let _ = tx
            .send((
                "app_function_completed".to_string(),
                serde_json::json!({
                    "status": status_str,
                    "duration_ms": duration_ms,
                    "log_lines": total,
                }),
            ))
            .await;
    }

    if status_str == "success" {
        Ok(body_text)
    } else {
        Err(error_msg.unwrap_or_else(|| "function failed".to_string()))
    }
}

/// A captured `ctx.log` / `console.*` line from a function run, surfaced back to
/// the app (as SSE `log` frames) so a developer sees output without opening the
/// oxy server logs. Defined here (un-gated) so the handler can hold the buffer
/// regardless of the `custom-app-functions` feature.
#[derive(Clone, serde::Serialize)]
pub struct LogLine {
    pub level: String,
    pub message: String,
}

/// Whether to look up the invoking user's workspace role before building the
/// project context.
///
/// Only `airhouse_managed` consults that role, and only to decide whether the
/// credential the broker mints may write. So this answers one question: could
/// this invocation possibly need write authority?
///
/// Three ways the answer is no, and each saves two sequential DB round-trips on
/// the pre-isolate path a caller is waiting on:
///
/// * **A system run.** Schedule, Airway step and manual job runs carry the ORG
///   OWNER's user id because the invocation row needs a non-null FK, not
///   because an owner asked for anything. Resolving a role from it would hand
///   every cron tick an Admin credential.
/// * **A workspace with no org.** There is no membership to resolve.
/// * **A function that declared no write destinations.** §11.3 already denies
///   `ctx.warehouse.{exec,insert,upsert}` and `ctx.tx` for it, checked before
///   any connector is built — so a role would buy nothing. It also keeps the
///   least-privilege Reader credential as a second, independent layer over the
///   read surfaces for read-only apps: `ctx.warehouse.query` is deliberately
///   NOT behind the write allowlist, so for those apps this is the layer.
#[cfg(feature = "custom-app-functions")]
fn should_resolve_role(
    identity_kind: runtime::CtxIdentityKind,
    has_org: bool,
    no_write_destinations: bool,
) -> bool {
    identity_kind == runtime::CtxIdentityKind::User && has_org && !no_write_destinations
}

#[cfg(all(test, feature = "custom-app-functions"))]
mod role_resolution_tests {
    // Fully qualified, not `use super::…`: the custom-apps boundary test
    // resolves a `use` against the FILE's module path and does not model nested
    // `mod` blocks, so `super::` from here reads as `crate::server::api::…` —
    // one level above this surface — and reports a violation that is not one.
    use crate::server::api::custom_apps_functions::runtime::CtxIdentityKind;
    use crate::server::api::custom_apps_functions::should_resolve_role;

    #[test]
    fn a_user_invoking_a_write_capable_function_resolves_a_role() {
        assert!(should_resolve_role(CtxIdentityKind::User, true, false));
    }

    #[test]
    fn a_system_run_never_does() {
        // The policy this pins: a schedule / Airway step / manual job carries
        // the org owner's id for FK reasons only, so resolving from it would
        // give every timer an Admin airhouse credential. This is the part of
        // the change a future refactor is most likely to "simplify" away.
        assert!(!should_resolve_role(CtxIdentityKind::System, true, false));
    }

    #[test]
    fn a_read_only_function_does_not_pay_for_a_role_it_cannot_use() {
        // No write destinations means §11.3 denies every write surface anyway.
        assert!(!should_resolve_role(CtxIdentityKind::User, true, true));
    }

    #[test]
    fn a_workspace_with_no_org_has_no_membership_to_resolve() {
        assert!(!should_resolve_role(CtxIdentityKind::User, false, false));
    }
}

#[cfg(feature = "custom-app-functions")]
struct RunArgs<'a> {
    db: &'a sea_orm::DatabaseConnection,
    /// Runs `ctx.query`/`ctx.queryStream`; injected at the composition root so
    /// the runtime depends on the trait, not on `projects::query`.
    query_exec: std::sync::Arc<dyn seam::FunctionQueryExecutor>,
    app: &'a entity::apps::Model,
    artifact_js: String,
    body: Vec<u8>,
    /// HTTP method of the triggering request, and the headers the function is
    /// allowed to see (see `sanitize_request_headers`). The scheduled/Airway
    /// paths synthesise `POST` with no headers — there is no real request.
    method: String,
    headers: std::collections::BTreeMap<String, String>,
    user_id: Uuid,
    user_email: Option<String>,
    /// Display identity for `ctx.user.name` / `ctx.user.picture`. `None` on the
    /// system paths (schedule / Airway / manual job run), where the `user_id` is
    /// only the org owner's FK and there is no caller to attribute the run to —
    /// reporting the owner's name there would let a function pin a background run
    /// on a person. Note a manual **Run now** does have a human behind it; what
    /// it lacks is any way to carry them through the task queue.
    user_name: Option<String>,
    user_picture: Option<String>,
    /// Whether a human or the platform triggered this run; surfaced as
    /// `ctx.user.kind` and the gate on every human-only identity field.
    identity_kind: runtime::CtxIdentityKind,
    org_id: Uuid,
    invocation_id: Uuid,
    /// Identity of the run, for the invocation span only — the isolate learns
    /// its own name from the manifest, not from here. `mode` matches
    /// `app_function_invocations.mode`: `"route"` | `"schedule"` | `"airway"`.
    function_name: &'a str,
    mode: &'a str,
    build_id: Uuid,
    /// The `x-oxy-request-id` of the HTTP request behind this run, when there
    /// was one. `None` on the schedule / Airway paths, where no request exists.
    /// Deliberately absent rather than synthesized: a fabricated id would make a
    /// join silently group unrelated background runs into one "request".
    request_id: Option<Uuid>,
    timeout: Duration,
    /// §11.3 allowlist — databases `ctx.warehouse.*` may write to (empty = none).
    write_destinations: Vec<String>,
    /// Fail-closed capability gates (`ctx.secrets.set`, `ctx.email.send`,
    /// `ctx.storage` read/write) plus the app's storage retention policy.
    caps: host::FunctionCapabilities,
    /// Shared log buffer: the isolate appends `console.*`/`ctx.log`; the handler
    /// drains it after the run and sends it back as `log` frames (batched with
    /// the response, not live-tailed).
    logs: std::sync::Arc<Mutex<Vec<LogLine>>>,
    /// In-process cancellation from the caller. Merged with the
    /// `cancel_requested_at` poll by the watchdog. The scheduled path passes the
    /// run task's token so a fleet/operator cancel stops the isolate; the route
    /// path passes a never-fired token (its cancellation is the DB flag alone).
    cancel: tokio_util::sync::CancellationToken,
    /// Layer-1 preagg cache + renewal threshold for `ctx.semantic`. Default on
    /// the scheduled path (see `run_scheduled_function`).
    preagg: crate::server::api::middlewares::workspace_context::PreaggCacheCtx,
}

/// The span the isolate thread runs under — see the call site in
/// `run_with_runtime_inner` for why it has no `tracing` parent, and
/// `oxy_telemetry::propagation::adopt_current_parent` for how it still lands in
/// the invocation's trace.
#[cfg(feature = "custom-app-functions")]
fn isolate_span(
    app_id: Uuid,
    org_id: Uuid,
    build_id: Uuid,
    invocation_id: Uuid,
    function_name: &str,
    mode: &str,
) -> tracing::Span {
    let span = tracing::info_span!(
        parent: None,
        "custom_app_function.isolate",
        app_id = %app_id,
        // Recorded by the caller. This is the innermost span on the isolate
        // thread, so it is the one whose fields every `ctx.log()` line copies
        // into the stderr JSON (`span.app_slug`, `span.request_id`) — the two
        // values a person debugging an app actually has in hand.
        app_slug = tracing::field::Empty,
        org_id = %org_id,
        build_id = %build_id,
        invocation_id = %invocation_id,
        request_id = tracing::field::Empty,
        function = %function_name,
        mode = %mode,
    );
    oxy_telemetry::propagation::adopt_current_parent(&span);
    span
}

/// The instrumented entry point for every function run, in all three modes.
///
/// **This span is what makes function logs observable at all.** Until it
/// existed, `ctx.log()` / `console.*` emitted `tracing` events with no
/// enclosing span, and `SpanCollectorLayer::on_event` drops those on the floor —
/// so route-mode logs reached the server's stdout and nowhere else, while the
/// scheduled path persisted its own as `agentic_run_events`. The two modes
/// disagreed. Note the span alone is not sufficient: `runtime::run` also has to
/// carry it onto the isolate thread, because `tracing`'s current span is
/// thread-local.
///
/// A thin wrapper rather than an attribute on the body, so `status` and
/// `duration_ms` land on **every** return path — the body has several early
/// returns for workspace/context resolution failures, and a span that reports
/// only the happy path is worse than one that reports nothing.
#[cfg(feature = "custom-app-functions")]
#[tracing::instrument(
    name = "custom_app_function",
    skip_all,
    fields(
        app_id = %args.app.id,
        app_slug = %args.app.slug,
        org_id = %args.org_id,
        project_id = %args.app.project_id,
        build_id = %args.build_id,
        invocation_id = %args.invocation_id,
        function = %args.function_name,
        mode = %args.mode,
        request_id = tracing::field::Empty,
        status = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
        // Set by `failure_signal::Failure::report` when the invocation failed.
        otel.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
        // FaaS semantic conventions, so HyperDX's own views group invocations
        // by function; `otel.name` makes the exported span read
        // `fn <app>/<function>` while the tracing span keeps its name for the
        // product layer.
        otel.name = %format!("fn {}/{}", args.app.slug, args.function_name),
        faas.name = %format!("{}/{}", args.app.slug, args.function_name),
        faas.invocation_id = %args.invocation_id,
        faas.version = %args.build_id,
        faas.trigger = host_call_attrs::faas_trigger(args.mode),
    )
)]
async fn run_with_runtime(args: RunArgs<'_>) -> RunOutcome {
    let started = Instant::now();
    let span = tracing::Span::current();
    // Recorded, not declared, so an absent id stays absent instead of being
    // reported as an empty string that looks like a value. Recorded BEFORE the
    // run: the stderr JSON line copies the enclosing span's fields at the
    // moment of each event, so every `ctx.log()` line of this invocation
    // carries `span.request_id` — the id the browser, `oxyc proxy` and the
    // operator logs endpoint all quote.
    if let Some(id) = args.request_id {
        span.record("request_id", tracing::field::display(id));
    }

    let outcome = run_with_runtime_inner(args).await;

    span.record("status", outcome.0);
    span.record("duration_ms", started.elapsed().as_millis() as i64);
    if let Some(failure) = failure_signal::Failure::of(outcome.0, outcome.3, outcome.1.as_deref()) {
        failure.report();
    }
    outcome
}

/// Build the project context + host, spawn the cancel watchdog, and drive
/// the isolate. Kept off the main handler body to hold it under the
/// function-size budget.
#[cfg(feature = "custom-app-functions")]
async fn run_with_runtime_inner(args: RunArgs<'_>) -> RunOutcome {
    use crate::server::api::custom_apps_gates::build_project_context_with_role;
    use entity::prelude::Workspaces;

    // Resolve the project context (connectors) from the app's workspace.
    let workspace = match Workspaces::find_by_id(args.app.project_id)
        .one(args.db)
        .await
    {
        Ok(Some(ws)) => ws,
        Ok(None) => {
            return (
                "error",
                Some("workspace not found".into()),
                String::new(),
                0,
            );
        }
        Err(e) => {
            return (
                "error",
                Some(format!("workspace lookup failed: {e}")),
                String::new(),
                0,
            );
        }
    };
    // Resolve the invoking user's workspace role, so an app function can WRITE
    // to an airhouse-managed database when the person invoking it may.
    //
    // Without a role every function got a Reader credential and the first real
    // write failed with "Permission denied: Reader role cannot execute Update
    // statements". Reads were unaffected, which is why it went unnoticed for so
    // long: `ctx.warehouse.query` is a Reader operation, so the entire read path
    // worked and only writes were denied.
    //
    // ONLY on the user path. A system run (schedule / Airway step / manual job)
    // carries the ORG OWNER's user id because the invocation row needs a
    // non-null FK — not because an owner asked for anything — and resolving a
    // role from it would mint an Admin credential for everything that fires on
    // a timer. That is a larger grant than this is for, so those runs keep the
    // Reader default and cannot write airhouse.
    let workspace_role = if should_resolve_role(
        args.identity_kind,
        workspace.org_id.is_some(),
        args.write_destinations.is_empty(),
    ) {
        let org_id = workspace.org_id.expect("checked by should_resolve_role");
        crate::server::api::middlewares::workspace_context::resolve_effective_role(
            args.db,
            workspace.id,
            org_id,
            args.user_id,
            // `""` when the invocation carries no email, and that fails CLOSED.
            // The only thing the callee does with it is `assume::may_act_as`,
            // which checks it against the staff allow-list and the partner
            // grants — an empty string matches neither, so an emailless caller
            // gets real membership or nothing, never synthesized authority.
            args.user_email.as_deref().unwrap_or_default(),
        )
        .await
        .ok()
        // Drop a SYNTHESIZED Owner. The middleware keeps this third value
        // precisely so guards that must never accept an Oxy operator acting as
        // the tenant can tell one from a real member — and minting a credential
        // that WRITES the customer's warehouse is such a guard.
        //
        // An assume-role session is explicit, time-boxed and reason-logged, so
        // honouring it here would be defensible. It is refused because it was
        // never considered when this was written, and the rest of this change
        // fails closed: staff reading a tenant's airhouse still works, and staff
        // needing to write it should hold real membership rather than acquire
        // write authority as a side effect of invoking an app function.
        .filter(|(_, _, is_global_override)| !is_global_override)
        .map(|(_, role, _)| role)
    } else {
        None
    };

    let proj_ctx = match build_project_context_with_role(
        &workspace,
        args.user_id,
        args.app.project_id,
        workspace_role,
    )
    .await
    {
        Ok(c) => c,
        Err(_) => {
            return (
                "error",
                Some("could not build workspace context".into()),
                String::new(),
                0,
            );
        }
    };
    // Everything the isolate needs before it can start, resolved together.
    //
    // Identity facts for `ctx.user` are all resolved server-side so none of it can
    // be forged by the caller.
    //
    // `app_role` is what a function gates a privileged surface on (e.g. the
    // warehouse app's `?view=admin`) instead of a hard-coded email allowlist.
    // `org_role` + `teams` are tenant-internal standing, for a function that wants
    // to *explain* rather than gate ("ask your org admin to connect a warehouse").
    //
    // Only a human run resolves the org facts: a schedule executes under the
    // owner's id, and reporting `orgRole: "owner"` there would make an owner-only
    // branch fire on every tick.
    //
    // Concurrent, because these are independent reads and this is **pre-isolate
    // latency a caller waits on** — not the view recorder's background spawn, where
    // the same queries are free. `env` is in here for the same reason and is the
    // most expensive of the three: `resolve_function_env` lists the project's
    // secrets and then fetches each match one at a time, so leaving it sequential
    // in front of the identity reads paid for it twice over.
    //
    // `join!` rather than `try_join!`: each fact keeps its own fail-CLOSED posture,
    // so a blip on one can't blank the other, and an errored lookup yields no role,
    // never "admin".
    let human = args.identity_kind == runtime::CtxIdentityKind::User;
    let (env, app_role, org_standing, held) = tokio::join!(
        resolve_function_env(args.db, args.app.project_id, args.app.id),
        crate::server::api::custom_apps_auth::resolve_app_role(
            args.db,
            args.user_id,
            args.user_email.as_deref().unwrap_or(""),
            args.app,
        ),
        async {
            if human {
                // One membership read covers both the role and the teams gate.
                crate::server::api::custom_apps_auth::resolve_org_standing(
                    args.db,
                    args.user_id,
                    args.org_id,
                )
                .await
            } else {
                Ok((None, Vec::new()))
            }
        },
        async {
            // The caller's positions — what reach is decided from. A schedule
            // has none to read: it reaches everywhere as the system.
            if human {
                crate::server::api::operating_graph::reach::held_by(
                    args.db,
                    args.org_id,
                    args.user_id,
                )
                .await
            } else {
                Ok(Vec::new())
            }
        },
    );

    let app_role = app_role
        .unwrap_or_else(|e| {
            error!("app role lookup failed for app {}: {e}", args.app.id);
            None
        })
        .map(str::to_string);

    let (org_role, teams) = match org_standing {
        Ok((role, teams)) => (
            role.map(str::to_string),
            teams
                .into_iter()
                .map(|team| runtime::CtxTeam {
                    id: team.id.to_string(),
                    name: team.name,
                })
                .collect(),
        ),
        Err(e) => {
            error!("org standing lookup failed for app {}: {e}", args.app.id);
            (None, Vec::new())
        }
    };

    // Reach: the rule over the facts above. Fail-closed like its siblings — a
    // positions read that errored yields nowhere, never everywhere.
    let reach = {
        use crate::server::api::operating_graph::reach::{Caller, Reach, reach_of};
        let caller = Caller {
            system: !human,
            app_admin: app_role.as_deref() == Some("admin"),
            member: org_role.is_some(),
        };
        match held {
            Ok(held) => reach_of(caller, &held),
            Err(e) => {
                error!("reach lookup failed for app {}: {e}", args.app.id);
                Reach::nowhere()
            }
        }
    };

    let host = host::into_arc(host::ProjectFunctionHost::new(
        // Hand the runtime the project context behind its trait, so the host
        // depends on `FunctionProjectContext`, not on `agentic_wiring`. The
        // concrete `OxyProjectContext` is only named here (outside the runtime).
        std::sync::Arc::new(proj_ctx) as std::sync::Arc<dyn seam::FunctionProjectContext>,
        args.query_exec.clone(),
        args.db.clone(),
        args.write_destinations.clone(),
        args.app.project_id,
        args.app.id,
        args.org_id,
        args.user_id,
        args.app.name.clone(),
        args.caps.clone(),
        args.preagg.clone(),
        // The same reach `ctx.user` carries, so `ctx.semantic.query({ scope:
        // "reach" })` pins to what the function itself was told.
        reach.clone(),
        data_audit::InvocationIdentity {
            invocation_id: args.invocation_id,
            function_name: args.function_name.to_string(),
            mode: args.mode.to_string(),
            request_id: args.request_id,
            app_slug: args.app.slug.clone(),
            user_id: human.then_some(args.user_id),
            user_email: if human { args.user_email.clone() } else { None },
        },
    ));

    let ctx = runtime::InvocationCtx {
        user: runtime::CtxUser {
            id: args.user_id.to_string(),
            email: args.user_email,
            org_id: args.org_id.to_string(),
            name: args.user_name,
            picture: args.user_picture,
            app_role,
            org_role,
            teams,
            kind: args.identity_kind,
            reach,
        },
        env,
    };

    // §11.4 — cancellation watchdog: poll `cancel_requested_at` every 1s and
    // fire the cancel signal when it's set (dashboard cancel / client gone).
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let watchdog = tokio::spawn(spawn_cancel_watchdog(
        args.db.clone(),
        args.invocation_id,
        cancel_tx,
        args.cancel,
    ));

    // Copied out before the `args` fields below are moved into the call.
    let app_id = args.app.id;
    let app_slug = args.app.slug.clone();
    let request_id = args.request_id;
    let org_id = args.org_id;
    let build_id = args.build_id;
    let invocation_id = args.invocation_id;
    let function_name = args.function_name.to_string();
    let mode = args.mode.to_string();

    // The host outlives the run by one call: the audit rows for the writes
    // this invocation made are written once, here, not on every write.
    let host_for_audit = host.clone();
    let result = runtime::run(
        args.artifact_js,
        ctx,
        runtime::FnRequest {
            method: args.method,
            headers: args.headers,
            body: args.body,
        },
        host,
        cancel_rx,
        args.timeout,
        args.logs,
        // No `tracing` parent, carrying the same identifying fields as the
        // invocation span above. See `runtime::run`: a clone of the caller's
        // span would be pinned open by an isolate thread that outlives a cancel
        // or a timeout, exporting the invocation late and with a `duration_ns`
        // that includes the leaked thread. The product layer correlates the two
        // by `invocation_id`. The platform trace joins them properly:
        // `adopt_current_parent` parents by OpenTelemetry *context* — a pair
        // of ids — which pins nothing open.
        {
            let span = isolate_span(
                app_id,
                org_id,
                build_id,
                invocation_id,
                &function_name,
                &mode,
            );
            span.record("app_slug", app_slug.as_str());
            if let Some(id) = request_id {
                span.record("request_id", tracing::field::display(id));
            }
            span
        },
    )
    .await;
    watchdog.abort();
    host_for_audit.end_of_invocation().await;

    match result {
        Ok(resp) => ("success", None, resp.body, resp.status),
        Err(runtime::RuntimeError::Cancelled) => (
            "cancelled",
            Some("function was cancelled".into()),
            String::new(),
            // Not a function status: the isolate never returned one. The error
            // framing carries these, so the value is never read.
            0,
        ),
        Err(runtime::RuntimeError::Timeout) => (
            "timeout",
            Some("function execution timed out".into()),
            String::new(),
            // Not a function status: the isolate never returned one. The error
            // framing carries these, so the value is never read.
            0,
        ),
        Err(e) => ("error", Some(e.to_string()), String::new(), 0),
    }
}

/// Resolve `ctx.env` for an invocation (§11.4): secrets stored under the
/// `apps/<app_id>/<KEY>` prefix in the project's secret manager, exposed to
/// the isolate with the prefix stripped (so the function sees plain `KEY`).
/// Resolved fresh per invocation — never cached in the artifact.
#[cfg(feature = "custom-app-functions")]
async fn resolve_function_env(
    db: &sea_orm::DatabaseConnection,
    project_id: Uuid,
    app_id: Uuid,
) -> std::collections::BTreeMap<String, String> {
    use oxy::service::secret_manager::SecretManagerService;

    let secret_manager = SecretManagerService::new(project_id);
    let prefix = format!("apps/{app_id}/");

    let names = match secret_manager.list_secrets(db).await {
        Ok(secrets) => secrets
            .into_iter()
            .filter_map(|s| {
                // Take the suffix as an owned `String` first so the borrow of
                // `s.name` ends before we move `s.name` into the tuple.
                let key = s.name.strip_prefix(&prefix)?.to_string();
                Some((s.name, key))
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            error!("ctx.env: failed to list secrets for app {app_id}: {e}");
            return Default::default();
        }
    };

    let mut env = std::collections::BTreeMap::new();
    for (full_name, key) in names {
        match secret_manager.get_secret(&full_name).await {
            Some(value) => {
                env.insert(key, value);
            }
            None => error!("ctx.env: failed to resolve secret '{full_name}'"),
        }
    }
    env
}

/// Resolve `cancel_tx` (which terminates the isolate) on the first of two
/// signals: the in-process `token` firing (the runtime/worker cancelled this
/// task — e.g. fleet shutdown or an operator cancel) or
/// `app_function_invocations.cancel_requested_at` going non-null (cross-process:
/// dashboard cancel / client gone), polled once per second.
#[cfg(feature = "custom-app-functions")]
async fn spawn_cancel_watchdog(
    db: sea_orm::DatabaseConnection,
    invocation_id: Uuid,
    cancel_tx: tokio::sync::oneshot::Sender<()>,
    token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            // In-process cancel — fire immediately rather than waiting up to a
            // second for the next poll.
            _ = token.cancelled() => {
                let _ = cancel_tx.send(());
                return;
            }
            // Cross-process cancel — poll the invocation's flag.
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                match AppFunctionInvocations::find_by_id(invocation_id).one(&db).await {
                    Ok(Some(row)) if row.cancel_requested_at.is_some() => {
                        let _ = cancel_tx.send(());
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("cancel watchdog poll failed: {e}");
                        return;
                    }
                }
            }
        }
    }
}

/// Headers a route function may see, beyond the `x-*` rule below.
///
/// An **allowlist**, deliberately. A denylist would have to enumerate every
/// credential-bearing header Oxy uses, and the authenticator sits behind a
/// trait (`BuiltInAuthenticator::authenticate`) that can grow new ones — a
/// denylist here would silently stop being correct the day it does.
const PASSTHROUGH_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "accept",
    "accept-language",
    "user-agent",
    "traceparent",
    // Webhook signatures that do NOT start with `x-`. The `x-` rule covers the
    // rest (`x-hub-signature-256`, `x-slack-signature`, `x-shopify-hmac-sha256`,
    // `x-twilio-signature`, …), which is what keeps a new provider from needing
    // a PR here.
    "stripe-signature",
];

/// Headers that must NEVER reach app code, even though they match `x-*`.
///
/// `cookie` and `authorization` are the load-bearing ones: the session cookie
/// (`oxy_session`) and the bearer header carry **the same JWT**
/// (`crates/auth/src/built_in.rs`), so handing either to a function would let
/// any custom app lift a viewer's session and act as them across the whole
/// platform — an escalation from "app author" to "any viewer".
///
/// An app that wants its own shared-secret auth on a route must use a custom
/// `x-` header; it cannot reuse `authorization`, because we cannot tell its
/// secret apart from ours.
const BLOCKED_HEADERS: &[&str] = &[
    "cookie",
    "set-cookie",
    "authorization",
    "proxy-authorization",
    "x-api-key",
];

/// Project an inbound `HeaderMap` down to what a route function is allowed to
/// see, lowercasing names so a function can index them predictably.
///
/// Anything not on [`PASSTHROUGH_HEADERS`], not `x-*`, or on
/// [`BLOCKED_HEADERS`] (or under the reserved `x-oxy-` prefix) is dropped.
///
/// **A header sent twice collapses to its FIRST value**, matching what
/// `HeaderMap::get` returns everywhere else in the codebase — including the
/// authenticator. Last-wins would mean the platform and the app disagree about
/// which value is real for the same request, which is the shape request
/// smuggling exploits. Neither joining (`a, b`) nor dropping the header is
/// obviously better, but silently disagreeing with `get` is obviously worse.
fn sanitize_request_headers(
    headers: &axum::http::HeaderMap,
) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (name, value) in headers.iter() {
        let key = name.as_str().to_ascii_lowercase();
        if BLOCKED_HEADERS.contains(&key.as_str()) {
            continue;
        }
        if key.starts_with("x-oxy-") {
            continue;
        }
        let allowed = PASSTHROUGH_HEADERS.contains(&key.as_str()) || key.starts_with("x-");
        if !allowed {
            continue;
        }
        // A header whose bytes are not UTF-8 is dropped rather than lossily
        // decoded: a signature that silently changes shape is worse than an
        // absent one, because the function would verify it and fail confusingly.
        if let Ok(v) = value.to_str() {
            // `or_insert`, not `insert` — first value wins (see above).
            out.entry(key).or_insert_with(|| v.to_string());
        }
    }
    out
}

#[cfg(test)]
mod fetch_cap_tests {
    use super::{FunctionManifestEntry, resolve_fetch_max_bytes};

    fn entry(json: serde_json::Value) -> FunctionManifestEntry {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn absent_means_the_host_default_not_zero() {
        // The critical case: `None` must reach the host so it falls back to
        // FETCH_MAX_BYTES. A `0` here would deny every fetch instead.
        assert_eq!(resolve_fetch_max_bytes(&entry(serde_json::json!({}))), None);
        assert_eq!(
            resolve_fetch_max_bytes(&entry(serde_json::json!({ "fetch": {} }))),
            None
        );
        // An explicit 0 is nonsense, not a deny-all — treat it as unspecified.
        assert_eq!(
            resolve_fetch_max_bytes(&entry(
                serde_json::json!({ "fetch": { "maxResponseBytes": 0 } })
            )),
            None
        );
    }

    #[test]
    fn a_declared_value_is_honoured_and_clamped_to_the_platform_ceiling() {
        // 32 MiB — comfortably above the 10 MiB default, below the ceiling.
        assert_eq!(
            resolve_fetch_max_bytes(&entry(
                serde_json::json!({ "fetch": { "maxResponseBytes": 33_554_432u64 } })
            )),
            Some(33_554_432)
        );
        // A manifest cannot talk its way past the platform: 200 MiB clamps to
        // the 100 MiB default ceiling.
        assert_eq!(
            resolve_fetch_max_bytes(&entry(
                serde_json::json!({ "fetch": { "maxResponseBytes": 209_715_200u64 } })
            )),
            Some(100 * 1024 * 1024)
        );
    }

    #[test]
    fn an_unrelated_manifest_is_unaffected() {
        // Every function shipped before this field existed must keep the
        // built-in default rather than acquiring a cap.
        let e = entry(serde_json::json!({
            "route": true, "timeoutSeconds": 120, "destinations": ["clickhouse"]
        }));
        assert_eq!(resolve_fetch_max_bytes(&e), None);
    }
}

#[cfg(test)]
mod request_header_tests {
    use super::sanitize_request_headers;
    use axum::http::HeaderMap;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    // The one that matters. `oxy_session` and the bearer header carry the same
    // JWT, so either one reaching app code is an escalation from "app author"
    // to "any viewer of the app".
    #[test]
    fn credentials_never_reach_the_function() {
        let out = sanitize_request_headers(&hm(&[
            ("cookie", "oxy_session=eyJhbGciOi.REAL_JWT"),
            ("authorization", "Bearer eyJhbGciOi.REAL_JWT"),
            ("proxy-authorization", "Basic abc"),
            ("x-api-key", "sk-live-123"),
            ("x-oxy-required-role", "admin"),
            ("content-type", "application/json"),
        ]));
        assert_eq!(out.keys().collect::<Vec<_>>(), vec!["content-type"]);
        // Belt and braces: no value anywhere in the output carries the token.
        assert!(
            !out.values().any(|v| v.contains("REAL_JWT")),
            "a credential leaked into the isolate: {out:?}"
        );
    }

    #[test]
    fn webhook_signatures_pass_without_needing_a_pr_per_provider() {
        let out = sanitize_request_headers(&hm(&[
            ("stripe-signature", "t=1,v1=abc"),
            ("x-hub-signature-256", "sha256=abc"),
            ("x-slack-request-timestamp", "1700000000"),
            ("x-shopify-hmac-sha256", "abc="),
        ]));
        assert_eq!(out.len(), 4, "{out:?}");
    }

    #[test]
    fn a_repeated_header_keeps_its_first_value() {
        // `HeaderMap::get` — which the authenticator uses — returns the first
        // value, so app code must see the same one. Platform and app disagreeing
        // about which value is real for one request is the smuggling shape.
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "1.1.1.1".parse().unwrap());
        h.append("x-forwarded-for", "2.2.2.2".parse().unwrap());
        let out = sanitize_request_headers(&h);
        assert_eq!(
            out.get("x-forwarded-for").map(String::as_str),
            Some("1.1.1.1")
        );
        assert_eq!(
            h.get("x-forwarded-for").unwrap().to_str().unwrap(),
            out["x-forwarded-for"],
            "must agree with what HeaderMap::get sees"
        );
    }

    #[test]
    fn unknown_non_x_headers_are_dropped_and_names_are_lowercased() {
        let out = sanitize_request_headers(&hm(&[
            ("Content-Type", "text/csv"),
            ("referer", "https://example.com"),
            ("forwarded", "for=1.2.3.4"),
        ]));
        // Conservative default: only the allowlisted one survives, lowercased.
        assert_eq!(
            out.get("content-type").map(String::as_str),
            Some("text/csv")
        );
        assert!(!out.contains_key("referer"), "{out:?}");
        assert!(!out.contains_key("forwarded"), "{out:?}");
    }
}

#[cfg(test)]
mod job_policy_tests {
    use super::function_task_policy;
    use agentic_core::delegation::BackoffStrategy;
    use serde_json::json;

    #[test]
    fn no_retries_block_is_none() {
        assert!(function_task_policy(&json!({ "route": true })).is_none());
    }

    #[test]
    fn single_attempt_is_none() {
        // maxAttempts: 1 means "try once" — no retry policy.
        assert!(function_task_policy(&json!({ "retries": { "maxAttempts": 1 } })).is_none());
    }

    #[test]
    fn maps_attempts_to_retries_and_exponential_backoff() {
        let policy = function_task_policy(&json!({
            "retries": { "maxAttempts": 3, "minTimeoutMs": 2000, "maxTimeoutMs": 40000 }
        }))
        .expect("expected a policy");
        let retry = policy.retry.expect("expected retry");
        // 3 attempts total = 2 retries after the first.
        assert_eq!(retry.max_retries, 2);
        match retry.backoff {
            BackoffStrategy::Exponential {
                initial_delay_ms,
                max_delay_ms,
            } => {
                assert_eq!(initial_delay_ms, 2000);
                assert_eq!(max_delay_ms, 40000);
            }
            other => panic!("expected exponential backoff, got {other:?}"),
        }
    }

    #[test]
    fn defaults_backoff_window_when_omitted() {
        let policy =
            function_task_policy(&json!({ "retries": { "maxAttempts": 2 } })).expect("policy");
        match policy.retry.unwrap().backoff {
            BackoffStrategy::Exponential {
                initial_delay_ms,
                max_delay_ms,
            } => {
                assert_eq!(initial_delay_ms, 1000);
                assert_eq!(max_delay_ms, 30000);
            }
            other => panic!("expected exponential backoff, got {other:?}"),
        }
    }

    #[test]
    fn caps_runaway_retry_count() {
        let policy =
            function_task_policy(&json!({ "retries": { "maxAttempts": 9999 } })).expect("policy");
        assert_eq!(policy.retry.unwrap().max_retries, super::MAX_JOB_RETRIES);
    }
}

#[cfg(test)]
mod function_status_tests {
    use super::success_sse_body;

    /// The terminal frame carries the status the FUNCTION returned.
    ///
    /// This was hardcoded `200`, so a handler answering 403 framed as a success
    /// and the SDK resolved it. Asserted on the pure helper because the failure
    /// is invisible from here — it surfaces as a client that never sees a
    /// rejection, three layers away.
    #[test]
    fn the_done_frame_carries_the_status_it_was_given() {
        let body = success_sse_body(r#"{"error":"forbidden"}"#, 403);
        assert!(
            body.contains(r#""status":403"#),
            "the done frame must report the function's status: {body}"
        );
        // And the payload still arrives as a parsed value, not a re-encoded
        // string — the property the SDK's single `JSON.parse` depends on.
        assert!(
            body.contains(r#"data: {"error":"forbidden"}"#),
            "the body must be emitted as a JSON value: {body}"
        );
    }

    /// A 2xx that is not 200 is passed through unaltered.
    ///
    /// A naive `status == 200` check anywhere in this path would turn every
    /// 201 or 204 into a rejection on the SDK side.
    #[test]
    fn a_created_status_is_not_normalised_to_200() {
        assert!(success_sse_body("{}", 201).contains(r#""status":201"#));
    }

    /// A replay of a row written before `result_status` existed reports 200.
    ///
    /// Those rows genuinely do not know what they returned, and 200 is what
    /// they were already being reported as — so the fallback is the status quo
    /// rather than an assertion. Pinned because a backfill to some other
    /// default would silently change what every historical replay claims.
    #[test]
    fn a_missing_stored_status_replays_as_200() {
        let stored: Option<i16> = None;
        let body = success_sse_body("{}", stored.unwrap_or(200) as u16);
        assert!(body.contains(r#""status":200"#), "{body}");
    }
}

#[cfg(test)]
mod org_capability_tests {
    use super::FunctionManifestEntry;

    /// `ctx.org.people()` is DENIED unless the manifest asks for it.
    ///
    /// Every arm here is a way an author could fail to declare the capability,
    /// and all of them must land on `false`. A capability that fails OPEN is
    /// not a bug in a feature, it is a directory handed to a bundle the tenant
    /// did not write — so the absence cases are asserted individually rather
    /// than as one "default is false".
    #[test]
    fn org_read_fails_closed_on_every_way_of_not_asking() {
        let deny = [
            ("no manifest block at all", r#"{}"#),
            ("an empty org block", r#"{"org":{}}"#),
            ("read explicitly false", r#"{"org":{"read":false}}"#),
            ("org present but null", r#"{"org":null}"#),
            // A neighbouring capability must not carry this one in with it.
            (
                "some OTHER capability granted",
                r#"{"storage":{"read":true},"email":{"send":true}}"#,
            ),
        ];
        for (why, json) in deny {
            let m: FunctionManifestEntry = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("{why}: manifest did not parse: {e}"));
            assert!(!m.org_read(), "org.read must be denied when there is {why}");
        }

        // And the one shape that grants it actually does, so the test above is
        // not passing because `org_read()` returns false unconditionally.
        let granted: FunctionManifestEntry =
            serde_json::from_str(r#"{"org":{"read":true}}"#).unwrap();
        assert!(
            granted.org_read(),
            "org.read must be granted when asked for"
        );
    }

    /// Declaring the directory does not quietly grant anything else.
    ///
    /// The capabilities are independent by construction, and this pins that:
    /// an app that wants to name a colleague has not thereby asked to email
    /// one, write a secret, or reach the app's storage silo.
    #[test]
    fn asking_for_the_directory_grants_only_the_directory() {
        let m: FunctionManifestEntry = serde_json::from_str(r#"{"org":{"read":true}}"#).unwrap();
        assert!(m.org_read());
        assert!(!m.email_send(), "org.read must not imply email.send");
        assert!(!m.secrets_write(), "org.read must not imply secrets.write");
        assert!(!m.storage_read(), "org.read must not imply storage.read");
        assert!(!m.storage_write(), "org.read must not imply storage.write");
    }
}

#[cfg(test)]
mod webhook_contract_tests {
    use super::function_webhook_contract;

    fn manifest(webhook: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "route": true, "webhook": webhook })
    }

    #[test]
    fn reads_a_complete_declaration() {
        let m = manifest(serde_json::json!({
            "secretVar": "UBER_SIGNING_KEYS",
            "signatureHeader": "x-uber-signature",
            "encoding": "base64",
        }));
        assert_eq!(
            function_webhook_contract(&m),
            Some((
                "UBER_SIGNING_KEYS".to_string(),
                "x-uber-signature".to_string(),
                "base64".to_string()
            ))
        );
    }

    /// Hex is the default because it is what most providers send (Uber included),
    /// so the common manifest does not have to say so.
    #[test]
    fn encoding_defaults_to_hex() {
        let m = manifest(serde_json::json!({
            "secretVar": "K",
            "signatureHeader": "x-sig",
        }));
        assert_eq!(function_webhook_contract(&m).unwrap().2, "hex");
    }

    /// No `webhook:` block is the 404 case — the function exists and is
    /// invocable by its owner, but it is not a webhook endpoint, and an
    /// anonymous caller learns nothing about it.
    #[test]
    fn a_function_without_a_webhook_block_has_no_contract() {
        let m = serde_json::json!({ "route": true, "timeoutSeconds": 30 });
        assert_eq!(function_webhook_contract(&m), None);
    }

    /// A half-written declaration must fail CLOSED. Both of these would
    /// otherwise leave an anonymous route with nothing to verify against: no
    /// secret to check, or no header to read the signature from.
    #[test]
    fn an_incomplete_declaration_yields_no_contract() {
        for partial in [
            serde_json::json!({ "signatureHeader": "x-sig" }),
            serde_json::json!({ "secretVar": "K" }),
            serde_json::json!({}),
        ] {
            assert_eq!(
                function_webhook_contract(&manifest(partial.clone())),
                None,
                "incomplete webhook block yielded a contract: {partial}"
            );
        }
    }

    /// An unparseable manifest is not a webhook endpoint either — it must not
    /// panic, and it must not resolve to a contract.
    #[test]
    fn a_malformed_manifest_yields_no_contract() {
        assert_eq!(
            function_webhook_contract(&serde_json::json!("not an object")),
            None
        );
    }
}
