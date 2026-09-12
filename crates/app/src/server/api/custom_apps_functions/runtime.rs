//! V8-isolate execution for Oxy Functions.
//!
//! See `internal-docs/customer-apps-functions.md` §4 and
//! §11. Given a bundled function artifact (esbuild ESM output) and a `ctx`
//! payload, run `export default async (req, ctx) => Response` to completion
//! and return the resulting status/body.
//!
//! ## Why a dedicated thread + channel broker
//!
//! `deno_core::JsRuntime` owns a V8 isolate and is `!Send` — it cannot be
//! held across `.await` in the `Send` axum handler future, nor moved between
//! tokio workers. So the isolate runs on its **own OS thread** with a
//! current-thread tokio runtime, and every `ctx.*` host call is bridged to
//! the async host side (DB connectors, outbound fetch) over an mpsc channel.
//! The handler future only ever holds channel endpoints + a join handle, all
//! of which are `Send`.
//!
//! ```text
//!   handler (Send)            isolate thread (!Send)
//!   ─────────────             ──────────────────────
//!   broker loop  <── HostCall ── op_ctx_query / op_ctx_fetch
//!        │  host.query(sql).await
//!        └────────── oneshot reply ─────────►  resolves the JS promise
//! ```
//!
//! `ctx.semantic.query` (airlayer), `ctx.airway.run` (Airway runner), and
//! `ctx.warehouse.{insert,exec,upsert}` (project-database allowlist, §11.3)
//! are all wired to real backends.
//!
//! `ctx.tx` adds multi-statement atomicity over that same allowlist: the
//! isolate holds only a handle id, the pinned connection lives in the host's
//! `tx::TxRegistry`, and the bootstrap wrapper owns the commit/rollback
//! bracket so an author cannot leave one open. `ctx.queryStream` (§11.5) fetches up to
//! `FUNCTION_STREAM_MAX_ROWS` rows in one host call and yields them to the
//! function as an async generator in client-side batches — a pragmatic MVP
//! pending a true warehouse-cursor implementation.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use deno_core::{JsRuntime, OpState, RuntimeOptions, op2};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

/// Per-invocation context handed to the isolate. Built fresh for every
/// invocation from the resolved identity (design doc §11.7) — never cached
/// across invocations.
#[derive(Debug, Clone, Serialize)]
pub struct InvocationCtx {
    pub user: CtxUser,
    pub env: BTreeMap<String, String>,
}

/// One org team the caller belongs to, as surfaced through `ctx.user.teams`.
///
/// **Scoped to the app's own org.** A team the caller holds in some *other* org
/// is never reported here — the same user in two tenants must not learn one
/// tenant's team names from the other's app.
#[derive(Debug, Clone, Serialize)]
pub struct CtxTeam {
    pub id: String,
    pub name: String,
}

/// Where this invocation's identity came from.
///
/// The distinction is **"is there a caller to attribute this to"**, not "did a
/// human cause it". Every background path runs under the org **owner's**
/// `user_id` (the invocation row needs a non-null FK, and `ctx.secrets` needs a
/// `created_by`) and carries no caller — including an operator's manual
/// **Run now**, which `trigger_function_job` deliberately routes down the same
/// system path under the owner identity, with no caller context beyond whatever
/// `input` the trigger was given. So a person may well have clicked; the platform
/// simply did not carry who through the task queue.
///
/// A function that branches on identity — "email the person who clicked", "show
/// the admin view" — has to be able to tell the two apart, and an email-sniffing
/// check (`endsWith("@system.oxy")`) is exactly the kind of heuristic that
/// silently stops working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CtxIdentityKind {
    /// A signed-in human called the function over its HTTP route.
    User,
    /// No caller to attribute it to: a schedule tick, an Airway transform step,
    /// or an operator's manual **Run now** (which the job trigger routes down
    /// this same path). Every *caller* field (`name`, `picture`, `orgRole`,
    /// `teams`) is absent, and `email` is the synthetic
    /// `schedule+<fn>@system.oxy`.
    ///
    /// A manual run therefore cannot reach the operator who triggered it —
    /// `run_function_job` discards the authenticated user, and the task payload
    /// has nowhere to put it. Threading it through is a real follow-up, and a
    /// behaviour change: the run would then execute under that operator's
    /// authority rather than the owner's.
    System,
}

/// The identity of whoever (or whatever) invoked this function.
///
/// Assembled server-side from the authenticated session on every invocation and
/// never cached across them, so **nothing here is client-supplied** — that is
/// the whole point of reading identity from `ctx` rather than from the request
/// body. See `internal-docs/custom-apps-user-identity.md` for the full contract
/// and for what the *client* side (`useShellContext`) can and cannot be trusted
/// for.
#[derive(Debug, Clone, Serialize)]
pub struct CtxUser {
    /// `users.id`. On a system invocation this is the org owner's id, not a
    /// caller — check `kind` before attributing anything to it.
    pub id: String,
    /// `users.email`, or the synthetic `schedule+<fn>@system.oxy` when
    /// `kind == "system"`.
    ///
    /// `None` — serialized as `ctx.user.email === null` — for a frontline
    /// worker enrolled without a mailbox. Deliberately NOT flattened to `""`:
    /// a function branching on "can I email this person" must be able to tell
    /// "no address" from "an address that happens to be empty", and an app that
    /// passes `""` to `ctx.email.send` gets an SES rejection instead of an
    /// obvious `null` check. See `internal-docs/frontline-identity.md`.
    pub email: Option<String>,
    /// The org that owns this app — the tenant boundary for any query the
    /// function runs.
    ///
    /// Serialized as `orgId`. Before 2026-08-21 this went out as `org_id`,
    /// which meant the documented `ctx.user.orgId` was `undefined` — a silent
    /// footgun for any SQL filtering on it. `__buildCtx` still mirrors the old
    /// `org_id` key so functions written against the shipped behaviour keep
    /// working.
    #[serde(rename = "orgId")]
    pub org_id: String,
    /// `users.name` — display identity, absent on a system invocation.
    ///
    /// Free text the user controls. Fine for a greeting or an audit row; never
    /// a key, and never interpolated into SQL or HTML without escaping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `users.picture` — an avatar URL, absent when unset or on a system
    /// invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// The caller's role **within this app** — `"admin"`, `"member"`, or absent.
    /// Server-derived: `"admin"` is `Ring::AppAdmin` in `oxy-authz` (an app grant,
    /// org owner/admin, or staff break-glass), and `"member"` is any grant the
    /// caller holds on the app — **direct (`app_members`) or through a team they
    /// belong to (`app_team_grants` × `org_team_members`)**. An app gates its
    /// privileged surface on this rather than on a client-side flag or a
    /// hard-coded email allowlist.
    ///
    /// A **system** invocation runs under the org owner, so this reads `"admin"`
    /// there — a schedule has owner authority by construction. Gate on `kind`
    /// too if a surface must be human-only.
    #[serde(rename = "appRole", skip_serializing_if = "Option::is_none")]
    pub app_role: Option<String>,
    /// The caller's role in the owning **org** — `"owner"`, `"admin"`, or
    /// `"member"`; absent when they reach the app without an org membership
    /// (Oxy staff on break-glass) or on a system invocation.
    ///
    /// A *fact* read straight off `org_members.role`, not an authorization
    /// verdict: org standing and app standing are different rings, and an app
    /// admin need not be an org Admin. Gate on [`Self::app_role`]; use this to
    /// explain, label, or route — "your org admin can change this".
    #[serde(rename = "orgRole", skip_serializing_if = "Option::is_none")]
    pub org_role: Option<String>,
    /// The org teams the caller belongs to, name-sorted, scoped to this app's
    /// org. Empty when they belong to none, and always present so a function can
    /// `.some(...)` without a null check.
    ///
    /// Teams are how an org grants an app to a group it already recognises, so
    /// they are useful for *shaping* a view (default the Finance team to the
    /// finance tab). They are not a permission: a team only means something on
    /// an app through `app_team_grants`, which is already folded into
    /// [`Self::app_role`].
    pub teams: Vec<CtxTeam>,
    /// Whether a human or the platform invoked this function.
    pub kind: CtxIdentityKind,
    /// Where the caller may act — the operating graph's answer, decided from
    /// their assignments before the function runs and applied by the function
    /// through `@oxy-hq/sdk/ops`. A system invocation reaches everywhere; a
    /// lookup failure reaches nowhere, so a blip cannot widen anything.
    /// `internal-docs/operating-graph.md` §3.3.
    pub reach: crate::server::api::operating_graph::reach::Reach,
}

/// Result of running a function to completion.
#[derive(Debug, Deserialize)]
pub struct FnResponse {
    #[serde(default = "default_status")]
    pub status: u16,
    #[serde(default)]
    pub body: String,
}

fn default_status() -> u16 {
    200
}

/// Errors surfaced to the route handler as SSE `event: error` frames.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("function threw: {0}")]
    Js(String),
    #[error("function was cancelled")]
    Cancelled,
    #[error("function execution timed out")]
    Timeout,
    #[error("internal runtime error: {0}")]
    Internal(String),
}

/// Function-scoped row cap for `ctx.query` (design doc §11.5): 10x the UI
/// `MAX_ROWS` cap, since ETL functions legitimately need more rows than a
/// rendered table. Genuinely large scans use `ctx.queryStream` instead.
pub const FUNCTION_MAX_ROWS: usize = 100_000;

/// Row cap for `ctx.queryStream` (design doc §11.5): a higher ceiling than
/// `FUNCTION_MAX_ROWS` for genuinely large scans. The MVP implementation
/// fetches up to this many rows in one shot and yields them to the isolate
/// in client-side batches; a true warehouse-cursor implementation is future
/// work.
pub const FUNCTION_STREAM_MAX_ROWS: usize = 1_000_000;

/// Host-side data plane the isolate calls back into. Implemented in
/// `mod.rs` against the resolved project context (connectors) + an HTTP
/// client. `Send + Sync` so the broker loop can `Arc`-clone it per call.
#[async_trait::async_trait]
pub trait FunctionHost: Send + Sync {
    /// `ctx.query(sql)` — read-only SQL, function-scoped row cap. Returns
    /// the rows as a JSON array value.
    async fn query(&self, sql: String) -> Result<serde_json::Value, String>;
    /// Called once after the isolate has finished (or been cancelled), on the
    /// caller's side of the channel: the place for work that must happen once
    /// per invocation rather than once per host call.
    async fn end_of_invocation(&self) {}
    /// `ctx.queryStream(sql)` — read-only SQL with a higher row cap
    /// (`FUNCTION_STREAM_MAX_ROWS`) for large scans. Returns the rows as a
    /// JSON array value; the isolate yields them to the function in batches.
    async fn query_stream(&self, sql: String) -> Result<serde_json::Value, String>;
    /// `ctx.fetch(url, init)` — SSRF-allowlisted outbound HTTP with a
    /// response size cap. Returns `{ status, body }`.
    async fn fetch(
        &self,
        url: String,
        init: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `ctx.semantic.query(spec)` — airlayer-compiled semantic query.
    /// `spec` is the JSON-encoded `agentic_semantic::config::SemanticQueryConfig`.
    async fn semantic_query(&self, spec: serde_json::Value) -> Result<serde_json::Value, String>;

    /// `ctx.org.people()` — the org's people directory, read-only.
    ///
    /// Takes no arguments on purpose: the org is the host's, so a function
    /// cannot name one and a manifest cannot point this at another tenant.
    ///
    /// Defaulted rather than required, because every other implementor of this
    /// trait is a test double that has no directory to answer with — and the
    /// default is the same fail-closed refusal the capability gate gives, so a
    /// double that forgets to override it denies rather than inventing people.
    async fn org_people(&self) -> Result<serde_json::Value, String> {
        Err("ctx.org.people is not available in this host".to_string())
    }
    /// `ctx.org.places()` — the org's locations. Same shape and same
    /// fail-closed default as `org_people`.
    async fn org_places(&self) -> Result<serde_json::Value, String> {
        Err("ctx.org.places is not available in this host".to_string())
    }
    /// `ctx.org.assignments()` — who holds which position where.
    async fn org_assignments(&self) -> Result<serde_json::Value, String> {
        Err("ctx.org.assignments is not available in this host".to_string())
    }
    /// `ctx.airway.run(pipelineRef, variables)` — seed an Airway ELT run.
    /// Returns `{ runId }`; the run is driven asynchronously by the worker
    /// fleet (it does not block on completion — ELT runs routinely exceed
    /// the function timeout ceiling).
    async fn airway_run(
        &self,
        pipeline_ref: String,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `ctx.warehouse.{insert,exec,upsert}` — write to one of the app's
    /// configured destination databases. `op` is `"insert"`, `"exec"`, or
    /// `"upsert"`; `payload` carries `{ database, table?, rows?, sql? }`
    /// depending on `op`. Validated against the project's configured
    /// databases (§11.3) before execution.
    /// `ctx.warehouse.query(database, sql)` — a read against a named database.
    async fn warehouse_query(
        &self,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String>;

    async fn warehouse_write(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `ctx.secrets.set(key, value)` — upsert an app-scoped secret
    /// (`apps/<app_id>/<key>`) into the same namespace `ctx.env` reads. Gated
    /// by the fail-closed `secrets.write` manifest capability.
    async fn secrets_set(&self, key: String, value: String) -> Result<serde_json::Value, String>;
    /// `ctx.email.send(input)` — send email on behalf of the app. Gated by the
    /// fail-closed `email.send` manifest capability; the platform controls the
    /// `from` address (the author may set `replyTo` only). `input` is the JS
    /// payload object; returns `{ messageId }`.
    async fn send_email(&self, input: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `ctx.storage.{getUploadUrl,getDownloadUrl,put,get,head,list,delete,copy}`
    /// — presigned S3 file storage scoped to the app's silo. `op` selects the
    /// operation; `payload` carries its args. Gated by the fail-closed
    /// `storage.{read,write}` manifest capabilities, per op in
    /// `host::check_storage_capability`. Mirrors `warehouse_write`'s
    /// single-op-dispatcher shape.
    async fn storage(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `ctx.tx(database, fn)` — a multi-statement transaction on a pinned
    /// connection. `op` is one of `begin` / `query` / `exec` / `commit` /
    /// `rollback`; `payload` carries its args. Same single-op-dispatcher shape
    /// as `warehouse_write`, and gated by the same fail-closed `destinations`
    /// allowlist — a transaction is a write, so it may not reach a database the
    /// function did not declare.
    ///
    /// The op split exists because the transaction has to stay open across
    /// `await`s **in the author's JavaScript**: the isolate holds a handle id
    /// and the pinned connection lives here.
    async fn tx(&self, op: String, payload: serde_json::Value)
    -> Result<serde_json::Value, String>;
    /// `ctx.oltp.{query,exec}` — read/write the app's OWN per-org OLTP schema
    /// (`app_<writer>`) on the managed Postgres tenant. `op` is `query` or
    /// `exec`; `payload` carries `{ sql, params? }`. Gated by the fail-closed
    /// `oltp` manifest capability and the OLTP kill-switch, and scoped to the
    /// app's own writer role — so unlike `ctx.warehouse` (read-only analyst on a
    /// managed database) it can write, and cannot see another app's data.
    async fn oltp(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

/// A request the isolate sends to the broker loop.
enum HostCall {
    Query {
        sql: String,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    QueryStream {
        sql: String,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Fetch {
        url: String,
        init: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    SemanticQuery {
        spec: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    AirwayRun {
        pipeline_ref: String,
        variables: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    WarehouseWrite {
        op: String,
        payload: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    SecretsSet {
        key: String,
        value: String,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    SendEmail {
        input: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Storage {
        op: String,
        payload: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    /// `ctx.org.people()` — no arguments; the org is the host's, never the
    /// function's.
    OrgPeople {
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    OrgPlaces {
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    OrgAssignments {
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Tx {
        op: String,
        payload: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Oltp {
        op: String,
        payload: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, String>>,
    },
}

// ── ctx ops ──────────────────────────────────────────────────────────────

use super::LogLine;

/// What `op_ctx_log` should do with a line, given how many are already buffered.
///
/// Pure so the cap can be tested without a V8 isolate — and it needed testing:
/// the first version returned at the marker without pushing, so the buffer
/// parked at exactly `MAX_CAPTURED_LOGS`, `Silent` was unreachable, and the
/// marker re-fired once per suppressed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LogAction {
    /// Under the cap — record it.
    Emit,
    /// Exactly at the cap — emit one truncation marker and record THAT, so the
    /// next call is over the cap rather than at it.
    Marker,
    /// Over the cap — drop silently.
    Silent,
}

pub(super) fn log_action(buffered: usize) -> LogAction {
    match buffered.cmp(&MAX_CAPTURED_LOGS) {
        std::cmp::Ordering::Less => LogAction::Emit,
        std::cmp::Ordering::Equal => LogAction::Marker,
        std::cmp::Ordering::Greater => LogAction::Silent,
    }
}

/// Per-invocation log buffer, shared between the isolate thread (appends via
/// `op_ctx_log`) and the caller (drains it after the run). Newtype so it's
/// uniquely addressable in `OpState`; capped so a runaway loop can't OOM.
pub(super) struct FunctionLogs(pub Arc<std::sync::Mutex<Vec<LogLine>>>);
const MAX_CAPTURED_LOGS: usize = 500;

// Still `(fast)`: the JS-visible args are primitives (no return), so deno_core
// 0.331 requires `(fast)` — and a fast op may take `&mut OpState` as a leading
// special arg to reach the shared log buffer.
#[op2(fast)]
fn op_ctx_log(state: &mut OpState, #[string] level: &str, #[string] message: &str) {
    // `name` is the field the observability layer's `EventVisitor` promotes to
    // the event name, so these land in ClickHouse as an identifiable
    // `function_log` rather than under the message's own text. `log_level`
    // survives the trip too — the stored event record keeps fields and an
    // error flag, not the tracing `Level`, so warn and info would otherwise be
    // indistinguishable once written.
    //
    // Nothing here names the app, function or invocation: that context comes
    // from the enclosing span opened by `run_with_runtime`, which `run` carries
    // onto the isolate thread. Without that span these events are dropped
    // entirely — `SpanCollectorLayer::on_event` returns early when there is no
    // current span.
    // The emit is capped by the SAME budget as the returned buffer, and that
    // ordering matters. With a span now enclosing these, every event is
    // retained in span extensions and serialised into one `event_data` blob at
    // close — so an uncapped emit turns `for (i=0;i<100000;i++) console.log()`
    // into unbounded per-invocation memory and a single enormous row.
    // Observability needs hard size caps (product-context is explicit), and
    // reusing the existing limit means the buffer and the span agree about
    // what was kept.
    let count = state
        .try_borrow::<FunctionLogs>()
        .and_then(|FunctionLogs(buf)| buf.lock().ok().map(|v| v.len()));
    // Absent buffer or poisoned mutex: drop the line rather than emit it
    // uncapped. Unreachable in production (`run` always inserts `FunctionLogs`),
    // and a poisoned lock means another thread panicked mid-push — at which
    // point an uncapped emit is the wrong direction to fail in.
    let Some(count) = count else {
        return;
    };
    let action = log_action(count);
    if action == LogAction::Silent {
        return;
    }
    if action == LogAction::Marker {
        // One marker, not silence: a truncated log that says so is debuggable,
        // one that just stops is misleading.
        //
        // The marker is PUSHED as well as emitted, and that is what makes the
        // cap a cap. Returning without pushing left the buffer parked at
        // exactly MAX forever, so `count == MAX` on every subsequent call and
        // the marker re-fired per suppressed line — 99,500 identical warn
        // events for a 100k-line loop, each retained in the span until close.
        // Pushing takes the length to MAX + 1, so every later call takes the
        // silent `>` branch above, and the buffer the app receives ends by
        // saying it was truncated instead of just stopping.
        let marker = format!("… further output suppressed after {MAX_CAPTURED_LOGS} lines");
        tracing::warn!(
            target: "custom_app_function",
            name = "function_log",
            log_level = "warn",
            "{marker}"
        );
        if let Some(FunctionLogs(buf)) = state.try_borrow::<FunctionLogs>()
            && let Ok(mut v) = buf.lock()
        {
            v.push(LogLine {
                level: "warn".to_string(),
                message: marker,
            });
        }
        return;
    }

    match level {
        "warn" => {
            tracing::warn!(target: "custom_app_function", name = "function_log", log_level = "warn", "{message}")
        }
        "error" => {
            tracing::error!(target: "custom_app_function", name = "function_log", log_level = "error", "{message}")
        }
        _ => {
            tracing::info!(target: "custom_app_function", name = "function_log", log_level = "info", "{message}")
        }
    }
    if let Some(FunctionLogs(buf)) = state.try_borrow::<FunctionLogs>()
        && let Ok(mut v) = buf.lock()
    {
        v.push(LogLine {
            level: level.to_string(),
            message: message.to_string(),
        });
    }
}

#[op2]
#[string]
async fn op_ctx_query(
    state: Rc<RefCell<OpState>>,
    #[string] sql: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::Query { sql, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.query", result))
}

#[op2]
#[string]
async fn op_ctx_query_stream(
    state: Rc<RefCell<OpState>>,
    #[string] sql: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::QueryStream { sql, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.queryStream", result))
}

#[op2]
#[string]
async fn op_ctx_fetch(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    #[string] init_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let init: serde_json::Value =
        serde_json::from_str(&init_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::Fetch { url, init, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.fetch", result))
}

#[op2]
#[string]
async fn op_ctx_warehouse(
    state: Rc<RefCell<OpState>>,
    #[string] op: String,
    #[string] payload_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let payload: serde_json::Value =
        serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::WarehouseWrite { op, payload, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.warehouse", result))
}

/// `ctx.tx` — bridge to `FunctionHost::tx`.
///
/// One op for all five verbs (`begin`/`query`/`exec`/`commit`/`rollback`), same
/// as `op_ctx_warehouse`. The transaction handle never crosses this boundary —
/// the isolate only ever holds the integer id `begin` returns, so a script
/// cannot fabricate a connection, only name one it was given.
#[op2]
#[string]
async fn op_ctx_tx(
    state: Rc<RefCell<OpState>>,
    #[string] op: String,
    #[string] payload_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let payload: serde_json::Value =
        serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::Tx { op, payload, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.tx", result))
}

/// `ctx.oltp.{query,exec}` — bridge to `FunctionHost::oltp`. Single-op
/// dispatcher shaped like `op_ctx_warehouse`: `op` is the verb, `payload_json`
/// carries `{ sql, params? }`. The app's own per-org OLTP schema is derived
/// host-side from the invoking app's slug (the manifest only gates), so no
/// database name — and no app-chosen target — crosses this boundary.
#[op2]
#[string]
async fn op_ctx_oltp(
    state: Rc<RefCell<OpState>>,
    #[string] op: String,
    #[string] payload_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let payload: serde_json::Value =
        serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::Oltp { op, payload, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.oltp", result))
}

/// `ctx.secrets.set(key, value)` — bridge to `FunctionHost::secrets_set`.
#[op2]
#[string]
async fn op_ctx_secrets_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::SecretsSet { key, value, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.secrets.set", result))
}

/// `ctx.email.send(input)` — bridge to `FunctionHost::send_email`. `input` is
/// the JS payload object, JSON-stringified by the bootstrap `__wrapOp`.
#[op2]
#[string]
async fn op_ctx_email_send(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let input: serde_json::Value =
        serde_json::from_str(&input_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::SendEmail { input, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.email.send", result))
}

/// `ctx.storage.*` — bridge to `FunctionHost::storage`. `op` selects the
/// operation ("getUploadUrl" / "getDownloadUrl" / "put" / "get" / "head" /
/// "list" / "delete" / "copy"), `payload` carries its args (JSON-stringified by
/// `__wrapOp`).
#[op2]
#[string]
async fn op_ctx_storage(
    state: Rc<RefCell<OpState>>,
    #[string] op: String,
    #[string] payload_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let payload: serde_json::Value =
        serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::Storage { op, payload, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.storage", result))
}

#[op2]
#[string]
async fn op_ctx_org_people(state: Rc<RefCell<OpState>>) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let (reply, rx) = oneshot::channel();
    // No payload: the org comes from the host. A function that could name its
    // own org here would be a manifest pointing at another tenant's roster.
    tx.send(HostCall::OrgPeople { reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.org.people", result))
}

#[op2]
#[string]
async fn op_ctx_org_places(state: Rc<RefCell<OpState>>) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let (reply, rx) = oneshot::channel();
    // No payload: the org comes from the host. A function that could name its
    // own org here would be a manifest pointing at another tenant's roster.
    tx.send(HostCall::OrgPlaces { reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.org.places", result))
}

#[op2]
#[string]
async fn op_ctx_org_assignments(state: Rc<RefCell<OpState>>) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let (reply, rx) = oneshot::channel();
    // No payload: the org comes from the host. A function that could name its
    // own org here would be a manifest pointing at another tenant's roster.
    tx.send(HostCall::OrgAssignments { reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.org.assignments", result))
}

#[op2]
#[string]
async fn op_ctx_semantic_query(
    state: Rc<RefCell<OpState>>,
    #[string] spec_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let spec: serde_json::Value = serde_json::from_str(&spec_json)
        .map_err(|e| JsErrorBox::generic(format!("invalid semantic query spec: {e}")))?;
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::SemanticQuery { spec, reply })
        .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.semantic.query", result))
}

#[op2]
#[string]
async fn op_ctx_airway_run(
    state: Rc<RefCell<OpState>>,
    #[string] pipeline_ref: String,
    #[string] vars_json: String,
) -> Result<String, JsErrorBox> {
    check_cancelled(&state)?;
    let tx = state
        .borrow()
        .borrow::<mpsc::UnboundedSender<HostCall>>()
        .clone();
    let variables: serde_json::Value =
        serde_json::from_str(&vars_json).unwrap_or(serde_json::Value::Null);
    let (reply, rx) = oneshot::channel();
    tx.send(HostCall::AirwayRun {
        pipeline_ref,
        variables,
        reply,
    })
    .map_err(|_| JsErrorBox::generic("function host unavailable"))?;
    let result = rx
        .await
        .map_err(|_| JsErrorBox::generic("function host dropped the request"))?;
    Ok(reply_json("ctx.airway.run", result))
}

/// Second cancellation layer (design doc §11.4): checked at the entry of
/// every `ctx.*` op so an in-flight or about-to-start host call fails fast
/// once cancellation has been observed, rather than only relying on
/// `terminate_execution` (which interrupts JS execution but not an
/// already-dispatched host call).
fn check_cancelled(state: &Rc<RefCell<OpState>>) -> Result<(), JsErrorBox> {
    if state
        .borrow()
        .borrow::<Arc<AtomicBool>>()
        .load(Ordering::Relaxed)
    {
        return Err(JsErrorBox::generic("function was cancelled"));
    }
    Ok(())
}

/// Encode a host reply as the JSON envelope the bootstrap `__wrapOp` reads:
/// either the value itself, or `{ __oxyError, message }` on failure.
///
/// `what` is the surface name (`ctx.query`, `ctx.oltp`, …) and this is its ONE
/// owner: host methods, the transaction registry and the Postgres connector all
/// return BARE messages, and the prefix is added here. They used to spell it
/// themselves as well, which rendered `ctx.oltp: ctx.oltp query: query failed:
/// ctx.tx: db error` — the surface named three times, the cause not once. The
/// connector is the reason this has to be a rule rather than a habit: it backs
/// both `ctx.tx` and `ctx.oltp` and cannot know which one called it.
fn reply_json(what: &str, result: Result<serde_json::Value, String>) -> String {
    match result {
        Ok(value) => value.to_string(),
        Err(message) => serde_json::json!({
            "__oxyError": "HostError",
            "message": format!("{what}: {message}"),
        })
        .to_string(),
    }
}

/// HMAC over `data` with `key`, both read as UTF-8.
///
/// UTF-8 only, deliberately: every webhook scheme in the wild signs a UTF-8
/// base string with a UTF-8 secret (GitHub signs the body, Slack `v0:ts:body`,
/// Stripe `ts.body`). A binary key or payload would need an encoding knob per
/// argument, which is surface nobody has asked for.
fn hmac_digest(algorithm: &str, key: &str, data: &str) -> Result<Vec<u8>, JsErrorBox> {
    use hmac::{Hmac, KeyInit, Mac};
    // An unset or empty secret must not silently become a usable key: an
    // attacker can sign with "" as easily as we can. This is enforced here, not
    // only in the JS wrapper, because the artifact shares a global with the
    // bootstrap and can reach `Deno.core.ops` directly.
    if key.is_empty() {
        return Err(JsErrorBox::generic(
            "ctx.crypto: `key` must not be empty — check the secret is set",
        ));
    }
    let bad_key = || JsErrorBox::generic("ctx.crypto: key is not valid for this algorithm");
    match algorithm {
        "sha256" => {
            let mut mac =
                Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).map_err(|_| bad_key())?;
            mac.update(data.as_bytes());
            Ok(mac.finalize().into_bytes().to_vec())
        }
        "sha512" => {
            let mut mac =
                Hmac::<sha2::Sha512>::new_from_slice(key.as_bytes()).map_err(|_| bad_key())?;
            mac.update(data.as_bytes());
            Ok(mac.finalize().into_bytes().to_vec())
        }
        other => Err(JsErrorBox::generic(format!(
            "ctx.crypto: unknown algorithm '{other}' (expected 'sha256' or 'sha512')"
        ))),
    }
}

fn encode_digest(bytes: &[u8], encoding: &str) -> Result<String, JsErrorBox> {
    use base64::Engine as _;
    match encoding {
        "hex" => Ok(hex::encode(bytes)),
        "base64" => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
        other => Err(JsErrorBox::generic(format!(
            "ctx.crypto: unknown encoding '{other}' (expected 'hex' or 'base64')"
        ))),
    }
}

/// `ctx.crypto.hmac` — sign. For talking TO an API that requires a signed
/// request; the inverse direction from `verifyHmac`.
#[op2]
#[string]
fn op_ctx_hmac(
    #[string] algorithm: &str,
    #[string] key: &str,
    #[string] data: &str,
    #[string] encoding: &str,
) -> Result<String, JsErrorBox> {
    encode_digest(&hmac_digest(algorithm, key, data)?, encoding)
}

/// `ctx.crypto.verifyHmac` — the reason this op exists. The comparison is
/// constant-time via [`constant_time_eq`] below; it does NOT use
/// `Mac::verify_slice`, because the provided signature has to be decoded from
/// hex/base64 first and a decode failure must reject rather than throw.
///
/// **A signature that will not decode returns `false`, it does not throw.**
/// That string is attacker-controlled: throwing would turn a forged request
/// into a 500 and an alert, instead of a clean rejection. An unknown
/// `algorithm` or `encoding` DOES throw, because those come from the app
/// author, not the caller.
///
/// The app strips any provider prefix first (`sha256=`, `v0=`) and passes the
/// bare digest — prefix formats are per-provider and do not belong in the
/// platform.
#[op2(fast)]
fn op_ctx_verify_hmac(
    #[string] algorithm: &str,
    #[string] key: &str,
    #[string] data: &str,
    #[string] signature: &str,
    #[string] encoding: &str,
) -> Result<bool, JsErrorBox> {
    use base64::Engine as _;
    // Validate author-supplied inputs first so a bad algorithm throws even when
    // the signature is also junk.
    let expected = hmac_digest(algorithm, key, data)?;
    let provided = match encoding {
        "hex" => hex::decode(signature).ok(),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(signature)
            .ok(),
        other => {
            return Err(JsErrorBox::generic(format!(
                "ctx.crypto: unknown encoding '{other}' (expected 'hex' or 'base64')"
            )));
        }
    };
    let Some(provided) = provided else {
        return Ok(false);
    };
    Ok(constant_time_eq(&expected, &provided))
}

/// `ctx.crypto.timingSafeEqual` — for a plain shared secret carried in a
/// header, where there is no HMAC to verify. Without it an author writes
/// `a === b`, which leaks the secret one byte at a time.
#[op2(fast)]
fn op_ctx_timing_safe_equal(#[string] a: &str, #[string] b: &str) -> bool {
    // An empty side means "absent" — an unset secret, or a header the caller
    // omitted. `constant_time_eq(b"", b"")` is true, so without this the
    // documented pattern
    // `timingSafeEqual(req.headers[...], ctx.env.SECRET)` AUTHORIZES when the
    // secret was never configured and the attacker sends nothing. Fail closed.
    if a.is_empty() || b.is_empty() {
        return false;
    }
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// Length is not secret here (a digest length is fixed by the algorithm, and a
/// shared secret's length is not the part worth protecting), but the byte
/// comparison must not short-circuit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

deno_core::extension!(
    oxy_functions_ext,
    ops = [
        op_ctx_log,
        op_ctx_query,
        op_ctx_query_stream,
        op_ctx_fetch,
        op_ctx_warehouse,
        op_ctx_secrets_set,
        op_ctx_semantic_query,
        op_ctx_airway_run,
        op_ctx_email_send,
        op_ctx_storage,
        op_ctx_org_people,
        op_ctx_org_places,
        op_ctx_org_assignments,
        op_ctx_tx,
        op_ctx_oltp,
        op_ctx_hmac,
        op_ctx_verify_hmac,
        op_ctx_timing_safe_equal,
    ],
);

/// Bootstrap script: polyfills the minimal `Response` the function author's
/// code expects, and assembles `globalThis.__buildCtx` from the host ops.
const BOOTSTRAP_JS: &str = r#"
class OxyResponse {
  constructor(body, init) {
    this.body = body ?? "";
    this.status = (init && init.status) || 200;
    this.headers = (init && init.headers) || {};
  }
  static json(value, init) {
    return new OxyResponse(JSON.stringify(value), {
      status: (init && init.status) || 200,
      headers: Object.assign({ "content-type": "application/json" }, init && init.headers),
    });
  }
}
globalThis.Response = OxyResponse;

// Base64. This isolate is bare deno_core — no `deno_web`, so none of the Web
// binary helpers exist, and V8 here predates `Uint8Array.prototype.toBase64`.
// Without these an author literally cannot produce the base64 that
// `ctx.email.send` attachments and `ctx.storage.put({encoding:"base64"})`
// require: `btoa` was simply `undefined`.
//
// These follow WHATWG semantics so that a helper unit-tested under Node behaves
// identically here. For BYTES use `bytesToBase64` from `@oxy-hq/sdk`, which is
// plain bundled JS and therefore the same function everywhere.
const __B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
// Reverse lookup. `__B64.indexOf(ch)` is a 64-char scan per input character;
// over the ~13.3 MB of base64 a 10 MiB attachment carries that is millions of
// scans inside a wall-clock-capped invocation.
const __B64R = new Uint8Array(256).fill(255);
for (let __i = 0; __i < 64; __i++) __B64R[__B64.charCodeAt(__i)] = __i;
// Accumulate into array segments rather than `out += c` per byte, which would
// allocate one rope node per byte at exactly the sizes this feature targets.
const __B64_CHUNK = 8192;

globalThis.btoa = (input) => {
  if (input instanceof ArrayBuffer || ArrayBuffer.isView(input)) {
    // The spec would ToString this to "37,80,68,70" and cheerfully encode the
    // wrong bytes. Refuse: a loud error beats a silently corrupt file, and the
    // named helper does the right thing.
    throw new TypeError(
      "btoa: expected a string. For bytes use bytesToBase64() from @oxy-hq/sdk"
    );
  }
  const s = String(input);
  const parts = [];
  let buf = "";
  for (let i = 0; i < s.length; i += 3) {
    const c0 = s.charCodeAt(i);
    const c1 = i + 1 < s.length ? s.charCodeAt(i + 1) : 0;
    const c2 = i + 2 < s.length ? s.charCodeAt(i + 2) : 0;
    if (c0 > 0xff || c1 > 0xff || c2 > 0xff) {
      // Same failure as a browser: btoa cannot carry UTF-8. Point at the way
      // out rather than emitting mojibake.
      throw new TypeError(
        "btoa: input contains characters outside the Latin1 range; for text " +
        "pass it directly with { encoding: \"utf8\" }"
      );
    }
    const n = (c0 << 16) | (c1 << 8) | c2;
    buf += __B64[(n >> 18) & 63] + __B64[(n >> 12) & 63]
      + (i + 1 < s.length ? __B64[(n >> 6) & 63] : "=")
      + (i + 2 < s.length ? __B64[n & 63] : "=");
    if (buf.length >= __B64_CHUNK) { parts.push(buf); buf = ""; }
  }
  parts.push(buf);
  return parts.join("");
};

globalThis.atob = (input) => {
  let s = String(input).replace(/[ \t\n\f\r]/g, "");
  // Strip padding BEFORE validating, and only when the length is a multiple of
  // 4 — that is what the spec does. Breaking out of the decode loop on the
  // first "=" instead would silently TRUNCATE: atob(chunkA + chunkB) where
  // chunkA ends in padding would return a short buffer and report success.
  if (s.length % 4 === 0) {
    let pad = 0;
    while (pad < 2 && s.charCodeAt(s.length - 1) === 61 /* = */) {
      s = s.slice(0, -1);
      pad++;
    }
  }
  if (s.indexOf("=") >= 0) {
    throw new TypeError("atob: '=' may only appear as trailing padding");
  }
  if (s.length % 4 === 1) throw new TypeError("atob: invalid base64 length");
  const parts = [];
  let chunk = [];
  let buf = 0;
  let bits = 0;
  for (let i = 0; i < s.length; i++) {
    const code = s.charCodeAt(i);
    const v = code < 256 ? __B64R[code] : 255;
    if (v === 255) throw new TypeError("atob: invalid base64 character '" + s[i] + "'");
    buf = (buf << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      chunk.push((buf >> bits) & 0xff);
      if (chunk.length >= __B64_CHUNK) {
        parts.push(String.fromCharCode.apply(null, chunk));
        chunk = [];
      }
    }
  }
  if (chunk.length) parts.push(String.fromCharCode.apply(null, chunk));
  return parts.join("");
};

// Wire the console developers reach for reflexively into the same host log
// sink as ctx.log — captured per-invocation and sent back with the response.
const __fmt = (a) => {
  if (typeof a === "string") return a;
  try {
    return JSON.stringify(a ?? null);
  } catch {
    // Circular refs / BigInt / etc. — native console.log tolerates these, so a
    // format failure must never fail the whole invocation.
    return String(a);
  }
};
const __log = (level) => (...args) => Deno.core.ops.op_ctx_log(level, args.map(__fmt).join(" "));
const __noop = () => {};
globalThis.console = {
  log: __log("info"),
  info: __log("info"),
  debug: __log("info"),
  warn: __log("warn"),
  error: __log("error"),
  trace: __log("info"),
  dir: __log("info"),
  assert: (cond, ...args) => {
    if (!cond) __log("error")("Assertion failed:", ...args);
  },
  // Extras stubbed to no-ops so an author reaching for them can't throw.
  table: __noop,
  group: __noop,
  groupCollapsed: __noop,
  groupEnd: __noop,
  count: __noop,
  countReset: __noop,
  time: __noop,
  timeEnd: __noop,
  timeLog: __noop,
};

function __wrapOp(opName) {
  return async (...args) => {
    const raw = await Deno.core.ops[opName](...args.map((a) =>
      typeof a === "string" ? a : JSON.stringify(a ?? null)
    ));
    const parsed = JSON.parse(raw);
    if (parsed && parsed.__oxyError) {
      const err = new Error(parsed.message);
      err.name = parsed.__oxyError;
      throw err;
    }
    return parsed;
  };
}

globalThis.__buildCtx = (ctxData) => ({
  // `org_id` is a back-compat mirror of `orgId`. The host used to serialize the
  // field snake_cased, so `ctx.user.orgId` — the name the SDK types and the docs
  // have always used — read `undefined`, and a tenant filter written against it
  // silently compared against nothing. The host now emits `orgId`; this keeps
  // any function written against the shipped `org_id` working.
  //
  // Removal is NOT gated on an SDK version. What reads this key is the function
  // source inside an already-published bundle, and a bundle keeps running until
  // someone republishes it — an SDK floor would never come due. The measurable
  // condition is the artifacts themselves: we hold every live build's
  // `functions/*.js` in the build store, so this is removable once no build a
  // live app points at contains `.org_id`.
  user: { ...ctxData.user, org_id: ctxData.user.orgId },
  env: ctxData.env,
  log: (...args) => Deno.core.ops.op_ctx_log("info", args.map(String).join(" ")),
  // Synchronous — pure CPU, so these skip the host-call channel entirely.
  //
  // verifyHmac is the one that matters: with req.headers carrying a signature,
  // an author would otherwise hand-roll HMAC in JS and compare with `===`,
  // which leaks the digest a byte at a time. Strip the provider's prefix
  // ("sha256=", "v0=") before calling — those formats are per-provider.
  crypto: {
    // `key` comes from configuration, never from the request, so an absent one
    // is an author error and throws. Coercing it would sign with the literal
    // "undefined" — a key anyone can guess.
    hmac: ({ algorithm, key, data, encoding }) => {
      if (key == null) throw new TypeError("ctx.crypto.hmac: `key` is required — is the secret set?");
      if (data == null) throw new TypeError("ctx.crypto.hmac: `data` is required");
      return Deno.core.ops.op_ctx_hmac(
        String(algorithm || "sha256"), String(key), String(data), String(encoding || "hex"));
    },
    verifyHmac: ({ algorithm, key, data, signature, encoding }) => {
      if (key == null) throw new TypeError("ctx.crypto.verifyHmac: `key` is required — is the secret set?");
      if (data == null) throw new TypeError("ctx.crypto.verifyHmac: `data` is required");
      // `signature` stays lenient on purpose: it is attacker-controlled, so an
      // absent or malformed one must reject rather than throw.
      return Deno.core.ops.op_ctx_verify_hmac(
        String(algorithm || "sha256"), String(key), String(data),
        String(signature ?? ""), String(encoding || "hex"));
    },
    // Both sides are symmetric here and either may be attacker-controlled (an
    // omitted header) or author-controlled (an unset secret) — we cannot tell
    // which. So an absent side is always `false`, never a throw: false fails
    // closed, and throwing would turn an omitted header into a 500.
    timingSafeEqual: (a, b) =>
      Deno.core.ops.op_ctx_timing_safe_equal(a == null ? "" : String(a), b == null ? "" : String(b)),
  },
  query: __wrapOp("op_ctx_query"),
  // queryStream(sql, opts?) — fetches up to FUNCTION_STREAM_MAX_ROWS rows in
  // one host call, then yields them to the caller in `opts.batchSize`-sized
  // arrays via an async generator. Not a true warehouse cursor (yet); see
  // design doc §11.5.
  queryStream: async function* (sql, opts) {
    const batchSize = (opts && opts.batchSize) || 1000;
    const rows = await __wrapOp("op_ctx_query_stream")(sql);
    for (let i = 0; i < rows.length; i += batchSize) {
      yield rows.slice(i, i + batchSize);
    }
  },
  fetch: __wrapOp("op_ctx_fetch"),
  warehouse: {
    // insert(database, table, rows) / exec(database, sql) /
    // upsert(database, table, rows, conflictColumns) — `op` stays a bare
    // string, the rest of the call is packed into a payload object that
    // __wrapOp JSON-stringifies.
    insert: (database, table, rows) =>
      __wrapOp("op_ctx_warehouse")("insert", { database, table, rows }),
    exec: (database, sql) =>
      __wrapOp("op_ctx_warehouse")("exec", { database, sql }),
    upsert: (database, table, rows, conflictColumns) =>
      __wrapOp("op_ctx_warehouse")("upsert", { database, table, rows, conflictColumns }),
    // query(database, sql) — a READ against a named database. `ctx.query` only
    // ever reaches the project default, so this is the one way an app reads its
    // own OLTP store. Not gated by `destinations`: that allowlist is about
    // writes, and postgres_managed resolves the read-only analyst regardless.
    query: (database, sql) =>
      __wrapOp("op_ctx_warehouse")("query", { database, sql }),
  },
  // tx(database, fn) — run `fn` inside one transaction on a pinned connection.
  // Commits when `fn` resolves, rolls back when it throws, and rethrows the
  // original error either way.
  //
  // The commit/rollback bracket lives HERE rather than in the author's code on
  // purpose: an author who forgets a rollback in a catch block leaves a
  // transaction open holding locks, and the only reliable moment to close it is
  // the one the runtime owns. Statements take bound parameters ($1, $2, …) —
  // never build SQL by concatenating user input.
  tx: async (database, fn) => {
    if (typeof fn !== "function") {
      throw new TypeError("ctx.tx(database, fn): fn must be a function");
    }
    const call = __wrapOp("op_ctx_tx");
    const { id } = await call("begin", { database: String(database) });
    let closed = false;
    // Every method re-checks `closed` so a handle that escapes the callback
    // (stashed on a global, captured by a stray promise) fails loudly instead
    // of addressing whatever transaction now holds that id.
    const live = (what) => {
      if (closed) {
        throw new Error(
          `ctx.tx: this transaction is already finished — ${what} was called after the callback returned`,
        );
      }
    };
    const handle = {
      query: async (sql, params) => {
        live("query");
        // `??` not `||`: both map a real omission to [], but `||` also
        // swallows 0 and "" — handing a wrong-typed argument to the host as
        // "no parameters" and producing a misleading arity error. Anything
        // else is forwarded as-is for the host to reject by name.
        const r = await call("query", { id, sql: String(sql), params: params ?? [] });
        return r.rows;
      },
      exec: async (sql, params) => {
        live("exec");
        const r = await call("exec", { id, sql: String(sql), params: params ?? [] });
        return r.rowCount;
      },
    };
    let result;
    try {
      result = await fn(handle);
    } catch (err) {
      closed = true;
      // Swallow a rollback failure: the connection drops either way, which the
      // server treats as a rollback, and surfacing it would replace the error
      // the author actually needs to see.
      try {
        await call("rollback", { id });
      } catch (_) {}
      throw err;
    }
    closed = true;
    await call("commit", { id });
    return result;
  },
  // oltp.query(sql, params?) / oltp.exec(sql, params?) — read/write the app's
  // OWN per-org OLTP schema (app_<writer>), and nothing else. This is the write
  // half ctx.warehouse cannot give an app on a managed database (that resolves
  // the read-only analyst, which also sees the org's raw_* extracts). The writer
  // is derived host-side from the app's own slug — oxy-app.json's
  // `oltp: { enabled }` only gates access, it never names the target — so no
  // database name crosses the boundary. Each call auto-commits (a failed
  // statement rolls back). Statements take bound parameters ($1, $2, …) — never
  // build SQL by concatenating user input.
  oltp: {
    query: async (sql, params) => {
      const r = await __wrapOp("op_ctx_oltp")("query", {
        sql: String(sql),
        params: params ?? [],
      });
      return r.rows;
    },
    exec: async (sql, params) => {
      const r = await __wrapOp("op_ctx_oltp")("exec", {
        sql: String(sql),
        params: params ?? [],
      });
      return r.rowCount;
    },
  },
  secrets: {
    // set(key, value) — both stay bare strings so they arrive as the op's
    // `#[string]` args (matching op_ctx_airway_run's bare-string pipelineRef).
    set: (key, value) => __wrapOp("op_ctx_secrets_set")(String(key), String(value)),
  },
  email: {
    // send(input) — input is an object; __wrapOp JSON-stringifies it. The host
    // controls `from` (author sets replyTo only). Render templates to `html`
    // with `render` from @oxy-hq/sdk/email before calling this.
    send: (input) => __wrapOp("op_ctx_email_send")(input),
  },
  org: {
    // people() — who is in this org, by name. Read-only, and NOT a contact
    // list: a display name and a role, never an email or a phone. No location —
    // the platform holds none for a member. Includes frontline workers granted
    // to THIS app, tagged `kind: "frontline"`; see the host handler for why the
    // scope is per-app.
    // Naming a colleague is a different need from being able to message them,
    // and only the first one was blocking anything.
    people: () => __wrapOp("op_ctx_org_people")(),
    // places() — the org's locations: hierarchy, status, timezone, and what
    // each integration calls the place. The whole registry; reach is applied
    // by the app, on top.
    places: () => __wrapOp("op_ctx_org_places")(),
    // assignments() — who holds which position where, for the people who can
    // reach this app. The roster, read.
    assignments: () => __wrapOp("op_ctx_org_assignments")(),
  },
  storage: {
    // The app's asset store — uploaded files AND generated ones, one silo.
    // getUploadUrl/getDownloadUrl mint presigned URLs the BROWSER talks to
    // directly (bytes never cross this boundary); put/get/head/list/delete/copy
    // are server-side. `op` stays a bare string and the rest is packed into a
    // payload object that __wrapOp JSON-stringifies (matching ctx.warehouse).
    getUploadUrl: (opts) => __wrapOp("op_ctx_storage")("getUploadUrl", opts || {}),
    getDownloadUrl: (key, opts) =>
      __wrapOp("op_ctx_storage")("getDownloadUrl", Object.assign({ key: String(key) }, opts || {})),
    // put(pathname, body, opts) — body is a string; pass
    // { encoding: "base64" } for binary assets (PDF/PNG/Parquet).
    put: (pathname, body, opts) =>
      __wrapOp("op_ctx_storage")(
        "put",
        Object.assign({ pathname: String(pathname), body: String(body) }, opts || {})
      ),
    get: (key, opts) =>
      __wrapOp("op_ctx_storage")("get", Object.assign({ key: String(key) }, opts || {})),
    head: (key) => __wrapOp("op_ctx_storage")("head", { key: String(key) }),
    list: (opts) => __wrapOp("op_ctx_storage")("list", opts || {}),
    // delete(key) or delete([key, ...])
    delete: (keyOrKeys) =>
      __wrapOp("op_ctx_storage")(
        "delete",
        Array.isArray(keyOrKeys)
          ? { keys: keyOrKeys.map(String) }
          : { key: String(keyOrKeys) }
      ),
    copy: (fromKey, toPathname, opts) =>
      __wrapOp("op_ctx_storage")(
        "copy",
        Object.assign({ fromKey: String(fromKey), toPathname: String(toPathname) }, opts || {})
      ),
  },
  semantic: { query: __wrapOp("op_ctx_semantic_query") }, // spec is JSON-stringified by __wrapOp
  airway: {
    // run(pipelineRef, variables?) — pipelineRef stays a bare string,
    // variables is JSON-stringified by __wrapOp.
    run: (pipelineRef, variables) =>
      __wrapOp("op_ctx_airway_run")(String(pipelineRef), variables ?? null),
  },
});
"#;

/// How long to wait for the isolate thread to actually exit after a wall-clock
/// timeout (or cancel) has terminated execution, before giving up and returning
/// `Timeout` regardless. Bounds the worst case where the isolate is parked in a
/// not-yet-returned host call that `terminate_execution` can't interrupt.
const TIMEOUT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `export default async (req, ctx) => Response` from a bundled ESM
/// artifact to completion, bridging `ctx.*` calls to `host`.
///
/// `cancel` resolving (the client disconnected, or the dashboard cancel
/// flag was observed) terminates the isolate promptly via
/// `terminate_execution`. `timeout` is enforced here (not by the caller
/// wrapping this future in `tokio::time::timeout`): on elapse the isolate is
/// terminated the same way as a cancel, and we wait up to [`TIMEOUT_GRACE`]
/// for the isolate thread to actually exit before returning — so in the common
/// case the OS thread + V8 isolate never outlive this call. If the isolate is
/// wedged in a not-yet-returned host call (which `terminate_execution` cannot
/// interrupt), we return `Timeout` after the grace period and let that thread
/// unwind on its own once the (individually bounded) host op completes.
/// The triggering HTTP request, as the function's `req` argument sees it.
///
/// Bundled rather than passed as three positional parameters: `run` is already
/// at the arity `internal-docs/backend-architecture.md` caps out at.
///
/// `headers` has already been filtered by the caller — the runtime does not
/// re-check it, so anything placed here reaches app code verbatim.
pub struct FnRequest {
    pub method: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl FnRequest {
    /// A request carrying only a body — no real HTTP request behind it. This is
    /// what the schedule / Airway / manual-run paths synthesise, and what a test
    /// exercising handler behaviour rather than request plumbing wants.
    pub fn from_body(body: Vec<u8>) -> Self {
        Self {
            method: "POST".to_string(),
            headers: std::collections::BTreeMap::new(),
            body,
        }
    }
}

pub async fn run(
    artifact_js: String,
    ctx: InvocationCtx,
    req: FnRequest,
    host: std::sync::Arc<dyn FunctionHost>,
    mut cancel: oneshot::Receiver<()>,
    timeout: std::time::Duration,
    // Shared with the caller: the isolate appends `console.*`/`ctx.log` here and
    // the caller drains it after (surfaced back to the app, not just tracing).
    logs: Arc<std::sync::Mutex<Vec<LogLine>>>,
    // The span the isolate thread runs inside. Must be PARENTLESS — see the
    // comment at the spawn below for why a clone of the caller's span is the
    // wrong thing here.
    isolate_span: tracing::Span,
) -> Result<FnResponse, RuntimeError> {
    let (call_tx, mut call_rx) = mpsc::unbounded_channel::<HostCall>();
    let (done_tx, done_rx) = oneshot::channel::<Result<FnResponse, RuntimeError>>();
    let (handle_tx, handle_rx) = oneshot::channel::<deno_core::v8::IsolateHandle>();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Isolate runs on its own thread with a current-thread runtime.
    //
    // `tracing`'s notion of "the current span" is THREAD-LOCAL, so a span
    // opened by the caller does not follow execution across this `spawn`.
    // Every `ctx.log()` / `console.*` line is emitted from the isolate thread,
    // so without a span entered *on that thread* they all fire with no current
    // span — and the observability layer drops an event that has no span.
    //
    // **`isolate_span` is deliberately PARENTLESS, not a clone of the caller's.**
    // A clone bumps the registry refcount and `on_close` fires only at zero, and
    // this thread outlives `run` on two paths: `Cancelled`, and the grace-expiry
    // branch that lets a detached thread unwind on its own. A clone would
    // therefore pin the caller's invocation span open — exporting it late, or
    // never if the thread parks in a host op forever, and inflating its
    // `duration_ns` (measured at close) to the leaked thread's whole lifetime.
    // The invocations that time out are exactly the ones worth having.
    //
    // The cost is that logs land on a sibling span rather than a child: they
    // are correlated by the `invocation_id` field both spans carry, not by the
    // trace tree. That is the right trade — a late-but-correct sibling beats a
    // parent whose duration is a lie.
    let thread = std::thread::Builder::new()
        .name("oxy-function".into())
        .spawn({
            let cancelled = cancelled.clone();
            move || {
                // Sync closure, so holding the guard for the whole thread body
                // is correct (the "never hold across an await" rule is about
                // async fns; everything below runs inside `block_on`).
                let _span_guard = isolate_span.enter();
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = done_tx.send(Err(RuntimeError::Internal(format!(
                            "failed to build isolate runtime: {e}"
                        ))));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    let result =
                        execute_isolate(artifact_js, ctx, req, call_tx, handle_tx, cancelled, logs)
                            .await;
                    let _ = done_tx.send(result);
                });
            }
        });
    if let Err(e) = thread {
        return Err(RuntimeError::Internal(format!(
            "failed to spawn isolate thread: {e}"
        )));
    }

    // Grab the isolate handle so cancellation can terminate execution even
    // when the isolate is stuck in a compute-only loop (design doc §11.4).
    let isolate_handle = handle_rx.await.ok();

    tokio::pin!(done_rx);
    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);
    // Grace timer, armed only after the wall-clock timeout fires. Starts
    // already-elapsed but is gated off by `if timed_out` until then.
    let grace = tokio::time::sleep(std::time::Duration::ZERO);
    tokio::pin!(grace);
    let mut timed_out = false;
    loop {
        tokio::select! {
            // Cancellation: terminate the isolate; the thread's done_tx will
            // then deliver an error/partial which we map to Cancelled.
            _ = &mut cancel => {
                cancelled.store(true, Ordering::Relaxed);
                if let Some(h) = &isolate_handle {
                    h.terminate_execution();
                }
                return Err(RuntimeError::Cancelled);
            }
            // Wall-clock timeout: terminate the isolate the same way as
            // cancel, then keep looping until the isolate thread actually
            // exits (`done_rx` below) so the OS thread + V8 isolate don't
            // outlive this call — but only up to `TIMEOUT_GRACE` (armed here),
            // after which we give up waiting (see the grace branch).
            _ = &mut sleep, if !timed_out => {
                timed_out = true;
                cancelled.store(true, Ordering::Relaxed);
                if let Some(h) = &isolate_handle {
                    h.terminate_execution();
                }
                grace
                    .as_mut()
                    .reset(tokio::time::Instant::now() + TIMEOUT_GRACE);
            }
            // Grace expired: the isolate did not exit within `TIMEOUT_GRACE`
            // of termination. This only happens if it's parked in a host call
            // that hasn't returned (terminate_execution can't interrupt a
            // pending host await — there's no running JS to throw into). Host
            // ops are individually bounded (ctx.fetch carries connect+total
            // timeouts; connectors carry their own), so rather than block this
            // request indefinitely we return Timeout and let the detached
            // thread unwind on its own once the host op completes — its
            // `done_tx`/`call_tx` sends then no-op against our dropped ends.
            _ = &mut grace, if timed_out => {
                return Err(RuntimeError::Timeout);
            }
            // Service a host call from the isolate. Spawn so concurrent
            // ctx.* awaits inside one function don't serialize. Each call runs
            // under its own span, a child of the invocation span `run` is
            // being polled in — `tokio::spawn` carries no span by itself, so
            // without `.instrument` every warehouse query and outbound fetch
            // was a root trace of its own.
            maybe_call = call_rx.recv() => {
                match maybe_call {
                    Some(call) => {
                        let host = host.clone();
                        let (span, kind) = host_call_span(&call);
                        tokio::spawn(
                            async move {
                                let (reply, result) = dispatch_host_call(call, host).await;
                                record_host_call_outcome(kind, &result);
                                let _ = reply.send(result);
                            }
                            .instrument(span),
                        );
                    }
                    None => { /* sender dropped; isolate is finishing */ }
                }
            }
            done = &mut done_rx => {
                if timed_out {
                    return Err(RuntimeError::Timeout);
                }
                return done.unwrap_or_else(|_| {
                    Err(RuntimeError::Internal("isolate thread vanished".into()))
                });
            }
        }
    }
}

/// Which reply shape [`record_host_call_outcome`] should read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostCallKind {
    Query,
    Fetch,
    Other,
}

/// The span one host op runs under. Named and attributed on the OpenTelemetry
/// semantic conventions where one exists (`db.query`,
/// `http.client.request`), `oxy.*` otherwise — see `host_call_attrs` for
/// what is deliberately *not* recorded.
fn host_call_span(call: &HostCall) -> (tracing::Span, HostCallKind) {
    use super::host_call_attrs::{HOST_CALL_TARGET, db_query_summary, fetch_target};
    use tracing::field::Empty;
    match call {
        HostCall::Query { sql, .. } | HostCall::QueryStream { sql, .. } => {
            let s = db_query_summary(sql);
            let streaming = matches!(call, HostCall::QueryStream { .. });
            let span = tracing::info_span!(target: HOST_CALL_TARGET,
                "db.query",
                db.operation.name = %s.verb,
                db.collection.name = %s.table,
                oxy.op = if streaming { "query_stream" } else { "query" },
                db.response.returned_rows = Empty,
                oxy.truncated = Empty,
                otel.status_code = Empty,
                error.type = Empty,
            );
            (span, HostCallKind::Query)
        }
        HostCall::Fetch { url, init, .. } => {
            let t = fetch_target(url);
            let method = init
                .get("method")
                .and_then(|m| m.as_str())
                .map(str::to_ascii_uppercase)
                .unwrap_or_else(|| "GET".to_string());
            let span = tracing::info_span!(target: HOST_CALL_TARGET,
                "http.client.request",
                otel.kind = "client",
                http.request.method = %method,
                url.scheme = %t.scheme,
                server.address = %t.host,
                server.port = t.port,
                http.response.status_code = Empty,
                otel.status_code = Empty,
                error.type = Empty,
            );
            (span, HostCallKind::Fetch)
        }
        HostCall::SemanticQuery { spec, .. } => {
            let measures = spec
                .get("measures")
                .and_then(|m| m.as_array())
                .map(|m| m.len());
            let span = tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.semantic.query",
                oxy.semantic.measures = measures,
                otel.status_code = Empty,
                error.type = Empty,
            );
            (span, HostCallKind::Other)
        }
        HostCall::AirwayRun { pipeline_ref, .. } => (
            tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.airway.run",
                oxy.airway.pipeline = %pipeline_ref,
                otel.status_code = Empty,
                error.type = Empty,
            ),
            HostCallKind::Other,
        ),
        HostCall::WarehouseWrite { op, .. } => {
            let span = tracing::info_span!(target: HOST_CALL_TARGET,
                "db.query",
                db.system = "airhouse",
                db.operation.name = %op,
                db.namespace = Empty,
                db.query.summary = Empty,
                db.collection.name = Empty,
                otel.status_code = Empty,
                error.type = Empty,
            );
            (span, HostCallKind::Other)
        }
        HostCall::Tx { op, .. } => {
            let span = tracing::info_span!(target: HOST_CALL_TARGET,
                "db.transaction",
                db.operation.name = %op,
                db.namespace = Empty,
                db.query.summary = Empty,
                db.collection.name = Empty,
                otel.status_code = Empty,
                error.type = Empty,
            );
            (span, HostCallKind::Other)
        }
        HostCall::Oltp { op, .. } => {
            let span = tracing::info_span!(target: HOST_CALL_TARGET,
                "db.query",
                db.system = "postgres",
                db.operation.name = %op,
                // `db.namespace` / `db.query.summary` / `db.collection.name`
                // are recorded by the host once it has resolved the schema
                // and summarised the SQL (`host::record_db_span`), so the
                // statement is not summarised twice.
                db.namespace = Empty,
                db.query.summary = Empty,
                db.collection.name = Empty,
                otel.status_code = Empty,
                error.type = Empty,
            );
            (span, HostCallKind::Other)
        }
        HostCall::SecretsSet { key, .. } => (
            tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.secrets.set",
                oxy.secret.name = %key,
                otel.status_code = Empty,
                error.type = Empty,
            ),
            HostCallKind::Other,
        ),
        HostCall::SendEmail { input, .. } => {
            let recipients = match input.get("to") {
                Some(serde_json::Value::Array(a)) => a.len(),
                Some(serde_json::Value::String(_)) => 1,
                _ => 0,
            };
            (
                tracing::info_span!(target: HOST_CALL_TARGET,
                    "oxy.email.send",
                    oxy.email.recipients = recipients,
                    otel.status_code = Empty,
                    error.type = Empty,
                ),
                HostCallKind::Other,
            )
        }
        HostCall::Storage { op, .. } => (
            tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.storage",
                oxy.storage.op = %op,
                otel.status_code = Empty,
                error.type = Empty,
            ),
            HostCallKind::Other,
        ),
        HostCall::OrgPeople { .. } => (
            tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.org.people",
                otel.status_code = Empty,
                error.type = Empty,
            ),
            HostCallKind::Other,
        ),
        HostCall::OrgPlaces { .. } => (
            tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.org.places",
                otel.status_code = Empty,
                error.type = Empty,
            ),
            HostCallKind::Other,
        ),
        HostCall::OrgAssignments { .. } => (
            tracing::info_span!(target: HOST_CALL_TARGET,
                "oxy.org.assignments",
                otel.status_code = Empty,
                error.type = Empty,
            ),
            HostCallKind::Other,
        ),
    }
}

/// Run one host op and hand back the reply channel with its result, so the
/// caller can record the outcome on the current span before answering.
async fn dispatch_host_call(
    call: HostCall,
    host: std::sync::Arc<dyn FunctionHost>,
) -> (
    oneshot::Sender<Result<serde_json::Value, String>>,
    Result<serde_json::Value, String>,
) {
    match call {
        HostCall::Query { sql, reply } => (reply, host.query(sql).await),
        HostCall::QueryStream { sql, reply } => (reply, host.query_stream(sql).await),
        HostCall::Fetch { url, init, reply } => (reply, host.fetch(url, init).await),
        HostCall::SemanticQuery { spec, reply } => (reply, host.semantic_query(spec).await),
        HostCall::AirwayRun {
            pipeline_ref,
            variables,
            reply,
        } => (reply, host.airway_run(pipeline_ref, variables).await),
        HostCall::WarehouseWrite { op, payload, reply } => {
            let result = if op == "query" {
                host.warehouse_query(payload).await
            } else {
                host.warehouse_write(op, payload).await
            };
            (reply, result)
        }
        HostCall::Tx { op, payload, reply } => (reply, host.tx(op, payload).await),
        HostCall::Oltp { op, payload, reply } => (reply, host.oltp(op, payload).await),
        HostCall::SecretsSet { key, value, reply } => (reply, host.secrets_set(key, value).await),
        HostCall::SendEmail { input, reply } => (reply, host.send_email(input).await),
        HostCall::Storage { op, payload, reply } => (reply, host.storage(op, payload).await),
        HostCall::OrgPeople { reply } => (reply, host.org_people().await),
        HostCall::OrgPlaces { reply } => (reply, host.org_places().await),
        HostCall::OrgAssignments { reply } => (reply, host.org_assignments().await),
    }
}

/// Record how a host op ended on the current span: an `error.type` facet on
/// failure, rows / truncation for a query, the upstream status for a fetch.
fn record_host_call_outcome(kind: HostCallKind, result: &Result<serde_json::Value, String>) {
    use super::host_call_attrs::{classify_host_error, fetch_status, rows_and_truncated};
    let span = tracing::Span::current();
    match result {
        Err(message) => {
            span.record("otel.status_code", "ERROR");
            span.record("error.type", classify_host_error(message));
        }
        Ok(value) => match kind {
            HostCallKind::Query => {
                let (rows, truncated) = rows_and_truncated(value);
                if let Some(rows) = rows {
                    span.record("db.response.returned_rows", rows);
                }
                if let Some(truncated) = truncated {
                    span.record("oxy.truncated", truncated);
                }
            }
            HostCallKind::Fetch => {
                if let Some(status) = fetch_status(value) {
                    span.record("http.response.status_code", status);
                    if status >= 500 {
                        span.record("otel.status_code", "ERROR");
                        span.record("error.type", "upstream_5xx");
                    }
                }
            }
            HostCallKind::Other => {}
        },
    }
}

/// Body that runs on the isolate thread. Loads + evaluates the module, calls
/// the default export, and returns the parsed `FnResponse`.
async fn execute_isolate(
    artifact_js: String,
    ctx: InvocationCtx,
    req: FnRequest,
    call_tx: mpsc::UnboundedSender<HostCall>,
    handle_tx: oneshot::Sender<deno_core::v8::IsolateHandle>,
    cancelled: Arc<AtomicBool>,
    logs: Arc<std::sync::Mutex<Vec<LogLine>>>,
) -> Result<FnResponse, RuntimeError> {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![oxy_functions_ext::init()],
        ..Default::default()
    });
    let _ = handle_tx.send(runtime.v8_isolate().thread_safe_handle());
    runtime.op_state().borrow_mut().put(call_tx);
    runtime.op_state().borrow_mut().put(cancelled);
    runtime.op_state().borrow_mut().put(FunctionLogs(logs));

    runtime
        .execute_script("oxy:bootstrap", BOOTSTRAP_JS)
        .map_err(|e| RuntimeError::Internal(format!("bootstrap failed: {e}")))?;

    let ctx_json = serde_json::to_string(&ctx)
        .map_err(|e| RuntimeError::Internal(format!("ctx serialize failed: {e}")))?;
    let req_body_str = String::from_utf8_lossy(&req.body);
    let req_json = serde_json::json!({
        "method": req.method,
        "headers": req.headers,
        "body": req_body_str,
    })
    .to_string();

    let specifier = deno_core::resolve_url("oxy:function")
        .map_err(|e| RuntimeError::Internal(format!("bad module specifier: {e}")))?;
    let mod_id = runtime
        .load_side_es_module_from_code(&specifier, artifact_js)
        .await
        .map_err(|e| RuntimeError::Js(format!("module load failed: {e}")))?;
    let eval = runtime.mod_evaluate(mod_id);
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(|e| RuntimeError::Js(e.to_string()))?;
    eval.await
        .map_err(|e| RuntimeError::Js(format!("module evaluation failed: {e}")))?;

    let invoke_script = format!(
        r#"
        (async () => {{
            const ctx = globalThis.__buildCtx({ctx_json});
            const req = {req_json};
            const mod = await import("oxy:function");
            const handler = mod.default;
            if (typeof handler !== "function") {{
                throw new Error("function module has no default export");
            }}
            const res = await handler(req, ctx);
            return {{
                status: (res && res.status) || 200,
                body: res && res.body !== undefined ? String(res.body) : "",
            }};
        }})()
        "#,
    );

    let promise = runtime
        .execute_script("oxy:invoke", invoke_script)
        .map_err(|e| RuntimeError::Js(format!("invoke failed: {e}")))?;
    // `resolve()` alone does NOT pump the event loop — it just returns a future
    // that settles when the promise does. A handler whose promise awaits an
    // async host op (ctx.query/fetch/…) would then hang forever: the op's reply
    // arrives on the broker, but nothing re-polls the isolate to deliver it back
    // into JS, so the promise never resolves (→ wall-clock timeout). Drive the
    // event loop while awaiting, exactly like the module-eval path above does.
    let resolve_fut = Box::pin(runtime.resolve(promise));
    let result = runtime
        .with_event_loop_promise(resolve_fut, deno_core::PollEventLoopOptions::default())
        .await
        .map_err(|e| RuntimeError::Js(e.to_string()))?;

    // deno_core 0.410 removed `JsRuntime::handle_scope()`; `scope!` is the
    // exported replacement (it enters the runtime's main context the same way).
    deno_core::scope!(scope, runtime);
    let local = deno_core::v8::Local::new(scope, result);
    let value: serde_json::Value = deno_core::serde_v8::from_v8(scope, local)
        .map_err(|e| RuntimeError::Internal(format!("result deserialize failed: {e}")))?;
    serde_json::from_value(value)
        .map_err(|e| RuntimeError::Internal(format!("result shape invalid: {e}")))
}

#[cfg(test)]
mod tests {

    /// Every host-op span must carry [`HOST_CALL_TARGET`]: the product
    /// collector keeps them out of the tenant Traces console with an
    /// `oxy::host_call=off` directive, and a span that falls back to the
    /// module target slips past it. A new `HostCall` variant fails the
    /// exhaustive `match` below until it is added to the list.
    #[test]
    fn every_host_call_span_carries_the_platform_only_target() {
        use super::super::host_call_attrs::HOST_CALL_TARGET;
        use tokio::sync::oneshot;
        fn reply() -> oneshot::Sender<Result<serde_json::Value, String>> {
            oneshot::channel().0
        }
        let json = serde_json::Value::Null;
        let calls = vec![
            HostCall::Query {
                sql: "select 1".into(),
                reply: reply(),
            },
            HostCall::QueryStream {
                sql: "select 1".into(),
                reply: reply(),
            },
            HostCall::Fetch {
                url: "https://example.test/".into(),
                init: json.clone(),
                reply: reply(),
            },
            HostCall::SemanticQuery {
                spec: json.clone(),
                reply: reply(),
            },
            HostCall::AirwayRun {
                pipeline_ref: "p".into(),
                variables: json.clone(),
                reply: reply(),
            },
            HostCall::WarehouseWrite {
                op: "query".into(),
                payload: json.clone(),
                reply: reply(),
            },
            HostCall::SecretsSet {
                key: "k".into(),
                value: "v".into(),
                reply: reply(),
            },
            HostCall::SendEmail {
                input: json.clone(),
                reply: reply(),
            },
            HostCall::Storage {
                op: "put".into(),
                payload: json.clone(),
                reply: reply(),
            },
            HostCall::OrgPeople { reply: reply() },
            HostCall::OrgPlaces { reply: reply() },
            HostCall::OrgAssignments { reply: reply() },
            HostCall::Tx {
                op: "begin".into(),
                payload: json.clone(),
                reply: reply(),
            },
            HostCall::Oltp {
                op: "query".into(),
                payload: json,
                reply: reply(),
            },
        ];
        for call in &calls {
            // Exhaustive on purpose: a new variant must be listed above.
            match call {
                HostCall::Query { .. }
                | HostCall::QueryStream { .. }
                | HostCall::Fetch { .. }
                | HostCall::SemanticQuery { .. }
                | HostCall::AirwayRun { .. }
                | HostCall::WarehouseWrite { .. }
                | HostCall::SecretsSet { .. }
                | HostCall::SendEmail { .. }
                | HostCall::Storage { .. }
                | HostCall::OrgPeople { .. }
                | HostCall::OrgPlaces { .. }
                | HostCall::OrgAssignments { .. }
                | HostCall::Tx { .. }
                | HostCall::Oltp { .. } => {}
            }
        }
        // A bare registry enables every span, so `metadata()` is `Some`.
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            for call in &calls {
                let (span, _) = host_call_span(call);
                let meta = span.metadata().expect("span is enabled under a registry");
                assert_eq!(
                    meta.target(),
                    HOST_CALL_TARGET,
                    "span `{}` would leak into the product store",
                    meta.name()
                );
            }
        });
    }

    /// The cap has to actually cap. The first version returned at the marker
    /// without pushing it, so the buffer parked at exactly MAX forever: every
    /// later call saw `Equal`, re-emitted the marker, and `Silent` was
    /// unreachable — 99,500 identical warn events for a 100k-line loop, each
    /// retained in the span until close. Pushing the marker is what advances
    /// the count past MAX and makes the third state reachable.
    #[test]
    fn the_log_cap_emits_exactly_one_marker_then_goes_silent() {
        use super::{LogAction, MAX_CAPTURED_LOGS, log_action};

        assert_eq!(log_action(0), LogAction::Emit);
        assert_eq!(log_action(MAX_CAPTURED_LOGS - 1), LogAction::Emit);
        assert_eq!(log_action(MAX_CAPTURED_LOGS), LogAction::Marker);
        // The marker pushes a line, so the very next call is already over.
        assert_eq!(log_action(MAX_CAPTURED_LOGS + 1), LogAction::Silent);

        // Simulate the loop the cap exists for: 10k lines, one marker.
        let mut buffered = 0usize;
        let mut markers = 0;
        let mut emitted = 0;
        for _ in 0..10_000 {
            match log_action(buffered) {
                LogAction::Emit => {
                    emitted += 1;
                    buffered += 1;
                }
                LogAction::Marker => {
                    markers += 1;
                    buffered += 1;
                }
                LogAction::Silent => {}
            }
        }
        assert_eq!(
            markers, 1,
            "the marker must fire once, not per dropped line"
        );
        assert_eq!(emitted, MAX_CAPTURED_LOGS);
        assert_eq!(buffered, MAX_CAPTURED_LOGS + 1);
    }
    use super::*;

    /// Test host: `ctx.query` returns one row and `ctx.email.send` records what
    /// actually arrived, so a test can assert a payload survived the isolate
    /// boundary byte-for-byte. Everything else is unused.
    #[derive(Default)]
    struct MockHost {
        last_email: std::sync::Mutex<Option<serde_json::Value>>,
        /// Every `ctx.tx` op this host saw, in order — the bootstrap wrapper's
        /// commit/rollback bracket is only observable from here.
        tx_ops: std::sync::Mutex<Vec<String>>,
    }

    impl MockHost {
        fn tx_ops(&self) -> Vec<String> {
            self.tx_ops.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl FunctionHost for MockHost {
        async fn query(&self, _sql: String) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "rows": [{ "x": 1 }], "truncated": false }))
        }
        async fn tx(
            &self,
            op: String,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.tx_ops.lock().unwrap().push(op.clone());
            match op.as_str() {
                "begin" => Ok(serde_json::json!({ "id": 1 })),
                // Echo the params back so a test can assert they crossed the
                // boundary as an array rather than being stringified.
                "query" => Ok(serde_json::json!({
                    "rows": [{ "id": 42, "params": payload.get("params").cloned() }]
                })),
                "exec" => Ok(serde_json::json!({ "rowCount": 1 })),
                "commit" | "rollback" => Ok(serde_json::json!({ "ok": true })),
                other => Err(format!("unexpected tx op '{other}'")),
            }
        }
        async fn send_email(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
            *self.last_email.lock().unwrap() = Some(input);
            Ok(serde_json::json!({ "messageId": "test-message-id" }))
        }
        async fn query_stream(&self, _sql: String) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
        async fn fetch(
            &self,
            _url: String,
            _init: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
        async fn semantic_query(
            &self,
            _spec: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
        async fn airway_run(
            &self,
            _pipeline_ref: String,
            _variables: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
        async fn warehouse_query(
            &self,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            // Echo the database back so a test can prove the name crossed the
            // isolate boundary rather than being defaulted host-side.
            Ok(serde_json::json!({
                "rows": [{ "x": 1 }],
                "truncated": false,
                "db": payload["database"],
            }))
        }
        async fn warehouse_write(
            &self,
            _op: String,
            _payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
        async fn oltp(
            &self,
            op: String,
            payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            // Echo sql/params so a test can prove the call crossed the isolate
            // boundary intact — params as an array, not a stringified blob.
            match op.as_str() {
                "query" => Ok(serde_json::json!({
                    "rows": [{ "sql": payload.get("sql"), "params": payload.get("params") }]
                })),
                "exec" => Ok(serde_json::json!({ "rowCount": 1 })),
                other => Err(format!("unexpected oltp op '{other}'")),
            }
        }
        async fn secrets_set(
            &self,
            _key: String,
            _value: String,
        ) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
        async fn storage(
            &self,
            _op: String,
            _payload: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("unused".into())
        }
    }

    fn test_ctx() -> InvocationCtx {
        InvocationCtx {
            user: CtxUser {
                id: "u".into(),
                email: Some("e@example.com".into()),
                org_id: "o".into(),
                name: None,
                picture: None,
                app_role: None,
                org_role: None,
                teams: Vec::new(),
                kind: CtxIdentityKind::User,
                reach: crate::server::api::operating_graph::reach::Reach::nowhere(),
            },
            env: Default::default(),
        }
    }

    #[test]
    fn reply_json_ok_passes_value_through() {
        let out = reply_json("ctx.query", Ok(serde_json::json!({ "rows": [] })));
        assert_eq!(out, r#"{"rows":[]}"#);
    }

    #[test]
    fn reply_json_err_wraps_as_oxy_error() {
        let out = reply_json("ctx.warehouse", Err("boom".to_string()));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["__oxyError"], "HostError");
        assert_eq!(parsed["message"], "ctx.warehouse: boom");
    }

    #[test]
    fn cancelled_flag_defaults_false() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.load(Ordering::Relaxed));
        flag.store(true, Ordering::Relaxed);
        assert!(flag.load(Ordering::Relaxed));
    }

    fn full_identity() -> CtxUser {
        CtxUser {
            id: "11111111-1111-1111-1111-111111111111".into(),
            email: Some("ada@acme.com".into()),
            org_id: "22222222-2222-2222-2222-222222222222".into(),
            name: Some("Ada Lovelace".into()),
            picture: Some("https://cdn.example/ada.png".into()),
            app_role: Some("admin".into()),
            org_role: Some("member".into()),
            teams: vec![CtxTeam {
                id: "33333333-3333-3333-3333-333333333333".into(),
                name: "Finance".into(),
            }],
            kind: CtxIdentityKind::User,
            reach: crate::server::api::operating_graph::reach::Reach::everywhere(
                "app-admin",
                Vec::new(),
            ),
        }
    }

    /// The wire contract the SDK's `OxyFunctionUser` is typed against. Every key
    /// here is camelCase — `org_id` shipped snake-cased once and made the
    /// documented `ctx.user.orgId` read `undefined`, so the casing is pinned.
    #[test]
    fn ctx_user_serializes_the_documented_camel_case_keys() {
        let json: serde_json::Value = serde_json::to_value(full_identity()).unwrap();
        assert_eq!(json["orgId"], "22222222-2222-2222-2222-222222222222");
        assert_eq!(json["appRole"], "admin");
        assert_eq!(json["orgRole"], "member");
        assert_eq!(json["name"], "Ada Lovelace");
        assert_eq!(json["picture"], "https://cdn.example/ada.png");
        assert_eq!(json["kind"], "user");
        assert_eq!(json["teams"][0]["name"], "Finance");
        assert!(
            json.get("org_id").is_none(),
            "the host serializes orgId; the snake alias is added in __buildCtx, not here"
        );
    }

    #[test]
    fn a_frontline_user_serializes_email_as_null_not_absent_and_not_empty() {
        // The contract app code branches on. `null` is load-bearing three ways:
        // `""` would look like an address to `ctx.email.send` and to any string
        // concat; an ABSENT key reads as `undefined`, which a `=== null` check
        // misses; and only an explicit null lets a function say "this person
        // cannot be emailed" without guessing.
        let json: serde_json::Value = serde_json::to_value(CtxUser {
            id: "11111111-1111-1111-1111-111111111111".into(),
            email: None,
            org_id: "22222222-2222-2222-2222-222222222222".into(),
            name: Some("Maria S.".into()),
            picture: None,
            app_role: None,
            org_role: None,
            teams: Vec::new(),
            kind: CtxIdentityKind::User,
            reach: crate::server::api::operating_graph::reach::Reach::nowhere(),
        })
        .unwrap();
        assert_eq!(
            json.get("email"),
            Some(&serde_json::Value::Null),
            "email must be present and null, never omitted or empty: {json}"
        );
    }

    /// A schedule tick runs under the org owner's `user_id` but has no human
    /// behind it. Absent human fields are what lets a function tell the two
    /// apart without sniffing the synthetic email.
    #[test]
    fn system_identity_omits_every_human_field() {
        let json: serde_json::Value = serde_json::to_value(CtxUser {
            email: Some("schedule+rollup@system.oxy".into()),
            name: None,
            picture: None,
            org_role: None,
            teams: Vec::new(),
            kind: CtxIdentityKind::System,
            ..full_identity()
        })
        .unwrap();
        assert_eq!(json["kind"], "system");
        for absent in ["name", "picture", "orgRole"] {
            assert!(json.get(absent).is_none(), "{absent} must be absent");
        }
        assert_eq!(json["teams"], serde_json::json!([]));
    }

    /// The isolate's view, end to end: `ctx.user.orgId` is what the SDK types
    /// promise, and the legacy `ctx.user.org_id` still resolves so functions
    /// written against the shipped snake key keep working.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn isolate_sees_camel_case_identity_and_the_legacy_org_id_alias() {
        let (result, _host) = run_with_mock_ctx(
            r#"
            export default async (req, ctx) => Response.json({
                orgId: ctx.user.orgId,
                legacy: ctx.user.org_id,
                name: ctx.user.name,
                orgRole: ctx.user.orgRole,
                team: ctx.user.teams[0].name,
                kind: ctx.user.kind,
            });
        "#,
            InvocationCtx {
                user: full_identity(),
                env: Default::default(),
            },
        )
        .await;

        let body = result.expect("function must resolve").body;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["orgId"], "22222222-2222-2222-2222-222222222222");
        assert_eq!(
            parsed["legacy"], parsed["orgId"],
            "the back-compat mirror must track orgId, not drift from it"
        );
        assert_eq!(parsed["name"], "Ada Lovelace");
        assert_eq!(parsed["orgRole"], "member");
        assert_eq!(parsed["team"], "Finance");
        assert_eq!(parsed["kind"], "user");
    }

    /// Run `artifact` against a fresh `MockHost` and hand back both, so a test
    /// can assert on the response *and* on what reached the host.
    async fn run_with_mock(artifact: &str) -> (Result<FnResponse, RuntimeError>, Arc<MockHost>) {
        run_with_mock_ctx(artifact, test_ctx()).await
    }

    /// [`run_with_mock`] with a caller-supplied identity, for asserting what the
    /// isolate actually sees on `ctx.user`.
    async fn run_with_mock_ctx(
        artifact: &str,
        ctx: InvocationCtx,
    ) -> (Result<FnResponse, RuntimeError>, Arc<MockHost>) {
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let host = Arc::new(MockHost::default());
        let result = run(
            artifact.to_string(),
            ctx,
            FnRequest::from_body(b"{}".to_vec()),
            host.clone(),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await;
        (result, host)
    }

    /// `req` carries the request, not just its body. Asserts the plumbing end
    /// to end — `sanitize_request_headers` decides *which* headers get here,
    /// this proves the ones that do actually arrive at app code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn req_exposes_method_and_headers_to_the_handler() {
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let host = Arc::new(MockHost::default());
        let result = run(
            r#"export default async function (req) {
                 return { status: 200, body: JSON.stringify({
                   method: req.method,
                   sig: req.headers["x-hub-signature-256"],
                   ct: req.headers["content-type"],
                   body: req.body,
                 }) };
               }"#
            .to_string(),
            test_ctx(),
            FnRequest {
                method: "POST".to_string(),
                headers: std::collections::BTreeMap::from([
                    ("content-type".to_string(), "application/json".to_string()),
                    ("x-hub-signature-256".to_string(), "sha256=abc".to_string()),
                ]),
                body: br#"{"hello":"world"}"#.to_vec(),
            },
            host.clone(),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await
        .expect("handler must complete");

        let parsed: serde_json::Value = serde_json::from_str(&result.body).unwrap();
        assert_eq!(parsed["method"], "POST");
        assert_eq!(parsed["ct"], "application/json");
        // The whole point: a webhook signature reaches the handler, so it can
        // be verified rather than trusted.
        assert_eq!(parsed["sig"], "sha256=abc");
        // And the body is unchanged by any of this.
        assert_eq!(parsed["body"], r#"{"hello":"world"}"#);
    }

    /// The published HMAC-SHA256 vector for key "key" over "The quick brown fox
    /// jumps over the lazy dog". Using a known-outside vector rather than
    /// round-tripping our own output means a wrong implementation cannot agree
    /// with itself and pass.
    const FOX_HMAC_HEX: &str = "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crypto_hmac_matches_the_published_vector_and_verifies() {
        let (result, _) = run_with_mock(&format!(
            r#"export default async function (req, ctx) {{
                 const key = "key";
                 const data = "The quick brown fox jumps over the lazy dog";
                 return {{ status: 200, body: JSON.stringify({{
                   hex: ctx.crypto.hmac({{ key, data }}),
                   b64: ctx.crypto.hmac({{ key, data, encoding: "base64" }}),
                   good: ctx.crypto.verifyHmac({{ key, data, signature: "{FOX_HMAC_HEX}" }}),
                   tampered: ctx.crypto.verifyHmac({{ key, data, signature: "{tampered}" }}),
                   wrongKey: ctx.crypto.verifyHmac({{ key: "kex", data, signature: "{FOX_HMAC_HEX}" }}),
                 }}) }};
               }}"#,
            FOX_HMAC_HEX = FOX_HMAC_HEX,
            // Same length, one nibble changed — so a length check cannot be
            // what rejects it.
            tampered = format!("e{}", &FOX_HMAC_HEX[1..]),
        ))
        .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&result.expect("handler must complete").body).unwrap();
        assert_eq!(parsed["hex"], FOX_HMAC_HEX);
        // base64 of the SAME published digest, derived from the hex vector
        // independently — not copied from what this code emitted.
        assert_eq!(
            parsed["b64"],
            "97yD9DBThCSxMpjmqm+xQ+9NWaFJRhdZl0edvC0aPNg="
        );
        assert_eq!(parsed["good"], true);
        assert_eq!(parsed["tampered"], false);
        assert_eq!(parsed["wrongKey"], false);
    }

    /// The security-relevant split: a malformed signature is attacker-controlled
    /// and must reject cleanly, while a bad algorithm is an author bug and must
    /// throw. Getting this backwards turns a forged request into a 500.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crypto_rejects_junk_signatures_but_throws_on_author_error() {
        let (result, _) = run_with_mock(
            r#"export default async function (req, ctx) {
                 const base = { key: "k", data: "d" };
                 let threw = false;
                 try { ctx.crypto.verifyHmac({ ...base, algorithm: "md5", signature: "aa" }); }
                 catch { threw = true; }
                 return { status: 200, body: JSON.stringify({
                   notHex: ctx.crypto.verifyHmac({ ...base, signature: "zzzz" }),
                   empty: ctx.crypto.verifyHmac({ ...base, signature: "" }),
                   badAlgorithmThrew: threw,
                   eqSame: ctx.crypto.timingSafeEqual("s3cret", "s3cret"),
                   eqDiff: ctx.crypto.timingSafeEqual("s3cret", "s3cres"),
                   eqLen: ctx.crypto.timingSafeEqual("s3cret", "s3cre"),
                 }) };
               }"#,
        )
        .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&result.expect("handler must complete").body).unwrap();
        assert_eq!(
            parsed["notHex"], false,
            "undecodable signature must not throw"
        );
        assert_eq!(parsed["empty"], false);
        assert_eq!(parsed["badAlgorithmThrew"], true);
        assert_eq!(parsed["eqSame"], true);
        assert_eq!(parsed["eqDiff"], false);
        assert_eq!(parsed["eqLen"], false);
    }

    /// Regression: an unset secret must not authorize. The documented pattern is
    /// `timingSafeEqual(req.headers[...], ctx.env.SECRET)`; if the env var was
    /// never set and the attacker simply omits the header, both sides coerced to
    /// "" and an empty-vs-empty compare returned TRUE. Reported in review.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unset_secret_never_authorizes() {
        let (result, _) = run_with_mock(
            r#"export default async function (req, ctx) {
                 const out = { threw: [] };
                 const t = (name, fn) => { try { out[name] = fn(); } catch { out.threw.push(name); } };

                 // The documented shared-secret pattern, with NOTHING configured
                 // and NOTHING sent. Must not come back true.
                 t("unsetVsMissing", () =>
                   ctx.crypto.timingSafeEqual(req.headers["x-shared-secret"], ctx.env.NOPE));
                 t("bothEmpty", () => ctx.crypto.timingSafeEqual("", ""));
                 t("emptyVsReal", () => ctx.crypto.timingSafeEqual("", "s3cret"));

                 // A missing key must not silently become the literal "undefined",
                 // which is a publicly known key anyone can sign with.
                 t("verifyNoKey", () =>
                   ctx.crypto.verifyHmac({ key: ctx.env.NOPE, data: "d", signature: "aa" }));
                 t("verifyEmptyKey", () =>
                   ctx.crypto.verifyHmac({ key: "", data: "d", signature: "aa" }));
                 t("hmacNoKey", () => ctx.crypto.hmac({ key: ctx.env.NOPE, data: "d" }));

                 return { status: 200, body: JSON.stringify(out) };
               }"#,
        )
        .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&result.expect("handler must complete").body).unwrap();

        assert_eq!(
            parsed["unsetVsMissing"], false,
            "an unset secret + an absent header must NOT authorize"
        );
        assert_eq!(parsed["bothEmpty"], false);
        assert_eq!(parsed["emptyVsReal"], false);

        // A missing/empty key is an author error, so it throws rather than
        // signing with a guessable key.
        let threw: Vec<String> = serde_json::from_value(parsed["threw"].clone()).unwrap();
        for case in ["verifyNoKey", "verifyEmptyKey", "hmacNoKey"] {
            assert!(
                threw.contains(&case.to_string()),
                "{case} must throw: {parsed}"
            );
        }
    }

    /// The whole point of this and the `req.headers` work together: a GitHub
    /// webhook can be verified inside a function, which was impossible before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_github_style_webhook_can_be_verified_end_to_end() {
        let secret = "It's a Secret to Everybody";
        let body = "Hello, World!";
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let host = Arc::new(MockHost::default());
        let result = run(
            r#"export default async function (req, ctx) {
                 const header = req.headers["x-hub-signature-256"] || "";
                 const sig = header.replace(/^sha256=/, "");
                 return { status: 200, body: JSON.stringify({
                   ok: ctx.crypto.verifyHmac({
                     key: ctx.env.WEBHOOK_SECRET, data: req.body, signature: sig,
                   }),
                 }) };
               }"#
            .to_string(),
            {
                let mut c = test_ctx();
                c.env
                    .insert("WEBHOOK_SECRET".to_string(), secret.to_string());
                c
            },
            FnRequest {
                method: "POST".to_string(),
                headers: std::collections::BTreeMap::from([(
                    "x-hub-signature-256".to_string(),
                    format!(
                        "sha256={}",
                        hex::encode(super::hmac_digest("sha256", secret, body).unwrap())
                    ),
                )]),
                body: body.as_bytes().to_vec(),
            },
            host.clone(),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await
        .expect("handler must complete");
        let parsed: serde_json::Value = serde_json::from_str(&result.body).unwrap();
        assert_eq!(parsed["ok"], true, "a valid GitHub signature must verify");
    }

    /// The happy path of the `ctx.tx` bracket: begin → the author's statements
    /// → commit, with the callback's return value handed back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctx_tx_commits_when_the_callback_resolves() {
        let (result, host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                const id = await ctx.tx("appdb", async (tx) => {
                    const rows = await tx.query("INSERT INTO orders DEFAULT VALUES RETURNING id");
                    await tx.exec("UPDATE inventory SET on_hand = on_hand - $1", [2]);
                    return rows[0].id;
                });
                return Response.json({ id });
            };
        "#,
        )
        .await;

        let resp = result.expect("ctx.tx must resolve");
        assert!(resp.body.contains(r#""id":42"#), "{}", resp.body);
        assert_eq!(
            host.tx_ops(),
            vec!["begin", "query", "exec", "commit"],
            "the wrapper must commit exactly once, after the author's statements"
        );
    }

    /// The property the whole bracket exists for: an author who throws — or
    /// whose statement fails — must not leave a transaction open, and must
    /// still see their own error rather than a rollback error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctx_tx_rolls_back_when_the_callback_throws_and_rethrows_the_original() {
        let (result, host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                let seen = null;
                try {
                    await ctx.tx("appdb", async (tx) => {
                        await tx.exec("INSERT INTO orders DEFAULT VALUES");
                        throw new Error("inventory went negative");
                    });
                } catch (e) {
                    seen = e.message;
                }
                return Response.json({ seen });
            };
        "#,
        )
        .await;

        let resp = result.expect("the handler itself must still return");
        assert!(
            resp.body.contains("inventory went negative"),
            "the author's error must survive the rollback: {}",
            resp.body
        );
        assert_eq!(
            host.tx_ops(),
            vec!["begin", "exec", "rollback"],
            "a throwing callback must roll back and must NOT commit"
        );
    }

    /// A handle that escapes its callback must fail loudly. Without this, a
    /// stashed handle would address whatever transaction later holds that id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tx_handle_used_after_the_callback_returns_is_rejected() {
        let (result, host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                let escaped;
                await ctx.tx("appdb", async (tx) => { escaped = tx; });
                let threw = false;
                try { await escaped.exec("DELETE FROM orders"); } catch (e) { threw = true; }
                return Response.json({ threw });
            };
        "#,
        )
        .await;

        let resp = result.expect("handler must return");
        assert!(resp.body.contains(r#""threw":true"#), "{}", resp.body);
        assert_eq!(
            host.tx_ops(),
            vec!["begin", "commit"],
            "the escaped handle must never reach the host"
        );
    }

    /// Parameters must cross the boundary as a JSON array. If they arrived
    /// stringified, binding would silently become interpolation — the exact
    /// failure this API exists to prevent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_params_cross_the_boundary_as_an_array() {
        let (result, _host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                const rows = await ctx.tx("appdb", (tx) =>
                    tx.query("SELECT * FROM t WHERE a = $1 AND b = $2", [7, "x"]));
                return Response.json({ params: rows[0].params });
            };
        "#,
        )
        .await;

        let resp = result.expect("handler must return");
        assert!(
            resp.body.contains(r#""params":[7,"x"]"#),
            "params must arrive as an array: {}",
            resp.body
        );
    }

    /// Omitting `params` is the common case (a statement with no placeholders)
    /// and must not become `undefined` on the wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tx_params_default_to_an_empty_array() {
        let (result, _host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                const rows = await ctx.tx("appdb", (tx) => tx.query("SELECT 1"));
                return Response.json({ params: rows[0].params });
            };
        "#,
        )
        .await;

        let resp = result.expect("handler must return");
        assert!(resp.body.contains(r#""params":[]"#), "{}", resp.body);
    }

    /// Regression: a handler that awaits an async host op (`ctx.query`) must
    /// RESUME and return once the op replies — it must not hang until the
    /// wall-clock timeout. This reproduces the `resolve()`-doesn't-pump-the-
    /// event-loop bug: with the buggy invoke path this returns `Err(Timeout)`;
    /// with `with_event_loop_promise` it returns the handler's `Response` fast.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_resumes_after_async_host_op() {
        let artifact = r#"
            export default async (req, ctx) => {
                const r = await ctx.query("select 1");
                return Response.json({ rows: r.rows.length });
            };
        "#;
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let result = run(
            artifact.to_string(),
            test_ctx(),
            FnRequest::from_body(b"{}".to_vec()),
            Arc::new(MockHost::default()),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await;
        let resp = result.expect("handler must resume after the async host op, not time out");
        assert!(
            resp.body.contains("\"rows\":1"),
            "unexpected handler body: {}",
            resp.body
        );
    }

    /// `ctx.warehouse.query` reaches a named database.
    ///
    /// `ctx.query` only ever hits the project default, and every other
    /// named-database surface is a write — so before this an app had no way to
    /// read its own OLTP store unless it happened to be the default.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warehouse_query_reads_a_named_database() {
        let artifact = r#"
            export default async (req, ctx) => {
                const r = await ctx.warehouse.query("oltp", "SELECT 1");
                return Response.json({ rows: r.rows.length, db: r.db });
            };
        "#;
        let host = Arc::new(MockHost::default());
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let resp = run(
            artifact.to_string(),
            test_ctx(),
            FnRequest::from_body(b"{}".to_vec()),
            host.clone(),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await
        .expect("handler should succeed");

        assert!(resp.body.contains("\"rows\":1"), "body: {}", resp.body);
        // The database name has to survive the boundary, or the read would
        // silently target whatever the host defaulted to.
        assert!(resp.body.contains("\"db\":\"oltp\""), "body: {}", resp.body);
    }

    /// `ctx.oltp.query(sql, params)` reaches `host.oltp("query", { sql, params })`
    /// with `params` as an array — the whole JS → op → HostCall → dispatch wiring
    /// for the app-writer path. No database name crosses the boundary: the app's
    /// own writer is derived host-side from its slug (the manifest only gates).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oltp_query_forwards_sql_and_params_to_the_host() {
        let (result, _host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                const rows = await ctx.oltp.query(
                    "SELECT * FROM bookings WHERE party_size > $1", [4]);
                return Response.json(rows[0]);
            };
        "#,
        )
        .await;

        let resp = result.expect("handler must return");
        assert!(
            resp.body.contains(r#""params":[4]"#),
            "params must cross as an array, not a stringified blob: {}",
            resp.body
        );
        assert!(
            resp.body.contains("party_size"),
            "sql must cross the boundary intact: {}",
            resp.body
        );
    }

    /// `ctx.oltp.exec` omitting `params` sends `[]`, not `undefined` — the same
    /// guarantee `ctx.tx` makes, so a no-placeholder write is not read as a
    /// wrong-arity error host-side.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oltp_exec_returns_row_count() {
        let (result, _host) = run_with_mock(
            r#"
            export default async (req, ctx) => {
                const n = await ctx.oltp.exec("DELETE FROM bookings WHERE cancelled");
                return Response.json({ n });
            };
        "#,
        )
        .await;

        let resp = result.expect("handler must return");
        assert!(resp.body.contains(r#""n":1"#), "body: {}", resp.body);
    }

    /// Regression: this isolate is bare `deno_core` (no `deno_web`) on a V8 that
    /// predates `Uint8Array.prototype.toBase64`, so `btoa` was `undefined` — an
    /// author could not produce the base64 that `ctx.email.send` attachments
    /// require, and attaching a generated file was impossible. Proves the
    /// encoder exists AND that the bytes reach the host unmangled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn isolate_can_base64_encode_attachment_bytes() {
        let artifact = r#"
            export default async (req, ctx) => {
                // Latin1 bytes as a string — what btoa is actually specified for.
                const pdf = "\x25\x50\x44\x46"; // "%PDF"
                await ctx.email.send({
                    to: "a@b.com",
                    subject: "report",
                    text: "see attached",
                    attachments: [{ filename: "r.pdf", content: btoa(pdf) }],
                });
                let wideThrew = false;
                try { btoa("日本語"); } catch { wideThrew = true; }
                // Handing btoa raw bytes must FAIL rather than encode
                // String(u8) == "37,80,68,70". Silent corruption is the thing
                // this whole change exists to prevent.
                let bytesThrew = false;
                try { btoa(new Uint8Array([0x25, 0x50])); } catch { bytesThrew = true; }
                // Padding must not terminate the decode early: concatenated
                // base64 is malformed and has to say so, not truncate.
                let concatThrew = false;
                try { atob(btoa("hello") + btoa("world")); } catch { concatThrew = true; }
                return Response.json({
                    str: btoa("hello"),
                    roundTrip: atob(btoa("hello")),
                    wideThrew,
                    bytesThrew,
                    concatThrew,
                });
            };
        "#;
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let host = Arc::new(MockHost::default());
        let resp = run(
            artifact.to_string(),
            test_ctx(),
            FnRequest::from_body(b"{}".to_vec()),
            host.clone(),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await
        .expect("btoa/atob must exist in the isolate");

        assert!(resp.body.contains(r#""str":"aGVsbG8=""#), "{}", resp.body);
        assert!(
            resp.body.contains(r#""roundTrip":"hello""#),
            "{}",
            resp.body
        );
        // Latin1-only, exactly like a browser. Note the subtler trap this
        // guards: btoa ACCEPTS U+0080..U+00FF and encodes them as Latin1, so
        // `btoa(csv)` on accented text yields mojibake rather than an error —
        // which is why generated text should use `encoding: "utf8"` instead.
        assert!(resp.body.contains(r#""wideThrew":true"#), "{}", resp.body);
        assert!(resp.body.contains(r#""bytesThrew":true"#), "{}", resp.body);
        assert!(resp.body.contains(r#""concatThrew":true"#), "{}", resp.body);

        let email = host
            .last_email
            .lock()
            .unwrap()
            .clone()
            .expect("ctx.email.send must reach the host");
        assert_eq!(email["attachments"][0]["content"], "JVBERg==");
    }

    /// The other half of the fix: generated TEXT needs no encoder at all, and
    /// `encoding: "utf8"` must survive the isolate boundary so the host can
    /// attach the bytes verbatim.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn utf8_attachment_crosses_the_boundary_intact() {
        let artifact = r#"
            export default async (req, ctx) => {
                await ctx.email.send({
                    to: "a@b.com",
                    subject: "report",
                    text: "see attached",
                    attachments: [{
                        filename: "report.csv",
                        content: "name,total\nCafé,3\n",
                        encoding: "utf8",
                        contentType: "text/csv",
                    }],
                });
                return Response.json({ ok: true });
            };
        "#;
        let (_cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let host = Arc::new(MockHost::default());
        run(
            artifact.to_string(),
            test_ctx(),
            FnRequest::from_body(b"{}".to_vec()),
            host.clone(),
            cancel_rx,
            std::time::Duration::from_secs(10),
            Arc::new(std::sync::Mutex::new(Vec::new())),
            tracing::Span::none(),
        )
        .await
        .expect("handler must complete");

        let email = host.last_email.lock().unwrap().clone().expect("sent");
        let att = &email["attachments"][0];
        assert_eq!(att["encoding"], "utf8");
        assert_eq!(att["content"], "name,total\nCafé,3\n");
    }
}
