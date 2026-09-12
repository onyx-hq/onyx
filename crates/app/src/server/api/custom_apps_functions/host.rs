//! Host-side data plane for Oxy Functions — the Rust implementation of the
//! `ctx.query` / `ctx.fetch` calls the isolate makes over the broker channel.
//!
//! See `internal-docs/customer-apps-functions.md` §11.5
//! (query cap), §11.9 (fetch size cap), §11.3 (warehouse write scope —
//! per-function fail-closed `destinations` allowlist).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use agentic_connector::{DatabaseConnector, PostgresConnector};
use agentic_pipeline::airway_run::StartAirwayRequest;
use agentic_semantic::compile::{CompiledQuery, resolve_and_compile};
use agentic_semantic::config::SemanticQueryConfig;
use sea_orm::DatabaseConnection;
use uuid::Uuid;

use oxy::service::secret_manager::SecretManagerService;

use crate::server::api::custom_apps_storage::RetentionPolicy;

use super::data_audit::{self, InvocationIdentity, WriteRecord};
use super::host_call_attrs::db_query_summary;
use super::runtime::{FUNCTION_MAX_ROWS, FUNCTION_STREAM_MAX_ROWS, FunctionHost};
use super::seam::{FunctionProjectContext, FunctionQueryExecutor};
use super::upsert_support;
use agentic_connector::SqlDialect;
use agentic_connector::SqlTransaction;

/// Outbound fetch response size cap (design doc §11.9).
const FETCH_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Total per-request wall-clock cap for `ctx.fetch` (connect + transfer). A
/// backstop so a slow/never-responding upstream can't keep the isolate parked
/// in a host call past the function's own timeout (design doc §11.9).
const FETCH_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Concrete `FunctionHost` backed by the invocation's resolved project
/// context (for SQL connectors) and a shared HTTP client (for `ctx.fetch`).
pub struct ProjectFunctionHost {
    /// The invocation's project context (connectors + workspace config +
    /// Airway seed), behind a trait so the runtime doesn't depend on
    /// `agentic_wiring`. Injected at construction.
    proj_ctx: Arc<dyn FunctionProjectContext>,
    /// Runs `ctx.query` / `ctx.queryStream` SQL. Injected at the composition
    /// root so the runtime depends on the trait, not on `projects::query`.
    query_exec: Arc<dyn FunctionQueryExecutor>,
    db: DatabaseConnection,
    http: reqwest::Client,
    /// §11.3 allowlist — database names `ctx.warehouse.*` may write to. Empty →
    /// the function may not write to any database (fail-closed).
    write_destinations: Vec<String>,
    /// Project the app belongs to — scopes the SecretManager for `ctx.secrets.set`.
    project_id: Uuid,
    /// App id — `ctx.secrets.set` writes land under `apps/<app_id>/`.
    app_id: Uuid,
    /// Org the app belongs to. Storage quotas are org-level (a tenant thinks in
    /// organizations, and one invoice covers every app under it), so the write
    /// path needs it without re-reading `apps`.
    org_id: Uuid,
    /// Actor stamped as created_by/updated_by on secret writes (the invoking
    /// user for a route call; the app owner for a scheduled call). Non-null FK.
    actor: Uuid,
    /// App display name, used as the `From` friendly name on `ctx.email.send`.
    app_name: String,
    /// What this function may do, and under what storage policy.
    caps: FunctionCapabilities,
    /// Per-invocation `ctx.email.send` counter (this host lives for exactly one
    /// invocation), bounding email fan-out from a single run.
    email_send_count: std::sync::atomic::AtomicUsize,
    /// Transactions `ctx.tx()` has open. Per-invocation like the counter above,
    /// which is what makes cleanup automatic — see `tx`'s module docs.
    transactions: super::tx::TxRegistry,
    /// Layer-1 preagg cache + renewal threshold for `ctx.semantic`. Injected
    /// at the serve router and threaded through the invocation, because this
    /// route is mounted outside the API router and so has no `AppState` to read
    /// it from. Both fields `None` (the scheduled path, or a composition with no
    /// rebuild worker) means every semantic query compiles to warehouse SQL.
    preagg: crate::server::api::middlewares::workspace_context::PreaggCacheCtx,
    /// Where the caller may act — the same `Reach` `ctx.user.reach` carries,
    /// so `ctx.semantic.query({ scope: "reach" })` pins to it server-side.
    reach: crate::server::api::operating_graph::reach::Reach,
    /// Resolved `ctx.oltp` writer connection, cached for this invocation so N
    /// calls cost one control-plane resolve (a query + decrypt) rather than N.
    /// The per-call TCP+TLS connect to the tenant still happens (each call is a
    /// one-shot transaction on its own connection — see `oltp`); this only saves
    /// the resolve. Filled lazily on the first `ctx.oltp` call.
    oltp_conn: tokio::sync::Mutex<Option<oxy_oltp::resolver::WriterConnection>>,
    /// Who is running, for the audit record on every data-plane write.
    identity: InvocationIdentity,
    /// The writes each open `ctx.tx()` handle has made so far, keyed by the
    /// handle id; audited as one row at commit, dropped at rollback.
    tx_writes: tokio::sync::Mutex<std::collections::HashMap<u64, TxAudit>>,
    /// Every committed `ctx.oltp` / `ctx.warehouse` write of this invocation,
    /// coalesced by target; written as one row per plane when the invocation
    /// ends (`end_of_invocation`), so the hot path pays no control-plane
    /// round trip.
    writes: tokio::sync::Mutex<data_audit::WriteBuffer>,
}

/// The audit state of one open transaction.
struct TxAudit {
    plane: &'static str,
    database: String,
    writes: Vec<WriteRecord>,
}

/// The fail-closed capability gates a function's manifest grants, plus the
/// app-level storage retention policy those writes are stamped with.
///
/// Grouped rather than passed as five more positional `bool`s: adjacent
/// same-typed parameters are exactly where a transposed argument silently grants
/// a capability nobody declared, and this constructor already carried a
/// `too_many_arguments` waiver.
///
/// Every gate defaults to **false** and the policy to empty, so a caller that
/// forgets to set one denies the capability rather than granting it.
#[derive(Debug, Clone, Default)]
pub struct FunctionCapabilities {
    /// §11.x — `ctx.secrets.set` is rejected unless the manifest declares
    /// `secrets.write`.
    pub secrets_write: bool,
    /// Gate for `ctx.email.send`.
    pub email_send: bool,
    /// Gate for `ctx.org.people()` — the org's people directory, read-only.
    pub org_read: bool,
    /// Gate for `ctx.storage` reads (getDownloadUrl / get / head / list, and
    /// the source side of copy).
    pub storage_read: bool,
    /// Gate for `ctx.storage` writes (getUploadUrl / put / delete, and the
    /// destination side of copy).
    pub storage_write: bool,
    /// What `ctx.oltp` may do — the gate AND why it's closed, so the two
    /// fail-closed reasons get different diagnoses. See [`OltpCapability`].
    pub oltp: OltpCapability,
    /// App-level `storage.retention` policy. Empty → written objects carry no
    /// TTL tag and never expire.
    pub storage_retention: RetentionPolicy,
    /// Per-function `ctx.fetch` response ceiling. `None` → [`FETCH_MAX_BYTES`].
    ///
    /// `Option`, not a bare `u64`: this struct is `Default`-constructible and
    /// every other field defaults to DENY, but a `0` here would deny every
    /// fetch rather than fall back — so absence has to mean "the default", not
    /// "nothing".
    pub fetch_max_bytes: Option<u64>,
}

/// Whether `ctx.oltp` is available, and — when it is not — which of the two
/// fail-closed reasons applies, so the host can tell them apart in the error.
///
/// The writer is DERIVED from the app's slug, never named by the manifest (the
/// binding that keeps one app out of another's schema), so "the slug can't back
/// a schema" is a distinct failure from "the manifest didn't ask for `ctx.oltp`"
/// — and reporting the first as the second sends an author to edit a manifest
/// that is already correct.
#[derive(Debug, Clone, Default)]
pub enum OltpCapability {
    /// The manifest did not enable `ctx.oltp`.
    #[default]
    Disabled,
    /// Enabled, and the app's slug backs a valid `app_<writer>` schema.
    Enabled { writer: String },
    /// Enabled, but the app's slug cannot normalise to a schema identifier
    /// (leading digit, too long, …). Carries the slug for the diagnosis.
    SlugNotDerivable { slug: String },
}

impl OltpCapability {
    /// Build from the manifest gate and the app's slug. The writer is DERIVED
    /// from the slug (never the manifest), so an enabled gate whose slug cannot
    /// back a schema is [`Self::SlugNotDerivable`] — a different fail-closed
    /// reason from a disabled gate, and one the host reports differently.
    pub fn resolve(enabled: bool, app_slug: &str) -> Self {
        if !enabled {
            return Self::Disabled;
        }
        match oxy_oltp::schema::app_writer_name(app_slug) {
            Some(writer) => Self::Enabled { writer },
            None => Self::SlugNotDerivable {
                slug: app_slug.to_string(),
            },
        }
    }
}

impl ProjectFunctionHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        proj_ctx: Arc<dyn FunctionProjectContext>,
        query_exec: Arc<dyn FunctionQueryExecutor>,
        db: DatabaseConnection,
        write_destinations: Vec<String>,
        project_id: Uuid,
        app_id: Uuid,
        org_id: Uuid,
        actor: Uuid,
        app_name: String,
        caps: FunctionCapabilities,
        preagg: crate::server::api::middlewares::workspace_context::PreaggCacheCtx,
        reach: crate::server::api::operating_graph::reach::Reach,
        identity: InvocationIdentity,
    ) -> Self {
        Self {
            identity,
            tx_writes: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            writes: tokio::sync::Mutex::new(data_audit::WriteBuffer::new()),
            proj_ctx,
            query_exec,
            db,
            write_destinations,
            project_id,
            app_id,
            org_id,
            actor,
            app_name,
            caps,
            preagg,
            reach,
            email_send_count: std::sync::atomic::AtomicUsize::new(0),
            transactions: super::tx::TxRegistry::default(),
            oltp_conn: tokio::sync::Mutex::new(None),
            // `ctx.fetch` is defended in two layers:
            //  1. `is_safe_outbound` rejects the request URL up front (scheme,
            //     literal private IPs, internal suffixes).
            //  2. `PublicOnlyDnsResolver` validates every *resolved* IP at
            //     connect time, closing the DNS-rebinding hole the URL string
            //     check can't see (a public hostname whose A record points at
            //     169.254.169.254 / 10.x / 127.x).
            // Redirects are disabled so a 302 to an internal address can't
            // launder past either layer on a later hop (reqwest gives no
            // per-hop validation hook), and a total timeout bounds the call.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(10))
                .dns_resolver(Arc::new(PublicOnlyDnsResolver))
                .build()
                .expect("failed to build ctx.fetch HTTP client"),
        }
    }

    /// Resolve the database `ctx.query` runs against: the project's
    /// configured default, else the first listed database. Mirrors the
    /// fallback in `projects::query::run_query`.
    fn default_database(&self) -> Result<String, String> {
        let cm = &self.proj_ctx.workspace_manager().config_manager;
        if let Some(default) = cm.default_database_ref() {
            Ok(default.clone())
        } else if let Some(first) = cm.list_databases().first() {
            Ok(first.name.clone())
        } else {
            Err("this project has no databases configured".to_string())
        }
    }

    /// Build a connector for `db_name`, bounded by the host DB-op timeout so a
    /// slow TLS/auth handshake during connector construction can't keep a
    /// detached isolate thread alive past the backstop.
    async fn connect(&self, db_name: &str) -> Result<Arc<dyn DatabaseConnector>, String> {
        with_db_timeout("connect", async {
            self.proj_ctx
                .build_connector_for(db_name)
                .await
                .map_err(|e| format!("failed to connect to database '{db_name}': {e}"))
        })
        .await
    }

    /// `(trace id, traceparent)` of the host-op span this call runs under.
    fn trace_context() -> (Option<String>, Option<String>) {
        (
            oxy_telemetry::propagation::current_ids().map(|(t, _)| t),
            oxy_telemetry::propagation::current_traceparent(),
        )
    }

    /// Name the session for the transaction's lifetime (layer 1 of
    /// `data_audit`). Postgres only. A failure is a **platform error** raised
    /// before any tenant statement runs: the `SET LOCAL` executes inside the
    /// open block, so "log and continue" would leave the transaction aborted
    /// and the author's next statement failing with SQL they never wrote.
    async fn name_session(
        &self,
        tx: &mut dyn SqlTransaction,
        trace_id: Option<&str>,
    ) -> Result<(), String> {
        let tag = data_audit::session_tag(&self.identity, trace_id);
        let sql = data_audit::set_application_name_sql(&tag);
        with_db_timeout("name-session", async {
            tx.exec(&sql, &[]).await.map(|_| ()).map_err(|e| {
                format!("could not name the database session (platform-side application_name): {e}")
            })
        })
        .await
    }

    /// Layer 3, immediate: one hash-chained row now. Used for a committed
    /// `ctx.tx()` handle, which is already one row per transaction.
    async fn record_writes(
        &self,
        action: &'static str,
        writes: Vec<WriteRecord>,
        trace_id: Option<&str>,
    ) {
        if writes.is_empty() {
            return;
        }
        let entry = data_audit::entry(
            action,
            &self.identity,
            self.org_id,
            self.project_id,
            &writes,
            trace_id,
        );
        oxy_app_core::audit::record_best_effort(&self.db, entry).await;
    }

    /// Layer 3, deferred: fold a committed `ctx.oltp` / `ctx.warehouse` write
    /// into the invocation's buffer; `end_of_invocation` writes the rows.
    ///
    /// Host calls are detached tasks and `runtime::run` returns on cancel or
    /// timeout without awaiting them, so a write can commit after
    /// `end_of_invocation` drained the buffer. The buffer is then closed and
    /// hands the write back; it is recorded on its own, right here — the
    /// per-write round trip is the right trade on a path that has already
    /// timed out, and a committed write never goes unaudited.
    async fn note_write(&self, write: WriteRecord) {
        let late = self.writes.lock().await.note(write);
        if let Some(write) = late {
            let action = if write.plane == "oltp" {
                data_audit::ACTION_OLTP_WRITE
            } else {
                data_audit::ACTION_WAREHOUSE_WRITE
            };
            let (trace_id, _) = Self::trace_context();
            self.record_writes(action, vec![write], trace_id.as_deref())
                .await;
        }
    }

    /// Remember a write made through a `ctx.tx()` handle until it commits.
    async fn note_tx_write(
        &self,
        id: u64,
        summary: super::host_call_attrs::QuerySummary,
        rows: Option<u64>,
    ) {
        if !data_audit::is_write_verb(&summary.verb) {
            return;
        }
        if let Some(audit) = self.tx_writes.lock().await.get_mut(&id) {
            let write = WriteRecord {
                plane: audit.plane,
                namespace: audit.database.clone(),
                verb: summary.verb,
                table: summary.table,
                rows,
                statements: 1,
            };
            data_audit::coalesce(&mut audit.writes, write);
        }
    }

    /// The database a `ctx.tx()` handle was opened on, for the span.
    async fn tx_database(&self, id: u64) -> Option<String> {
        self.tx_writes
            .lock()
            .await
            .get(&id)
            .map(|a| a.database.clone())
    }

    /// §11.3 — a write may only target a database the function declared in its
    /// manifest `destinations`, and that database must actually be configured.
    ///
    /// Fail-closed: an empty allowlist denies every database. Checked *before*
    /// any connector — and therefore any credential — is built, so a function
    /// can never reach the project's source warehouse just because it happens
    /// to be configured. `ctx.tx` shares this with `ctx.warehouse` because a
    /// transaction is a write by definition; splitting them would let a
    /// transaction reach a database the same function may not `insert` into.
    ///
    /// The message names no surface: `reply_json` prefixes the one that asked
    /// (`ctx.warehouse` or `ctx.tx`), and spelling it here too was how this
    /// error came out as `ctx.tx: ctx.tx: database '…' is not in …`.
    fn check_write_destination(&self, database: &str) -> Result<(), String> {
        if !self.write_destinations.iter().any(|d| d == database) {
            return Err(format!(
                "database '{database}' is not in this function's \
                 `destinations` allowlist (declare it in oxy-app.json to permit writes)"
            ));
        }
        let cm = &self.proj_ctx.workspace_manager().config_manager;
        if !cm.list_databases().iter().any(|db| db.name == database) {
            return Err(format!(
                "database '{database}' is not configured for this project"
            ));
        }
        Ok(())
    }

    /// Pull `{ id, sql, params }` off a `ctx.tx` payload.
    ///
    /// `params` distinguishes three cases: **absent or null** is "no
    /// parameters" (the common case — a statement with no placeholders), an
    /// **array** is the argument list, and **anything else** is an author error
    /// reported as such. Collapsing the third into the first is what produced
    /// the misleading "takes 1 parameter(s) but 0 were passed" for
    /// `tx.exec(sql, {a: 1})`.
    fn tx_statement(
        payload: &serde_json::Value,
    ) -> Result<(u64, String, Vec<serde_json::Value>), String> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "`id` is required".to_string())?;
        let sql = payload
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "`sql` is required".to_string())?
            .to_string();
        // Absent means "no parameters"; present-but-not-an-array is an author
        // error and must say so. Collapsing both to `[]` reported the far more
        // confusing "takes 1 parameter(s) but 0 were passed".
        let params = match payload.get("params") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(a)) => a.clone(),
            Some(other) => {
                return Err(format!(
                    "`params` must be an array of values, got {}. \
                     Pass positional arguments for $1, $2, … — e.g. [tableNo, sku].",
                    match other {
                        serde_json::Value::Object(_) => "an object",
                        serde_json::Value::String(_) => "a string",
                        serde_json::Value::Number(_) => "a number",
                        serde_json::Value::Bool(_) => "a boolean",
                        _ => "a non-array value",
                    }
                ));
            }
        };
        Ok((id, sql, params))
    }

    /// Pull `{ sql, params }` off a `ctx.oltp` payload. Like [`tx_statement`]
    /// without the transaction `id` — `ctx.oltp` runs one auto-committed
    /// statement, not a caller-held transaction. Same three-way `params`
    /// handling (absent/null → none, array → the list, anything else → an
    /// author error) so a mistyped argument says so rather than surfacing as a
    /// confusing arity error.
    fn oltp_statement(
        payload: &serde_json::Value,
    ) -> Result<(String, Vec<serde_json::Value>), String> {
        let sql = payload
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "`sql` is required".to_string())?
            .to_string();
        let params = match payload.get("params") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(serde_json::Value::Array(a)) => a.clone(),
            Some(_) => {
                return Err("`params` must be an array of values. Pass \
                     positional arguments for $1, $2, … — e.g. [name, partySize]."
                    .to_string());
            }
        };
        Ok((sql, params))
    }
}

#[async_trait::async_trait]
impl FunctionHost for ProjectFunctionHost {
    /// Layer 3 for the buffered planes: one `app.oltp.write` and one
    /// `app.warehouse.write` row per invocation, each listing every target
    /// it touched with statement counts and row sums. Runs after the isolate
    /// has finished, off the hot path; `ctx.tx()` commits were recorded as
    /// they happened.
    async fn end_of_invocation(&self) {
        // Drain and close under the one lock `note_write` checks, so a write
        // racing this either lands in `writes` here or records itself.
        let writes = self.writes.lock().await.drain();
        if writes.is_empty() {
            return;
        }
        let (trace_id, _) = Self::trace_context();
        let (oltp, other): (Vec<_>, Vec<_>) = writes.into_iter().partition(|w| w.plane == "oltp");
        self.record_writes(data_audit::ACTION_OLTP_WRITE, oltp, trace_id.as_deref())
            .await;
        self.record_writes(
            data_audit::ACTION_WAREHOUSE_WRITE,
            other,
            trace_id.as_deref(),
        )
        .await;
    }

    async fn query(&self, sql: String) -> Result<serde_json::Value, String> {
        let db_name = self.default_database()?;
        let connector = self.connect(&db_name).await?;
        let (rows, truncated) =
            query_with_truncation(&self.query_exec, connector, &sql, FUNCTION_MAX_ROWS).await?;
        let result = serde_json::json!({ "rows": rows, "truncated": truncated });
        enforce_result_byte_cap(&result)?;
        Ok(result)
    }

    async fn query_stream(&self, sql: String) -> Result<serde_json::Value, String> {
        let db_name = self.default_database()?;
        let connector = self.connect(&db_name).await?;
        let rows = with_db_timeout(
            "query_stream",
            self.query_exec
                .execute(connector, &sql, FUNCTION_STREAM_MAX_ROWS),
        )
        .await?;
        Ok(serde_json::Value::Array(rows))
    }

    async fn fetch(
        &self,
        url: String,
        init: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let parsed = reqwest::Url::parse(&url).map_err(|e| format!("invalid url: {e}"))?;
        if !is_safe_outbound(&parsed) {
            return Err(format!("fetch to '{url}' blocked by SSRF allowlist"));
        }

        let method = init
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("GET")
            .to_uppercase();
        let mut req = self
            .http
            .request(
                reqwest::Method::from_bytes(method.as_bytes())
                    .map_err(|_| format!("invalid method '{method}'"))?,
                parsed,
            )
            .timeout(FETCH_TOTAL_TIMEOUT);
        if let Some(headers) = init.get("headers").and_then(|h| h.as_object()) {
            for (k, v) in headers {
                if let Some(vs) = v.as_str() {
                    req = req.header(k, vs);
                }
            }
        }
        if let Some(body) = fetch_body_bytes(&init)? {
            req = req.body(body);
        }

        let mut resp = req.send().await.map_err(|e| format!("fetch failed: {e}"))?;
        let status = resp.status().as_u16();

        // A function may raise this via `fetch.maxResponseBytes`, clamped to the
        // platform ceiling at manifest-resolve time.
        let max_bytes = self.caps.fetch_max_bytes.unwrap_or(FETCH_MAX_BYTES);
        // §11.9 — reject oversized responses up front via Content-Length…
        if let Some(len) = resp.content_length()
            && len > max_bytes
        {
            return Err(format!(
                "response too large ({len} bytes > {max_bytes} cap)"
            ));
        }
        // …but Content-Length is advisory: a chunked response carries none, and
        // a hostile server can simply lie. So the cap is enforced against bytes
        // RECEIVED, aborting mid-stream, rather than against bytes retained
        // after the fact.
        //
        // `resp.bytes()` would read the whole body into memory and only then
        // compare — which bounds what we KEEP, not what we ALLOCATE. That was
        // survivable at a fixed 10 MiB. It is not once a manifest can raise the
        // ceiling to `fetch_ceiling_bytes()` on a process shared by every app on
        // the instance, and it was never bounded at all for a length-less
        // response.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("read failed: {e}"))?
        {
            // Checked BEFORE extending, so the cap is never exceeded even
            // transiently by one oversized chunk.
            if bytes.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(format!(
                    "response too large (exceeded the {max_bytes} byte cap)"
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        // UTF-8 stays the default for back-compat, but it is lossy: every
        // non-UTF-8 byte becomes U+FFFD, so fetching a PDF/PNG to attach to an
        // email silently yields a corrupt file. `encoding: "base64"` is the
        // only way binary survives the crossing — same contract as
        // `ctx.storage.get`.
        let encoding = encoding_or(init.get("encoding").and_then(|e| e.as_str()), "utf8");
        let body = match encoding {
            "utf8" => String::from_utf8_lossy(&bytes).into_owned(),
            "base64" => encode_base64(&bytes),
            other => {
                return Err(format!(
                    "ctx.fetch: unknown encoding '{other}' (expected 'utf8' or 'base64')"
                ));
            }
        };
        Ok(serde_json::json!({ "status": status, "body": body, "encoding": encoding }))
    }

    async fn semantic_query(
        &self,
        mut spec: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // `scope: "reach"` is oxy's, not the query's: peeled off before the
        // config parses, then honoured below by pinning the bound view's key
        // to the keys the caller's places carry. Anything else in `scope` is a
        // typo, and a typo that silently answered everything is the one
        // outcome this option exists to prevent.
        let scope = spec
            .as_object_mut()
            .and_then(|m| m.remove("scope"))
            .map(|v| v.as_str().map(str::to_string).unwrap_or_default());
        let scoped = match scope.as_deref() {
            None | Some("") => false,
            Some("reach") => true,
            Some(other) => {
                return Err(format!(
                    "invalid semantic query spec: unknown scope `{other}`"
                ));
            }
        };
        let mut query: SemanticQueryConfig = serde_json::from_value(spec)
            .map_err(|e| format!("invalid semantic query spec: {e}"))?;

        let cm = &self.proj_ctx.workspace_manager().config_manager;
        let scan_path = cm.semantics_scan_path();
        // A scoped query needs the layer before it compiles, to know which of
        // its views are bound. Loaded once and handed to the compile below,
        // so the directory is walked one time, not two.
        let pre_loaded_layer = if scoped {
            let scan_for_layer = scan_path.clone();
            let layer = tokio::task::spawn_blocking(move || {
                oxy_airlayer_compat::load_layer_from_dir(&scan_for_layer)
            })
            .await
            .map_err(|e| format!("semantic layer task panicked: {e}"))?
            .map_err(|e| format!("semantic layer failed to load: {e}"))?;
            crate::server::api::operating_graph::binding::apply_reach_scope(
                &self.db,
                self.org_id,
                &layer,
                &self.reach,
                &mut query,
            )
            .await
            .map_err(|e| format!("ctx.semantic.query: {e}"))?;
            Some(layer)
        } else {
            None
        };
        let databases: Vec<oxy_airlayer_compat::DatabaseConfig> = cm
            .list_databases()
            .iter()
            .map(|db| oxy_airlayer_compat::database_config(db.name.clone(), db.dialect()))
            .collect();

        // The rollup short-circuit, resolved exactly as
        // `/api/projects/{id}/semantic-query` resolves it — a bundle asking the
        // same question through `ctx.semantic` instead of the HTTP route must
        // not silently drop to the warehouse. `preagg_context` yields `None`
        // when this composition carries no Layer-1 cache (the scheduled path),
        // and `try_resolve_preagg` yields `None` when no rollup covers the
        // request, so both fall through to the warehouse below.
        //
        // The threshold comes from THIS workspace's own
        // `pre_aggregations.refresh_worker.renewal_threshold` when the process
        // publishes no global value — the same key the rebuild cycle reads.
        let workspace_id = self.proj_ctx.workspace_manager().workspace_id;
        let renewal_threshold_secs = self.preagg.renewal_threshold_secs_or(cm);
        let preagg = crate::server::preagg_context::preagg_context(
            workspace_id,
            self.preagg.cache.clone(),
            Some(renewal_threshold_secs),
            // A read surface: `ctx.semantic` renders a number for a bundle to
            // display, and the badge says which tier answered.
            crate::server::preagg_context::RollupFreshness::ServeStale,
        );

        let compiled = tokio::task::spawn_blocking(move || {
            resolve_and_compile(
                &scan_path,
                &databases,
                &query,
                preagg.as_ref(),
                pre_loaded_layer,
            )
        })
        .await
        .map_err(|e| format!("semantic compile task panicked: {e}"))?
        .map_err(|e| format!("semantic compile failed: {e}"))?;

        let (sql, database_name) = match compiled {
            CompiledQuery::Warehouse { sql, database_name } => (sql, database_name),
            CompiledQuery::Preaggregation {
                preagg_sql,
                source,
                warehouse_sql,
                warehouse_database,
            } => {
                // A rollup that won't read is not a failed query — the same
                // question has a warehouse answer, and the variant carries the
                // SQL for it. Same posture as the `/semantic-query` route.
                match read_rollup(&preagg_sql, &source, FUNCTION_MAX_ROWS).await {
                    Ok(result) => {
                        enforce_result_byte_cap(&result)?;
                        return Ok(result);
                    }
                    Err(e) => {
                        tracing::warn!(
                            remote = source.is_remote(),
                            error = %e,
                            "ctx.semantic: preagg rollup read failed; answering from the warehouse instead"
                        );
                        (warehouse_sql, warehouse_database)
                    }
                }
            }
        };

        let connector = self.connect(&database_name).await?;
        let (rows, truncated) =
            query_with_truncation(&self.query_exec, connector, &sql, FUNCTION_MAX_ROWS).await?;
        let result = serde_json::json!({ "rows": rows, "truncated": truncated });
        enforce_result_byte_cap(&result)?;
        Ok(result)
    }

    async fn airway_run(
        &self,
        pipeline_ref: String,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Seed the run on the global queue and return the run id. We don't
        // drive a co-located coordinator (TaskScope::Global) because ELT runs
        // routinely outlast a function's timeout ceiling — the worker fleet
        // picks it up and drives it to completion asynchronously. The caller
        // polls the Orchestrator/runs API for status.
        let request = StartAirwayRequest {
            pipeline_ref,
            variables: if variables.is_null() {
                None
            } else {
                Some(variables)
            },
            thread_id: None,
            resources: Vec::new(),
            schedule_id: None,
            trigger: Some("manual".to_string()),
            logical_date: None,
            retry_of: None,
            backfill_from: None,
            backfill_to: None,
        };
        let run_id = self.proj_ctx.start_airway_seed(&self.db, request).await?;
        Ok(serde_json::json!({ "runId": run_id }))
    }

    /// `ctx.warehouse.query(database, sql)` — a READ against a named database.
    ///
    /// `ctx.query` targets the project's default database, and every other
    /// named-database surface here is a write — so before this there was no way
    /// for an app to read a database that was not the default. A per-org OLTP
    /// database never is: it is the app's own store, sitting beside whatever
    /// warehouse the project analyses.
    ///
    /// Deliberately NOT behind `check_write_destination`. That allowlist exists
    /// so a function cannot reach the project's source warehouse and modify it;
    /// a read is not that risk, and `postgres_managed` resolves the read-only
    /// analyst regardless of who asks. Requiring a *write* declaration to
    /// perform a read would also mean every read-only app had to ask for write
    /// access, which is the wrong shape.
    async fn warehouse_query(
        &self,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let database = payload
            .get("database")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "warehouse.query: `database` is required".to_string())?;
        let sql = payload
            .get("sql")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "warehouse.query: `sql` is required".to_string())?;

        data_audit::record_db_span(&db_query_summary(sql), Some(database), None);
        let connector = self.connect(database).await?;
        let (rows, truncated) =
            query_with_truncation(&self.query_exec, connector, sql, FUNCTION_MAX_ROWS).await?;
        let result = serde_json::json!({ "rows": rows, "truncated": truncated });
        enforce_result_byte_cap(&result)?;
        Ok(result)
    }

    async fn warehouse_write(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let database = payload
            .get("database")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "`database` is required".to_string())?;

        self.check_write_destination(database)?;

        let sql = match op.as_str() {
            "exec" => payload
                .get("sql")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "warehouse.exec: `sql` is required".to_string())?
                .to_string(),
            "insert" => build_insert_sql(&payload, false)?,
            "upsert" => build_insert_sql(&payload, true)?,
            other => return Err(format!("unknown op '{other}'")),
        };

        let summary = db_query_summary(&sql);
        let payload_table = payload.get("table").and_then(|v| v.as_str());
        data_audit::record_db_span(&summary, Some(database), payload_table);
        let connector = self.connect(database).await?;
        if op == "upsert" {
            upsert_support::check(connector.dialect())?;
        }
        let (_, traceparent) = Self::trace_context();
        let tag = data_audit::tag(&self.identity, traceparent.as_deref());
        let plane = data_audit::plane_for_dialect(connector.dialect());
        let label = format!("warehouse {op}");
        with_db_timeout(&label, async {
            // The SQL goes as the app wrote it; the connector decides where the
            // tag rides, because only it knows what its engine reads as SQL.
            connector
                .execute_statement_tagged(&sql, &tag)
                .await
                .map_err(|e| format!("warehouse {op} failed: {e}"))
        })
        .await?;
        // The verb decides, not the method: `ctx.warehouse.exec("SELECT …")`
        // is a read and leaves no row.
        if data_audit::is_write_verb(&summary.verb) {
            let table = if summary.table.is_empty() {
                payload_table.unwrap_or_default().to_string()
            } else {
                summary.table
            };
            self.note_write(WriteRecord {
                plane,
                namespace: database.to_string(),
                verb: summary.verb,
                table,
                rows: None,
                statements: 1,
            })
            .await;
        }
        Ok(serde_json::json!({ "ok": true }))
    }

    /// `ctx.tx` — five verbs over one op, dispatched onto the per-invocation
    /// [`TxRegistry`].
    ///
    /// `begin` is the only verb that touches authorization: it runs the same
    /// fail-closed `destinations` check as `ctx.warehouse`, before a connector
    /// exists. The other four take an id that `begin` handed out, and the
    /// registry rejects any id it did not issue — so a script cannot reach a
    /// database by guessing a number.
    ///
    /// [`TxRegistry`]: super::tx::TxRegistry
    async fn tx(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match op.as_str() {
            "begin" => {
                let database = payload
                    .get("database")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "`database` is required".to_string())?;
                self.check_write_destination(database)?;
                let connector = self.connect(database).await?;
                let mut tx = with_db_timeout("begin", async {
                    connector
                        .begin_transaction()
                        .await
                        .map_err(|e| format!("could not open a transaction on '{database}': {e}"))
                })
                .await?;
                data_audit::record_db_namespace(database);
                if connector.dialect() == SqlDialect::Postgres {
                    let (trace_id, _) = Self::trace_context();
                    // On error `tx` drops here, which closes the connection
                    // and rolls the empty transaction back.
                    self.name_session(&mut *tx, trace_id.as_deref()).await?;
                }
                let id = self.transactions.insert(tx).await?;
                self.tx_writes.lock().await.insert(
                    id,
                    TxAudit {
                        plane: data_audit::plane_for_dialect(connector.dialect()),
                        database: database.to_string(),
                        writes: Vec::new(),
                    },
                );
                Ok(serde_json::json!({ "id": id }))
            }
            "query" => {
                let (id, sql, params) = Self::tx_statement(&payload)?;
                let summary = db_query_summary(&sql);
                data_audit::record_db_span(&summary, self.tx_database(id).await.as_deref(), None);
                let (_, traceparent) = Self::trace_context();
                let tagged = data_audit::commented(&sql, &self.identity, traceparent.as_deref());
                let rows =
                    with_db_timeout("query", self.transactions.query(id, &tagged, &params)).await?;
                self.note_tx_write(id, summary, Some(rows.len() as u64))
                    .await;
                let result = serde_json::json!({ "rows": rows });
                enforce_result_byte_cap(&result)?;
                Ok(result)
            }
            "exec" => {
                let (id, sql, params) = Self::tx_statement(&payload)?;
                let summary = db_query_summary(&sql);
                data_audit::record_db_span(&summary, self.tx_database(id).await.as_deref(), None);
                let (_, traceparent) = Self::trace_context();
                let tagged = data_audit::commented(&sql, &self.identity, traceparent.as_deref());
                let count =
                    with_db_timeout("exec", self.transactions.exec(id, &tagged, &params)).await?;
                self.note_tx_write(id, summary, Some(count)).await;
                Ok(serde_json::json!({ "rowCount": count }))
            }
            "commit" | "rollback" => {
                let id = payload
                    .get("id")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| "`id` is required".to_string())?;
                // Bounded like the other ops: `take` waits on the slot lock, so
                // a commit racing a statement on the same handle (an author bug)
                // would otherwise wait unbounded on that statement.
                let tx = with_db_timeout("take", self.transactions.take(id)).await?;
                let committing = op == "commit";
                let label = if committing { "commit" } else { "rollback" };
                // Taken from the registry first, so a timeout here still drops
                // the handle — which closes the connection, which rolls back.
                with_db_timeout(label, async move {
                    let outcome = if committing {
                        tx.commit().await
                    } else {
                        tx.rollback().await
                    };
                    outcome.map_err(|e| format!("{op} failed: {e}"))
                })
                .await?;
                // A rollback leaves no trace: nothing was written.
                let audit = self.tx_writes.lock().await.remove(&id);
                if committing && let Some(audit) = audit {
                    let (trace_id, _) = Self::trace_context();
                    self.record_writes(
                        data_audit::ACTION_TX_COMMIT,
                        audit.writes,
                        trace_id.as_deref(),
                    )
                    .await;
                }
                Ok(serde_json::json!({ "ok": true }))
            }
            other => Err(format!("unknown op '{other}'")),
        }
    }

    /// `ctx.oltp.{query,exec}` — read or write the app's OWN per-org OLTP schema
    /// (`app_<writer>`) on the managed Postgres tenant, and nothing else.
    ///
    /// This is the write half `ctx.warehouse` could not give an app: for a
    /// `postgres_managed` database `ctx.warehouse` resolves the read-only
    /// analyst (org-wide read, `raw_*` included), so a write authenticates and
    /// then fails `permission denied`. `ctx.oltp` instead resolves the app's
    /// **writer** role, whose DML rights are scoped to the one `app_<writer>`
    /// schema — narrower on reads (no `raw_*`) and finally writable.
    ///
    /// Fail-closed on the `oltp` capability, gated by the OLTP kill-switch
    /// (`resolve_writer_connection_for_org` checks `oxy_oltp::flag`), and run in
    /// a one-shot transaction so parameters are bound (never string-concatenated)
    /// and a failed statement rolls back rather than leaving a partial write.
    async fn oltp(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let writer_name = match &self.caps.oltp {
            OltpCapability::Enabled { writer } => writer.as_str(),
            OltpCapability::Disabled => {
                return Err(
                    "OltpCapabilityMissing: this function has not declared the `oltp` capability \
                     (add \"oltp\": { \"enabled\": true } to its oxy-app.json entry). The schema \
                     it writes is derived from the app's own slug — the manifest only enables \
                     access."
                        .to_string(),
                );
            }
            // Enabled, but the slug can't back a schema — say THAT, not
            // "capability missing", which would send the author to a manifest
            // that already declares it.
            OltpCapability::SlugNotDerivable { slug } => {
                // Name every rule the slug can fail, INCLUDING no-underscore —
                // `app_writer_name` refuses `_` (it would alias a hyphenated
                // sibling onto one schema), which the identifier description
                // below does NOT cover, so a slug like `my_app` would otherwise
                // be refused against a message every clause of which it satisfies.
                return Err(format!(
                    "the capability is enabled, but this app's slug '{slug}' cannot back an \
                     OLTP schema: to do so a slug must start with a letter, be at most {max} \
                     characters, and use only lowercase letters, digits and hyphens (a `_` is \
                     refused — it would collide with the hyphenated form). A leading digit is a \
                     legal app slug but not a legal schema name. Rename the app to one that \
                     qualifies.",
                    max = oxy_oltp::schema::MAX_NAME_LEN,
                ));
            }
        };
        let (sql, params) = Self::oltp_statement(&payload)?;
        let summary = db_query_summary(&sql);
        let (trace_id, traceparent) = Self::trace_context();
        let tagged = data_audit::commented(&sql, &self.identity, traceparent.as_deref());
        // Resolve once per invocation, then reuse: N `ctx.oltp` calls cost one
        // control-plane resolve, not N. The isolate drives calls sequentially,
        // so holding this lock across the (first) resolve serialises nothing
        // real. The kill-switch is checked on that first resolve — an invocation
        // is short enough that a mid-run flag flip need not be observed. The
        // cached `WriterConnection` holds the DECRYPTED writer DSN for the rest
        // of the invocation; that is the same lifetime as the isolate that
        // already drives this credential, and the host is dropped when the
        // invocation ends, so the secret's window is not widened.
        let conn = {
            let mut cached = self.oltp_conn.lock().await;
            match cached.as_ref() {
                Some(c) => c.clone(),
                None => {
                    let writer = oxy_oltp::schema::WriterRef::app(writer_name)
                        .map_err(|e| format!("invalid writer '{writer_name}': {e}"))?;
                    let c = with_db_timeout("resolve", async {
                        oxy_oltp::resolver::resolve_writer_connection_for_org(
                            &self.db,
                            self.org_id,
                            &writer,
                        )
                        .await
                        .map_err(|e| {
                            // Names the app's own writer, and points at the org
                            // operator rather than a CLI the app author can't run.
                            format!(
                                "this app's OLTP store ('{writer_name}') is not \
                                 provisioned yet — ask whoever operates this org to provision \
                                 it: {e}"
                            )
                        })
                    })
                    .await?;
                    *cached = Some(c.clone());
                    c
                }
            }
        };
        // Verify the managed peer's certificate (see `WriterConnection::verify_tls`);
        // the DSN's `sslmode=require` only encrypts.
        let connector: Arc<dyn DatabaseConnector> = Arc::new(
            PostgresConnector::from_dsn(&conn.dsn, conn.verify_tls)
                .map_err(|e| format!("could not build a connection to '{}': {e}", conn.schema))?,
        );
        let mut tx = with_db_timeout("begin", async {
            connector
                .begin_transaction()
                .await
                .map_err(|e| format!("could not open a transaction: {e}"))
        })
        .await?;
        data_audit::record_db_span(&summary, Some(&conn.schema), None);
        // On error `tx` drops here, which closes the connection and rolls
        // the empty transaction back.
        self.name_session(&mut *tx, trace_id.as_deref()).await?;
        let mut rows_touched: Option<u64> = None;
        let outcome = match op.as_str() {
            "query" => with_db_timeout("query", async {
                tx.query(&tagged, &params).await.map_err(|e| e.to_string())
            })
            .await
            .map(|rows| {
                rows_touched = Some(rows.len() as u64);
                serde_json::json!({ "rows": rows })
            }),
            "exec" => with_db_timeout("exec", async {
                tx.exec(&tagged, &params).await.map_err(|e| e.to_string())
            })
            .await
            .map(|count| {
                rows_touched = Some(count);
                serde_json::json!({ "rowCount": count })
            }),
            other => {
                // Nothing ran; dropping `tx` rolls back the empty transaction.
                return Err(format!("unknown op '{other}'"));
            }
        };
        match outcome {
            Ok(result) => {
                with_db_timeout("commit", async move {
                    tx.commit().await.map_err(|e| format!("commit failed: {e}"))
                })
                .await?;
                if data_audit::is_write_verb(&summary.verb) {
                    self.note_write(WriteRecord {
                        plane: "oltp",
                        namespace: conn.schema.clone(),
                        verb: summary.verb,
                        table: summary.table,
                        rows: rows_touched,
                        statements: 1,
                    })
                    .await;
                }
                enforce_result_byte_cap(&result)?;
                Ok(result)
            }
            Err(e) => {
                // Roll back explicitly; a rollback failure is moot (the dropped
                // connection rolls back anyway) and must not mask the real error.
                let _ = tx.rollback().await;
                Err(format!("{op}: {e}"))
            }
        }
    }

    async fn secrets_set(&self, key: String, value: String) -> Result<serde_json::Value, String> {
        // §11.x fail-closed: the function must declare the `secrets.write`
        // capability. Scoped to the app's own `apps/<app_id>/` namespace, so it
        // can never touch another app's or the project's secrets.
        if !self.caps.secrets_write {
            return Err(
                "ctx.secrets.set: this function has not declared the `secrets.write` \
                 capability (add it to oxy-app.json to permit writes)"
                    .to_string(),
            );
        }
        SecretManagerService::new(self.project_id)
            .set_app_secret(&self.db, self.app_id, &key, &value, self.actor)
            .await
            .map_err(|e| format!("ctx.secrets.set failed: {e}"))?;
        Ok(serde_json::json!({ "ok": true }))
    }

    /// `ctx.org.people()` — who is in this org, by name.
    ///
    /// # Why this exists
    ///
    /// An app that models work assigned to humans had no way to name one. A
    /// function cannot call `GET /api/orgs/{org}/members`, so apps invent their
    /// own user ids and hold no human name at all — one of them drops a
    /// "who conducted this" column rather than guess, and another breaks its
    /// performance dashboard down by ROLE because a person is not available.
    /// Every such app grows its own fiction, and the fictions do not agree.
    ///
    /// # What it deliberately does not return
    ///
    /// No email, no phone. It answers with a display name, a role and a
    /// role — enough to put somebody on a roster, name an assignee, or label a
    /// submission. NOT a location: the platform holds no location for an org
    /// member, and the only place one exists is an app's own `staff` table, so
    /// promising one here would be a field this query cannot fill.
    ///
    /// # Who is in it
    ///
    /// **People who can reach this app** — one rule, both halves. Org members,
    /// who reach an org-visible app by membership; and frontline workers holding
    /// an explicit `app_members` grant on this app, which is already the only
    /// way a worker reaches anything.
    ///
    /// Each carries `kind: "member" | "frontline"`, and that field is the point:
    /// a caller that must not name a worker — a document's approver, say — can
    /// refuse structurally instead of by convention, and one that should name
    /// them (a submission's author, a roster row) does not have to make a second
    /// call and union it themselves. Two callers unioning by hand is how two
    /// people models start, which is the thing this capability exists to end.
    ///
    /// It is not a widening: every name here belongs to somebody who can already
    /// USE the app. A suspended worker is out, by the same column the login and
    /// the kiosk roster read.
    ///
    /// Being able to NAME a colleague is a different need
    /// from being able to contact them off-platform, and only the first was
    /// blocking anything. A bundle runs code the tenant did not write; handing
    /// it the org's address book by default is not a thing to do quietly.
    ///
    /// Scoped to `self.org_id`, which the host resolved — never taken from the
    /// function, so a manifest cannot point this at another tenant.
    async fn org_people(&self) -> Result<serde_json::Value, String> {
        // Fail-closed, like every other capability here.
        if !self.caps.org_read {
            return Err(
                "OrgCapabilityMissing: this function has not declared the `org.read` \
                 capability (add \"org\": { \"read\": true } to its oxy-app.json entry)"
                    .to_string(),
            );
        }
        org_directory(&self.db, self.org_id, self.app_id).await
    }

    /// `ctx.org.places()` — the org's locations, hierarchy and external ids.
    /// Same capability as `people()`: a place is not a secret, and an app
    /// that shows "Clovis" needs the row before it knows whether the caller
    /// reaches it. The whole registry, not a reach-scoped slice.
    async fn org_places(&self) -> Result<serde_json::Value, String> {
        if !self.caps.org_read {
            return Err(org_capability_missing("places"));
        }
        org_places_directory(&self.db, self.org_id).await
    }

    /// `ctx.org.assignments()` — who holds which position where. Scoped like
    /// `people()`: the assignments of people who can reach this app, which is
    /// what makes it not a widening of the directory.
    async fn org_assignments(&self) -> Result<serde_json::Value, String> {
        if !self.caps.org_read {
            return Err(org_capability_missing("assignments"));
        }
        org_assignments_directory(&self.db, self.org_id, self.app_id).await
    }

    async fn send_email(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
        // Fail-closed: the function must declare the `email.send` capability.
        if !self.caps.email_send {
            return Err(
                "EmailCapabilityMissing: this function has not declared the `email.send` \
                 capability (add \"email\": { \"send\": true } to its oxy-app.json entry)"
                    .to_string(),
            );
        }
        // Per-invocation fan-out cap (this host serves exactly one invocation).
        let n = self
            .email_send_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if n > MAX_EMAILS_PER_INVOCATION {
            return Err(format!(
                "RateLimitExceeded: this invocation exceeded the \
                 {MAX_EMAILS_PER_INVOCATION}-email limit"
            ));
        }
        let parsed =
            serde_json::from_value(input).map_err(|e| format!("InvalidEmailPayload: {e}"))?;
        crate::emails::app_emailer::AppEmailer::from_env(self.app_name.clone())
            .send(parsed)
            .await
    }

    async fn storage(
        &self,
        op: String,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use crate::server::api::custom_apps_storage as st;

        check_storage_capability(&op, &self.caps)?;

        let str_field = |key: &str| payload.get(key).and_then(|v| v.as_str());
        let bool_field = |key: &str| payload.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        let u64_field = |key: &str| payload.get(key).and_then(|v| v.as_u64());
        let required = |field: &str| format!("ctx.storage.{op}: `{field}` is required");

        match op.as_str() {
            "getUploadUrl" => {
                // `pathname` is the general form; `filename` is the ergonomic
                // shorthand for "a human picked a file", which lands under
                // `uploads/`. Generated assets pass `pathname` directly.
                let pathname = match (str_field("pathname"), str_field("filename")) {
                    (Some(p), _) => p.to_string(),
                    (None, Some(f)) => format!("uploads/{f}"),
                    (None, None) => return Err(required("pathname")),
                };
                let content_length =
                    u64_field("contentLength").ok_or_else(|| required("contentLength"))?;
                // Org-level quota. Checked before the URL is minted, not after
                // the bytes land: a presigned PUT goes straight to S3, so this is
                // the last moment oxy can refuse it.
                st::quota::check_write_allowed(&self.db, self.org_id, content_length)
                    .await
                    .map_err(|e| e.to_string())?;
                let out = st::get_upload_url(
                    self.app_id,
                    &pathname,
                    str_field("contentType").unwrap_or(""),
                    content_length,
                    u64_field("expiresInSeconds"),
                    &self.caps.storage_retention,
                )
                .await
                .map_err(|e| e.to_string())?;
                serde_json::to_value(out).map_err(|e| e.to_string())
            }
            "getDownloadUrl" => {
                let key = str_field("key").ok_or_else(|| required("key"))?;
                let out = st::get_download_url(
                    self.app_id,
                    key,
                    u64_field("expiresInSeconds"),
                    bool_field("download"),
                )
                .await
                .map_err(|e| e.to_string())?;
                serde_json::to_value(out).map_err(|e| e.to_string())
            }
            "put" => {
                let pathname = str_field("pathname")
                    .or_else(|| str_field("key"))
                    .ok_or_else(|| required("pathname"))?;
                let body = str_field("body").ok_or_else(|| required("body"))?;
                // base64 is how a BINARY generated asset (PDF, PNG, Parquet)
                // crosses the isolate's JSON boundary; utf8 is the default for
                // text a function built itself.
                let bytes = match encoding_or(str_field("encoding"), "utf8") {
                    "base64" => decode_base64(body)
                        .map_err(|e| format!("ctx.storage.put: `body` is not valid base64: {e}"))?,
                    "utf8" => body.as_bytes().to_vec(),
                    other => {
                        return Err(format!(
                            "ctx.storage.put: unknown encoding '{other}' (expected 'utf8' or \
                             'base64')"
                        ));
                    }
                };
                // Checked unconditionally, matching the presigned path above.
                //
                // An earlier version exempted `allowOverwrite: true` on the
                // reasoning that refusing an overwrite pushes the caller to write
                // under a new name and grow the silo. That only holds when the key
                // already exists — and the flag does not assert it does. A
                // function that always passes `allowOverwrite: true` with fresh
                // pathnames would never have been checked at all, which is exactly
                // the runaway growth this gate exists to stop. The concern it was
                // guarding against is moot anyway: past the hard limit *every*
                // write is refused, so there is no cheaper name to escape to.
                st::quota::check_write_allowed(&self.db, self.org_id, bytes.len() as u64)
                    .await
                    .map_err(|e| e.to_string())?;
                let out = st::put(
                    self.app_id,
                    pathname,
                    bytes,
                    st::PutOptions {
                        content_type: str_field("contentType").map(str::to_string),
                        add_random_suffix: bool_field("addRandomSuffix"),
                        allow_overwrite: bool_field("allowOverwrite"),
                        cache_control_max_age: u64_field("cacheControlMaxAge"),
                    },
                    &self.caps.storage_retention,
                )
                .await
                .map_err(|e| e.to_string())?;
                serde_json::to_value(out).map_err(|e| e.to_string())
            }
            "get" => {
                let key = str_field("key").ok_or_else(|| required("key"))?;
                let encoding = encoding_or(str_field("encoding"), "utf8").to_string();
                if !matches!(encoding.as_str(), "utf8" | "base64") {
                    return Err(format!(
                        "ctx.storage.get: unknown encoding '{encoding}' (expected 'utf8' or \
                         'base64')"
                    ));
                }
                match st::get(self.app_id, key).await.map_err(|e| e.to_string())? {
                    Some((bytes, content_type)) => {
                        let size = bytes.len();
                        let body = if encoding == "base64" {
                            encode_base64(&bytes)
                        } else {
                            String::from_utf8_lossy(&bytes).into_owned()
                        };
                        Ok(serde_json::json!({
                            "body": body,
                            "contentType": content_type,
                            "size": size,
                            "encoding": encoding,
                        }))
                    }
                    None => Ok(serde_json::Value::Null),
                }
            }
            "head" => {
                let key = str_field("key").ok_or_else(|| required("key"))?;
                match st::head(self.app_id, key)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Some(meta) => serde_json::to_value(meta).map_err(|e| e.to_string()),
                    None => Ok(serde_json::Value::Null),
                }
            }
            "list" => {
                let out = st::list(
                    self.app_id,
                    str_field("prefix"),
                    u64_field("limit").map(|v| v as usize),
                    str_field("cursor").map(str::to_string),
                )
                .await
                .map_err(|e| e.to_string())?;
                serde_json::to_value(out).map_err(|e| e.to_string())
            }
            "delete" => {
                // One key or a batch, mirroring the object-store idiom.
                let keys: Vec<String> = match payload.get("keys") {
                    Some(serde_json::Value::Array(items)) => items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                    _ => vec![str_field("key").ok_or_else(|| required("key"))?.to_string()],
                };
                let deleted = st::delete(self.app_id, &keys)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::json!({ "deleted": deleted }))
            }
            "copy" => {
                let from = str_field("fromKey").ok_or_else(|| required("fromKey"))?;
                let to = str_field("toPathname").ok_or_else(|| required("toPathname"))?;
                let out = st::copy(self.app_id, from, to, bool_field("allowOverwrite"))
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::to_value(out).map_err(|e| e.to_string())
            }
            other => Err(format!("ctx.storage: unknown op '{other}'")),
        }
    }
}

/// Fail-closed capability gate for one `ctx.storage` op.
///
/// - `getUploadUrl` / `put` / `delete` change the silo → `storage.write`.
/// - `copy` reads its source and writes its destination → both.
/// - `getDownloadUrl` / `get` / `head` / `list` → `storage.read`.
/// - Any other op is refused by name with the dispatch's own unknown-op error,
///   whatever the function declared.
///
/// `delete` and `copy` used to fall through to the read branch, so a function
/// that declared only `storage.read` could destroy or overwrite objects in its
/// app's silo. There is no catch-all arm for the same reason: an op added to the
/// dispatch without an arm here is refused, rather than shipping gated on
/// `storage.read` alone.
fn check_storage_capability(op: &str, caps: &FunctionCapabilities) -> Result<(), String> {
    let (needs_read, needs_write) = match op {
        "getUploadUrl" | "put" | "delete" => (false, true),
        "copy" => (true, true),
        "getDownloadUrl" | "get" | "head" | "list" => (true, false),
        other => return Err(format!("ctx.storage: unknown op '{other}'")),
    };
    if needs_write && !caps.storage_write {
        return Err(
            "StorageCapabilityMissing: this function has not declared the `storage.write` \
             capability (add \"storage\": { \"write\": true } to its oxy-app.json entry)"
                .to_string(),
        );
    }
    if needs_read && !caps.storage_read {
        return Err(
            "StorageCapabilityMissing: this function has not declared the `storage.read` \
             capability (add \"storage\": { \"read\": true } to its oxy-app.json entry)"
                .to_string(),
        );
    }
    Ok(())
}

/// Normalize an author-supplied `encoding` field. Absent, empty, and
/// whitespace-only all mean "unspecified" → `default`, so `ctx.fetch`,
/// `ctx.storage`, and `ctx.email.send` attachments all read it the same way:
/// a stray space shouldn't behave differently per API, and `?? ""` in JS
/// shouldn't be a distinct error from omitting the field.
fn encoding_or<'a>(value: Option<&'a str>, default: &'a str) -> &'a str {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
}

/// Decode a base64 `ctx.storage` payload, tolerating whitespace/newlines a caller
/// may have wrapped it in. base64 is the only way binary can cross the isolate's
/// JSON op boundary.
fn decode_base64(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine as _;
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&compact))
}

fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Resolve a `ctx.fetch` request body to real bytes.
///
/// A request body reaches us as a JS string, so raw binary cannot ride in it:
/// every byte >= 0x80 is re-encoded as a multi-byte UTF-8 sequence and the far
/// end receives a corrupt file. `bodyEncoding: "base64"` decodes to real bytes
/// first, which is what makes uploading a file to an external system possible
/// at all — the mirror of the `encoding: "base64"` that already exists on the
/// response side, and the same contract as `ctx.storage.put`.
///
/// Deliberately a SEPARATE field from `encoding`: that one names the encoding
/// of the RESPONSE. One knob cannot mean both, because posting a binary body
/// and reading back JSON needs them set differently.
///
/// Split out of `fetch` so the decision is unit-testable without a live HTTP
/// round trip.
fn fetch_body_bytes(init: &serde_json::Value) -> Result<Option<Vec<u8>>, String> {
    let Some(body) = init.get("body").and_then(|b| b.as_str()) else {
        return Ok(None);
    };
    match encoding_or(init.get("bodyEncoding").and_then(|e| e.as_str()), "utf8") {
        "utf8" => Ok(Some(body.as_bytes().to_vec())),
        "base64" => decode_base64(body)
            .map(Some)
            .map_err(|e| format!("ctx.fetch: `body` is not valid base64: {e}")),
        other => Err(format!(
            "ctx.fetch: unknown bodyEncoding '{other}' (expected 'utf8' or 'base64')"
        )),
    }
}

/// Hard ceiling for a host DB op. The runtime's per-function timeout (≤300s) is
/// the primary bound, but on the timeout-grace path the isolate thread is
/// *detached* on the premise that each host op is individually bounded — only
/// true if the underlying call can't hang forever. This wraps the DB paths most
/// likely to hang: connector construction (`connect`, i.e. TLS/auth handshake),
/// query execution, and warehouse `execute_statement`. NOT yet wrapped: the
/// semantic compile (`resolve_and_compile`, a `spawn_blocking`) and the Airway
/// run seed — a hang in either can still outlive the backstop (follow-up). Set
/// above the max function timeout so it only fires as a backstop, never trips a
/// legitimate long `airwayStep`.
const HOST_DB_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(330);

/// Max `ctx.email.send` calls per function invocation — bounds email fan-out
/// from a single run. The per-send recipient cap lives in `AppEmailer`.
const MAX_EMAILS_PER_INVOCATION: usize = 20;

async fn with_db_timeout<T>(
    label: &str,
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(HOST_DB_OP_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "{label} exceeded the {}s host DB-op ceiling",
            HOST_DB_OP_TIMEOUT.as_secs()
        )),
    }
}

/// The connector's `ResultCap` already bounds a query's *raw* result in-pod
/// (≈256 MB), but that payload is then re-encoded to JSON, shipped over the
/// broker channel, and parsed into the V8 isolate heap (~3× the bytes) — and
/// for `ctx.query` it ultimately lands in a browser. Reject a result whose JSON
/// exceeds this budget with a clear, actionable error rather than spiking
/// isolate memory. (`ctx.queryStream` is the path for genuinely large scans; it
/// stays bounded by the connector `ResultCap` since ETL data must flow through.)
const FUNCTION_QUERY_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Cheap estimate of a value's JSON byte size — walks the tree once with no
/// allocation, so the byte cap doesn't need a throwaway full serialization (the
/// runtime serializes the value for real afterward; measuring with `to_vec`
/// would double-encode every result).
fn approx_json_bytes(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 20,
        serde_json::Value::String(s) => s.len() + 2,
        serde_json::Value::Array(a) => 2 + a.len() + a.iter().map(approx_json_bytes).sum::<usize>(),
        serde_json::Value::Object(o) => {
            2 + o.len()
                + o.iter()
                    .map(|(k, val)| k.len() + 4 + approx_json_bytes(val))
                    .sum::<usize>()
        }
    }
}

fn enforce_result_byte_cap(result: &serde_json::Value) -> Result<(), String> {
    let bytes = approx_json_bytes(result);
    if bytes > FUNCTION_QUERY_MAX_BYTES {
        return Err(format!(
            "query result too large (~{bytes} bytes > {FUNCTION_QUERY_MAX_BYTES} byte cap); \
             narrow the query (fewer columns / a tighter filter), aggregate in SQL, or use \
             ctx.queryStream"
        ));
    }
    Ok(())
}

/// Read a covering rollup and shape it like `ctx.semantic`'s warehouse answer.
///
/// The row cap is pushed into the SQL rather than applied after the fact: the
/// preagg connection is `open_in_memory()` with no `temp_directory`, so a
/// high-cardinality `group_by` would build a `serde_json::Value` per cell for
/// rows the cap then throws away, with nowhere to spill. `max_rows + 1` keeps
/// `truncated` exact instead of a `len() == max_rows` guess — the same
/// treatment both branches of the `/semantic-query` route give it, so a
/// function sees one row ceiling however the question was answered.
async fn read_rollup(
    preagg_sql: &str,
    source: &agentic_semantic::compile::PreaggSource,
    max_rows: usize,
) -> Result<serde_json::Value, String> {
    // The same wrap `/semantic-query` applies, called rather than copied: the
    // parity claim above only holds while the two produce identical SQL, and a
    // fourth hand-rolled copy is how that stops being true.
    let read_sql =
        crate::server::api::projects::semantic_query::wrap_with_limit(preagg_sql, max_rows + 1);
    let src = source.clone();
    let value = tokio::task::spawn_blocking(move || {
        agentic_semantic::preagg::execute_preagg_sql(&read_sql, &src)
    })
    .await
    .map_err(|e| format!("preagg task panicked: {e}"))?
    .map_err(|e| e.to_string())?;

    let mut rows = match value.get("rows") {
        Some(serde_json::Value::Array(rows)) => rows.clone(),
        _ => Vec::new(),
    };
    let truncated = rows.len() > max_rows;
    rows.truncate(max_rows);
    Ok(serde_json::json!({ "rows": rows, "truncated": truncated }))
}

/// Run the injected query executor with a one-row overfetch so `truncated` is
/// reported correctly: a result of exactly `max_rows` rows is NOT truncated,
/// but `exec.execute(.., max_rows)` alone can't distinguish "exactly
/// `max_rows` rows exist" from "there were more and the LIMIT cut it off".
/// Fetching `max_rows + 1` and trimming the extra row resolves the ambiguity.
async fn query_with_truncation(
    exec: &Arc<dyn FunctionQueryExecutor>,
    connector: Arc<dyn DatabaseConnector>,
    sql: &str,
    max_rows: usize,
) -> Result<(Vec<serde_json::Value>, bool), String> {
    let mut rows = with_db_timeout("query", exec.execute(connector, sql, max_rows + 1)).await?;
    let truncated = rows.len() > max_rows;
    rows.truncate(max_rows);
    Ok((rows, truncated))
}

/// Build an `INSERT INTO table (cols) VALUES (...), (...)` statement from
/// `{ table, rows: [{col: value, ...}, ...], conflictColumns?: [...] }`.
/// `upsert` additionally appends `ON CONFLICT (conflictColumns) DO UPDATE SET
/// col = EXCLUDED.col` for every non-key column — the Postgres/DuckDB
/// upsert syntax, which covers the destinations this is scoped to (§11.3).
/// [`upsert_support::check`] refuses the others by name before this is sent.
fn build_insert_sql(payload: &serde_json::Value, upsert: bool) -> Result<String, String> {
    let table = payload
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "`table` is required".to_string())?;
    let rows = payload
        .get("rows")
        .and_then(|v| v.as_array())
        .filter(|rows| !rows.is_empty())
        .ok_or_else(|| "`rows` must be a non-empty array".to_string())?;

    let first = rows[0]
        .as_object()
        .ok_or_else(|| "each row must be an object".to_string())?;
    let columns: Vec<String> = first.keys().cloned().collect();

    let mut values_sql = Vec::with_capacity(rows.len());
    for row in rows {
        let obj = row
            .as_object()
            .ok_or_else(|| "each row must be an object".to_string())?;
        let mut literals = Vec::with_capacity(columns.len());
        for col in &columns {
            let value = obj
                .get(col)
                .ok_or_else(|| format!("row missing column '{col}'"))?;
            literals.push(json_value_to_sql_literal(value));
        }
        values_sql.push(format!("({})", literals.join(", ")));
    }

    let quoted_columns: Vec<String> = columns.iter().map(|c| quote_ident(c)).collect();
    let mut sql = format!(
        "INSERT INTO {} ({}) VALUES {}",
        quote_ident(table),
        quoted_columns.join(", "),
        values_sql.join(", ")
    );

    if upsert {
        let conflict_columns: Vec<String> = payload
            .get("conflictColumns")
            .and_then(|v| v.as_array())
            .map(|cols| {
                cols.iter()
                    .filter_map(|c| c.as_str().map(quote_ident))
                    .collect()
            })
            .unwrap_or_default();
        if conflict_columns.is_empty() {
            return Err("warehouse.upsert: `conflictColumns` must be a non-empty array".into());
        }
        let conflict_set: std::collections::HashSet<&str> = payload["conflictColumns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str())
            .collect();
        let update_assignments: Vec<String> = columns
            .iter()
            .filter(|c| !conflict_set.contains(c.as_str()))
            .map(|c| {
                let q = quote_ident(c);
                format!("{q} = EXCLUDED.{q}")
            })
            .collect();
        sql.push_str(&format!(" ON CONFLICT ({}) ", conflict_columns.join(", ")));
        if update_assignments.is_empty() {
            sql.push_str("DO NOTHING");
        } else {
            sql.push_str(&format!("DO UPDATE SET {}", update_assignments.join(", ")));
        }
    }

    Ok(sql)
}

/// Double-quote an identifier, escaping embedded `"` (Postgres/DuckDB/
/// ANSI-style quoting — the destinations `ctx.warehouse` targets).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Render a JSON value as a SQL literal. Strings are single-quote escaped;
/// objects/arrays are serialized to a JSON string literal (for `jsonb`-typed
/// columns).
fn json_value_to_sql_literal(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            format!("'{}'", value.to_string().replace('\'', "''"))
        }
    }
}

pub fn into_arc(host: ProjectFunctionHost) -> Arc<dyn FunctionHost> {
    Arc::new(host)
}

/// First-layer SSRF check for `ctx.fetch` — rejects non-HTTPS, loopback,
/// private, link-local, and internal-suffix hosts *by inspecting the URL
/// string only*. A non-literal hostname that resolves to a private/internal
/// address (DNS rebinding) passes this check; that case is caught at connect
/// time by [`PublicOnlyDnsResolver`]. Mirrors `custom_apps_proxy`'s
/// `is_safe_upstream` (kept as a local copy to avoid widening that module's
/// API surface; the two should stay in sync).
fn is_safe_outbound(url: &reqwest::Url) -> bool {
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host_for_ip = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = host_for_ip.parse::<IpAddr>() {
        return is_public_ip(&ip);
    }
    let lower = host.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback"
    ) {
        return false;
    }
    const INTERNAL_SUFFIXES: &[&str] = &[
        ".internal",
        ".local",
        ".localdomain",
        ".svc",
        ".cluster.local",
    ];
    !INTERNAL_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

fn is_public_ip(ip: &IpAddr) -> bool {
    // Fold an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) back to IPv4 before
    // classifying. Otherwise `::ffff:169.254.169.254` (cloud metadata),
    // `::ffff:10.x`, `::ffff:127.0.0.1` have `segments()[0] == 0` and slip
    // through the v6 arm below as "public" — and Linux routes a v4-mapped
    // address on a v6 socket straight to the underlying internal IPv4.
    // `to_canonical()` (stable since 1.75) makes the v4 checks below apply.
    let ip = ip.to_canonical();
    if ip.is_loopback() || ip.is_unspecified() {
        return false;
    }
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_private() && !v4.is_link_local() && !v4.is_broadcast() && !v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            // NAT64 (`64:ff9b::/96`) and IPv4-compatible (`::/96`) addresses
            // embed a v4 address in the low 32 bits. `to_canonical()` folds
            // v4-*mapped* (`::ffff:a.b.c.d`) but NOT these, so an id like
            // `64:ff9b::169.254.169.254` (or `::10.0.0.5`) would otherwise pass
            // the v6 arm below as "public" while pointing at metadata / RFC1918
            // — reachable on any host with a NAT64 path. Re-run the v4 checks on
            // the embedded address instead of trusting the v6 prefix.
            if let Some(v4) = embedded_ipv4(&v6) {
                return is_public_ip(&IpAddr::V4(v4));
            }
            let seg = v6.segments()[0];
            let link_local = (seg & 0xffc0) == 0xfe80;
            let unique_local = (seg & 0xfe00) == 0xfc00;
            !link_local && !unique_local
        }
    }
}

/// Extract the IPv4 address embedded in an IPv4-compatible (`::/96`) or NAT64
/// (`64:ff9b::/96`) IPv6 address. Returns `None` for every other v6 address.
///
/// v4-*mapped* (`::ffff:0:0/96`) is deliberately excluded: [`is_public_ip`]
/// calls `to_canonical()` first, which already folds it to a real `IpAddr::V4`,
/// so it never reaches here as a v6 value. Both prefixes handled here are
/// non-global by design (IPv4-compatible is deprecated; NAT64 is a translation
/// prefix), so treating anything in them as an embedded v4 cannot mis-block a
/// legitimately-routable global v6 address.
fn embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let seg = v6.segments();
    let is_v4_compatible = seg[0..6] == [0, 0, 0, 0, 0, 0];
    let is_nat64 = seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0];
    if is_v4_compatible || is_nat64 {
        Some(std::net::Ipv4Addr::from(
            ((seg[6] as u32) << 16) | seg[7] as u32,
        ))
    } else {
        None
    }
}

/// Partition resolved socket addresses into the ones safe to connect to,
/// dropping any that point at a non-public IP. Factored out of the resolver so
/// the rebinding decision is unit-testable without real DNS. Returns `Err` with
/// a diagnostic when *every* resolved address is filtered out (the rebinding
/// case), so the caller surfaces a clear message instead of an opaque DNS miss.
fn keep_public_addrs(host: &str, resolved: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    let safe: Vec<SocketAddr> = resolved
        .into_iter()
        .filter(|addr| is_public_ip(&addr.ip()))
        .collect();
    if safe.is_empty() {
        return Err(format!("'{host}' resolves only to non-public addresses"));
    }
    Ok(safe)
}

/// Second-layer SSRF defense for `ctx.fetch`: a custom reqwest DNS resolver
/// that drops every resolved address pointing at a non-public IP. This closes
/// the DNS-rebinding vector [`is_safe_outbound`] can't see — a public hostname
/// whose A record resolves to `169.254.169.254` / `10.x` / `127.x`. Validation
/// happens at resolution (i.e. connect) time, so there is no resolve-then-
/// reconnect TOCTOU window, and reqwest connects only to an address we vetted.
#[derive(Debug)]
struct PublicOnlyDnsResolver;

impl reqwest::dns::Resolve for PublicOnlyDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            // Port 0: reqwest overrides it with the URL's port; we only care
            // about the resolved IPs here.
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0u16))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            let safe = keep_public_addrs(&host, resolved)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let addrs: reqwest::dns::Addrs = Box::new(safe.into_iter());
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── ctx.fetch request bodies ──────────────────────────────────────────

    #[test]
    fn fetch_body_absent_stays_absent_and_defaults_to_utf8() {
        assert_eq!(fetch_body_bytes(&json!({})).unwrap(), None);
        assert_eq!(
            fetch_body_bytes(&json!({ "body": "hello" })).unwrap(),
            Some(b"hello".to_vec())
        );
        // An explicit empty/whitespace bodyEncoding means "unspecified", the
        // same as every other encoding field (see `encoding_or`).
        assert_eq!(
            fetch_body_bytes(&json!({ "body": "hello", "bodyEncoding": "  " })).unwrap(),
            Some(b"hello".to_vec())
        );
    }

    #[test]
    fn fetch_body_base64_survives_bytes_that_utf8_would_corrupt() {
        // 0xFF is not valid UTF-8. Carried as a JS string it becomes U+FFFD and
        // the far end receives a corrupt file — the exact failure this option
        // exists to prevent. Note the assertion is that the bytes round-trip
        // EXACTLY, not merely that decoding succeeds.
        let raw = vec![0x50u8, 0x4B, 0x03, 0x04, 0xFF, 0x00, 0x80];
        let b64 = encode_base64(&raw);
        assert_eq!(
            fetch_body_bytes(&json!({ "body": b64, "bodyEncoding": "base64" })).unwrap(),
            Some(raw.clone())
        );
        // And the same string WITHOUT the flag is what corruption looks like:
        // the utf8 path hands on the base64 text itself, not the bytes.
        assert_ne!(
            fetch_body_bytes(&json!({ "body": encode_base64(&raw) })).unwrap(),
            Some(raw)
        );
    }

    #[test]
    fn fetch_body_rejects_bad_base64_and_unknown_encodings() {
        let err = fetch_body_bytes(&json!({ "body": "not!base64", "bodyEncoding": "base64" }))
            .unwrap_err();
        assert!(err.contains("not valid base64"), "{err}");
        let err = fetch_body_bytes(&json!({ "body": "x", "bodyEncoding": "hex" })).unwrap_err();
        assert!(err.contains("unknown bodyEncoding"), "{err}");
    }

    // ── ctx.storage capability gate ─────────────────────────────────────────

    fn storage_caps(read: bool, write: bool) -> FunctionCapabilities {
        FunctionCapabilities {
            storage_read: read,
            storage_write: write,
            ..Default::default()
        }
    }

    #[test]
    fn storage_delete_and_copy_are_refused_without_write() {
        // A read-only function must not be able to destroy or overwrite what
        // is in its app's silo. Both ops used to pass on `storage.read` alone.
        let read_only = storage_caps(true, false);
        for op in ["delete", "copy"] {
            let err = check_storage_capability(op, &read_only)
                .expect_err(&format!("`{op}` must be refused without storage.write"));
            assert!(err.starts_with("StorageCapabilityMissing"), "{op}: {err}");
            assert!(err.contains("`storage.write`"), "{op}: {err}");
        }
    }

    #[test]
    fn storage_copy_is_refused_without_read() {
        // `copy` reads its source as well as writing its destination, so write
        // alone is not enough. `delete` reads nothing, so write alone is.
        let write_only = storage_caps(false, true);
        let err = check_storage_capability("copy", &write_only)
            .expect_err("`copy` must be refused without storage.read");
        assert!(err.contains("`storage.read`"), "{err}");
        for op in ["delete", "put", "getUploadUrl"] {
            check_storage_capability(op, &write_only)
                .unwrap_or_else(|e| panic!("`{op}` needs only storage.write: {e}"));
        }
        // Holding both is what a copy needs.
        check_storage_capability("copy", &storage_caps(true, true)).expect("read+write copies");
    }

    #[test]
    fn storage_reads_work_with_read_only_and_writes_do_not() {
        let read_only = storage_caps(true, false);
        for op in ["get", "list", "head", "getDownloadUrl"] {
            check_storage_capability(op, &read_only)
                .unwrap_or_else(|e| panic!("`{op}` needs only storage.read: {e}"));
        }
        for op in ["put", "getUploadUrl"] {
            assert!(
                check_storage_capability(op, &read_only).is_err(),
                "`{op}` must be refused without storage.write"
            );
        }
    }

    #[test]
    fn storage_gate_is_closed_when_nothing_is_declared() {
        let none = FunctionCapabilities::default();
        for op in [
            "getUploadUrl",
            "getDownloadUrl",
            "put",
            "get",
            "head",
            "list",
            "delete",
            "copy",
        ] {
            assert!(
                check_storage_capability(op, &none).is_err(),
                "`{op}` must be refused with no storage capability"
            );
        }
    }

    #[test]
    fn storage_unknown_op_is_refused_by_name_whatever_is_declared() {
        // An op the gate does not name must not inherit the read gate: a future
        // write op (`move`) would ship gated on `storage.read` alone, and a typo
        // (`heads`) would be reported as a missing capability instead of by name.
        let none = FunctionCapabilities::default();
        let read_write = storage_caps(true, true);
        for op in ["move", "heads", ""] {
            for (label, caps) in [("no capabilities", &none), ("read+write", &read_write)] {
                let err = check_storage_capability(op, caps)
                    .expect_err(&format!("unknown op `{op}` must be refused ({label})"));
                assert_eq!(err, format!("ctx.storage: unknown op '{op}'"), "{label}");
            }
        }
    }

    // ── build_insert_sql / quote_ident / json_value_to_sql_literal ─────────

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("orders"), "\"orders\"");
        assert_eq!(quote_ident(r#"weird"col"#), "\"weird\"\"col\"");
    }

    #[test]
    fn oltp_capability_distinguishes_its_two_fail_closed_reasons() {
        // Gate off → Disabled, whatever the slug.
        assert!(matches!(
            OltpCapability::resolve(false, "bookings"),
            OltpCapability::Disabled
        ));
        // Gate on + derivable slug → Enabled, writer derived from the slug
        // (hyphens → underscores), NOT from the manifest.
        match OltpCapability::resolve(true, "oltp-bookings") {
            OltpCapability::Enabled { writer } => assert_eq!(writer, "oltp_bookings"),
            other => panic!("expected Enabled, got {other:?}"),
        }
        // Gate on but the slug can't back a schema → SlugNotDerivable (NOT
        // Disabled), so the author sees "your slug can't back a schema" rather
        // than "you didn't declare the capability" — the exact misdiagnosis this
        // enum exists to prevent. A leading digit, an over-long slug, and an
        // underscore (which `app_writer_name` refuses to keep the derivation
        // injective) all resolve here.
        for bad in [
            "2fa-app",
            "1password-sync",
            "my_app", // an `_` slug — refused so it can't alias `my-app`
            &"a".repeat(oxy_oltp::schema::MAX_NAME_LEN + 1),
        ] {
            match OltpCapability::resolve(true, bad) {
                OltpCapability::SlugNotDerivable { slug } => assert_eq!(slug, bad),
                other => panic!("expected SlugNotDerivable for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn json_value_to_sql_literal_covers_all_variants() {
        assert_eq!(json_value_to_sql_literal(&json!(null)), "NULL");
        assert_eq!(json_value_to_sql_literal(&json!(true)), "true");
        assert_eq!(json_value_to_sql_literal(&json!(42)), "42");
        assert_eq!(json_value_to_sql_literal(&json!(4.5)), "4.5");
        assert_eq!(json_value_to_sql_literal(&json!("plain")), "'plain'");
        assert_eq!(json_value_to_sql_literal(&json!("it's")), "'it''s'");
        // Arrays/objects round-trip through JSON with single quotes escaped.
        assert_eq!(
            json_value_to_sql_literal(&json!({"a": "b's"})),
            "'{\"a\":\"b''s\"}'"
        );
    }

    #[test]
    fn build_insert_sql_basic_insert() {
        let payload = json!({
            "table": "daily_rollup",
            "rows": [
                { "day": "2026-06-13", "store_id": 12, "total": 4821.5 },
            ],
        });
        let sql = build_insert_sql(&payload, false).unwrap();
        assert_eq!(
            sql,
            r#"INSERT INTO "daily_rollup" ("day", "store_id", "total") VALUES ('2026-06-13', 12, 4821.5)"#
        );
    }

    #[test]
    fn build_insert_sql_multi_row() {
        let payload = json!({
            "table": "t",
            "rows": [
                { "a": 1, "b": "x" },
                { "a": 2, "b": "y" },
            ],
        });
        let sql = build_insert_sql(&payload, false).unwrap();
        assert_eq!(
            sql,
            r#"INSERT INTO "t" ("a", "b") VALUES (1, 'x'), (2, 'y')"#
        );
    }

    #[test]
    fn build_insert_sql_requires_table() {
        let payload = json!({ "rows": [{ "a": 1 }] });
        let err = build_insert_sql(&payload, false).unwrap_err();
        assert!(err.contains("`table`"));
    }

    #[test]
    fn build_insert_sql_requires_non_empty_rows() {
        let payload = json!({ "table": "t", "rows": [] });
        let err = build_insert_sql(&payload, false).unwrap_err();
        assert!(err.contains("`rows`"));
    }

    #[test]
    fn build_insert_sql_requires_consistent_columns() {
        let payload = json!({
            "table": "t",
            "rows": [
                { "a": 1, "b": 2 },
                { "a": 1 },
            ],
        });
        let err = build_insert_sql(&payload, false).unwrap_err();
        assert!(err.contains("missing column 'b'"));
    }

    #[test]
    fn build_insert_sql_upsert_appends_on_conflict() {
        let payload = json!({
            "table": "daily_rollup",
            "rows": [{ "day": "2026-06-13", "store_id": 12, "total": 4821.5 }],
            "conflictColumns": ["day", "store_id"],
        });
        let sql = build_insert_sql(&payload, true).unwrap();
        assert_eq!(
            sql,
            r#"INSERT INTO "daily_rollup" ("day", "store_id", "total") VALUES ('2026-06-13', 12, 4821.5) ON CONFLICT ("day", "store_id") DO UPDATE SET "total" = EXCLUDED."total""#
        );
    }

    #[test]
    fn build_insert_sql_upsert_all_columns_in_conflict_does_nothing() {
        let payload = json!({
            "table": "t",
            "rows": [{ "a": 1, "b": 2 }],
            "conflictColumns": ["a", "b"],
        });
        let sql = build_insert_sql(&payload, true).unwrap();
        assert!(sql.ends_with("DO NOTHING"));
    }

    #[test]
    fn build_insert_sql_upsert_requires_conflict_columns() {
        let payload = json!({
            "table": "t",
            "rows": [{ "a": 1 }],
        });
        let err = build_insert_sql(&payload, true).unwrap_err();
        assert!(err.contains("conflictColumns"));
    }

    // ── is_safe_outbound / is_public_ip (ctx.fetch SSRF allowlist) ──────────

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn rejects_non_https() {
        assert!(!is_safe_outbound(&url("http://example.com")));
    }

    #[test]
    fn allows_public_https_host() {
        assert!(is_safe_outbound(&url("https://api.example.com/v1")));
    }

    #[test]
    fn rejects_loopback_and_private_hosts() {
        assert!(!is_safe_outbound(&url("https://localhost/")));
        assert!(!is_safe_outbound(&url("https://127.0.0.1/")));
        assert!(!is_safe_outbound(&url("https://10.0.0.5/")));
        assert!(!is_safe_outbound(&url("https://192.168.1.1/")));
        assert!(!is_safe_outbound(&url("https://[::1]/")));
    }

    #[test]
    fn rejects_internal_suffix_hosts() {
        assert!(!is_safe_outbound(&url("https://service.internal/")));
        assert!(!is_safe_outbound(&url("https://db.svc/")));
        assert!(!is_safe_outbound(&url("https://app.cluster.local/")));
        assert!(!is_safe_outbound(&url("https://box.localdomain/")));
    }

    #[test]
    fn allows_public_ip_literal() {
        assert!(is_safe_outbound(&url("https://93.184.216.34/")));
    }

    #[test]
    fn rejects_link_local_ip() {
        assert!(!is_safe_outbound(&url("https://169.254.1.1/")));
    }

    #[test]
    fn rejects_ipv4_mapped_ipv6_internal_literals() {
        // ::ffff:a.b.c.d must be canonicalized to IPv4 before classification —
        // otherwise the metadata endpoint (and RFC1918 / loopback) reach the
        // internal target via a v6-socket-routed v4-mapped address.
        assert!(!is_safe_outbound(&url(
            "https://[::ffff:169.254.169.254]/latest/meta-data/"
        )));
        assert!(!is_safe_outbound(&url("https://[::ffff:10.0.0.5]/")));
        assert!(!is_safe_outbound(&url("https://[::ffff:127.0.0.1]/")));
        // The primitive directly: mapped-internal is rejected, mapped-public
        // still allowed.
        assert!(!is_public_ip(
            &"::ffff:169.254.169.254".parse::<IpAddr>().unwrap()
        ));
        assert!(is_public_ip(
            &"::ffff:93.184.216.34".parse::<IpAddr>().unwrap()
        ));
    }

    #[test]
    fn nat64_and_v4_compatible_embedded_internal_ips_are_blocked() {
        // NAT64 (`64:ff9b::/96`) and IPv4-compatible (`::/96`) addresses embed a
        // v4 in the low 32 bits and are NOT folded by `to_canonical()`, so the
        // embedded v4 must be re-checked. An embedded metadata / RFC1918 / loopback
        // address must be rejected; an embedded public one still allowed.
        for internal in [
            "64:ff9b::169.254.169.254", // NAT64 → cloud metadata
            "64:ff9b::10.0.0.5",        // NAT64 → RFC1918
            "64:ff9b::127.0.0.1",       // NAT64 → loopback
            "::169.254.169.254",        // IPv4-compatible → cloud metadata
            "::10.0.0.5",               // IPv4-compatible → RFC1918
        ] {
            let ip = internal.parse::<IpAddr>().unwrap();
            assert!(!is_public_ip(&ip), "{internal} must be classed non-public");
            assert!(
                !is_safe_outbound(&url(&format!("https://[{internal}]/latest/meta-data/"))),
                "{internal} must be blocked by is_safe_outbound"
            );
        }
        // An embedded PUBLIC v4 behind NAT64 is still reachable.
        assert!(is_public_ip(
            &"64:ff9b::93.184.216.34".parse::<IpAddr>().unwrap()
        ));
    }

    // ── keep_public_addrs (DNS-rebinding defense) ───────────────────────────

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn keep_public_addrs_filters_private_and_metadata() {
        // A "public" hostname that resolves to a mix keeps only the public IP.
        let resolved = vec![sa("169.254.169.254:0"), sa("93.184.216.34:0")];
        let kept = keep_public_addrs("evil.example.com", resolved).unwrap();
        assert_eq!(kept, vec![sa("93.184.216.34:0")]);
    }

    #[test]
    fn keep_public_addrs_rejects_when_all_private() {
        // Pure rebinding: every resolved address is internal → hard error.
        let resolved = vec![sa("169.254.169.254:0"), sa("10.0.0.5:0"), sa("127.0.0.1:0")];
        let err = keep_public_addrs("evil.example.com", resolved).unwrap_err();
        assert!(err.contains("non-public"), "unexpected error: {err}");
    }

    #[test]
    fn keep_public_addrs_passes_all_public() {
        let resolved = vec![sa("93.184.216.34:0"), sa("1.1.1.1:0")];
        let kept = keep_public_addrs("api.example.com", resolved.clone()).unwrap();
        assert_eq!(kept, resolved);
    }

    /// Write a one-column Parquet with `n` rows and return its path (plus the
    /// tempdir guard the caller must keep alive).
    fn rollup_fixture(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("rollup.parquet");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT i AS n FROM range({n}) t(i)) TO '{}' (FORMAT PARQUET);",
            path.display()
        ))
        .unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn read_rollup_shapes_the_result_like_the_warehouse_branch() {
        let (_guard, path) = rollup_fixture(3);
        let sql = format!(
            "SELECT n FROM read_parquet('{}') ORDER BY n",
            path.display()
        );
        let result = read_rollup(
            &sql,
            &agentic_semantic::compile::PreaggSource::Local(path),
            10,
        )
        .await
        .expect("rollup read");
        let rows = result["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 3);
        // Objects keyed by column name — the same shape `query_with_truncation`
        // hands back, so a function cannot tell which tier answered.
        assert_eq!(rows[0]["n"], serde_json::json!(0));
        assert_eq!(result["truncated"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn read_rollup_caps_rows_and_flags_truncation() {
        let (_guard, path) = rollup_fixture(5);
        let sql = format!(
            "SELECT n FROM read_parquet('{}') ORDER BY n",
            path.display()
        );
        let result = read_rollup(
            &sql,
            &agentic_semantic::compile::PreaggSource::Local(path),
            2,
        )
        .await
        .expect("rollup read");
        assert_eq!(result["rows"].as_array().unwrap().len(), 2);
        assert_eq!(result["truncated"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn read_rollup_reports_a_missing_parquet_rather_than_zero_rows() {
        // The caller answers from the warehouse on this error. Returning an
        // empty result instead would be indistinguishable from a rollup that
        // genuinely has no rows.
        let missing = std::path::PathBuf::from("/nonexistent/rollup.parquet");
        let err = read_rollup(
            "SELECT 1",
            &agentic_semantic::compile::PreaggSource::Local(missing),
            10,
        )
        .await
        .expect_err("a missing Parquet must be an error");
        assert!(!err.is_empty());
    }
}

/// The people directory, as a free function so it can be tested.
///
/// `ProjectFunctionHost` needs a project context, connectors and a workspace to
/// construct; this needs a database, an org and an app. Splitting them is what
/// lets the scoping rule — the part with a decision in it — be driven against a
/// real database without standing up an isolate.
fn org_capability_missing(member: &str) -> String {
    format!(
        "OrgCapabilityMissing: this function has not declared the `org.read` capability \
         (add \"org\": {{ \"read\": true }} to its oxy-app.json entry) — ctx.org.{member}"
    )
}

/// `ctx.org.places()`'s query. The org's whole registry, name-sorted, each
/// place with its hierarchy and what every integration calls it.
pub async fn org_places_directory(
    db: &DatabaseConnection,
    org_id: Uuid,
) -> Result<serde_json::Value, String> {
    let rows = crate::server::api::operating_graph::locations::location_rows(db, org_id)
        .await
        .map_err(|e| format!("ctx.org.places: {e}"))?;
    Ok(serde_json::json!({ "total": rows.len(), "places": rows }))
}

/// Who can reach this app, and the membership rows that said so.
pub struct Audience {
    pub ids: std::collections::HashSet<Uuid>,
    /// The org's members — every one is in `ids`. Returned so a caller that
    /// needs their roles does not read the table a second time.
    pub members: Vec<entity::org_members::Model>,
}

/// The people who can reach this app: org members, plus frontline workers
/// holding a grant on it — the one audience rule `people()` and
/// `assignments()` share, factored so it cannot drift between them.
pub async fn app_audience(
    db: &DatabaseConnection,
    org_id: Uuid,
    app_id: Uuid,
) -> Result<Audience, String> {
    use entity::{app_members, org_frontline_members, org_members};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    let members = org_members::Entity::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .all(db)
        .await
        .map_err(|e| format!("ctx.org: {e}"))?;
    let mut audience: std::collections::HashSet<Uuid> = members.iter().map(|m| m.user_id).collect();
    let workers: std::collections::HashSet<Uuid> = org_frontline_members::Entity::find()
        .filter(org_frontline_members::Column::OrgId.eq(org_id))
        .filter(org_frontline_members::Column::Status.eq(org_frontline_members::STATUS_ACTIVE))
        .all(db)
        .await
        .map_err(|e| format!("ctx.org: {e}"))?
        .into_iter()
        .map(|w| w.user_id)
        .collect();
    if !workers.is_empty() {
        let granted = app_members::Entity::find()
            .filter(app_members::Column::AppId.eq(app_id))
            .all(db)
            .await
            .map_err(|e| format!("ctx.org: {e}"))?;
        audience.extend(
            granted
                .into_iter()
                .map(|g| g.user_id)
                .filter(|u| workers.contains(u)),
        );
    }
    Ok(Audience {
        ids: audience,
        members,
    })
}

/// `ctx.org.assignments()`'s query: every assignment held by somebody in the
/// app's audience, with the names a screen shows.
pub async fn org_assignments_directory(
    db: &DatabaseConnection,
    org_id: Uuid,
    app_id: Uuid,
) -> Result<serde_json::Value, String> {
    use crate::server::api::operating_graph::{assignments, dto::AssignmentsQuery};
    let audience = app_audience(db, org_id, app_id).await?.ids;
    let rows: Vec<_> = assignments::rows(db, org_id, &AssignmentsQuery::default())
        .await
        .map_err(|e| format!("ctx.org.assignments: {e}"))?
        .into_iter()
        .filter(|a| audience.contains(&a.user_id))
        .map(|mut a| {
            // The supervisor is a person too. One the app may not name — an
            // area manager holding no grant on it — is withheld from the
            // row, not the row from the app: the assignment is still real.
            if a.supervisor_id.is_some_and(|s| !audience.contains(&s)) {
                a.supervisor_id = None;
                a.supervisor_name = None;
            }
            a
        })
        .collect();
    Ok(serde_json::json!({ "total": rows.len(), "assignments": rows }))
}

pub async fn org_directory(
    db: &DatabaseConnection,
    org_id: Uuid,
    app_id: Uuid,
) -> Result<serde_json::Value, String> {
    use entity::{org_frontline_members, users};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    // WHO is in the directory is `app_audience`'s decision — the one rule
    // `people()` and `assignments()` share. This function only decorates:
    // a name, an org role for a member, `kind` for both.
    let Audience {
        ids: audience,
        mut members,
    } = app_audience(db, org_id, app_id).await?;
    members.sort_by_key(|m| m.user_id);

    // The members' names, one batch; the membership rows came with the
    // audience, so `org_members` is read once.
    let member_ids: Vec<Uuid> = members.iter().map(|m| m.user_id).collect();
    let names: std::collections::HashMap<Uuid, String> = users::Entity::find()
        .filter(users::Column::Id.is_in(member_ids))
        .all(db)
        .await
        .map_err(|e| format!("ctx.org.people: {e}"))?
        .into_iter()
        .map(|u| (u.id, u.name))
        .collect();
    let mut people: Vec<serde_json::Value> = members
        .into_iter()
        .filter_map(|m| {
            // A membership whose user row is missing is data drift, not a
            // person. Dropped rather than emitted with a null name, because
            // a directory entry nobody can be is worse than a shorter list.
            let name = names.get(&m.user_id)?;
            Some(serde_json::json!({
                "id": m.user_id.to_string(),
                "name": name,
                "role": m.role.as_str(),
                "kind": "member",
            }))
        })
        .collect();

    // Frontline workers: in the audience only with a grant on THIS app, which
    // is the reason two apps in one org see different frontline slices — a
    // grant is per-app — and the reason this is not a widening: every name
    // here belongs to somebody who can already use the app.
    let workers = org_frontline_members::Entity::find()
        .filter(org_frontline_members::Column::OrgId.eq(org_id))
        .find_also_related(users::Entity)
        .order_by_asc(org_frontline_members::Column::UserId)
        .all(db)
        .await
        .map_err(|e| format!("ctx.org.people: {e}"))?;
    people.extend(
        workers
            .into_iter()
            .filter(|(w, _)| audience.contains(&w.user_id))
            .filter_map(|(_, u)| {
                let u = u?;
                Some(serde_json::json!({
                    "id": u.id.to_string(),
                    "name": u.name,
                    // `role` is the org-membership vocabulary and a worker has
                    // none, so it is null rather than an invented "worker" that
                    // would sort beside owner/admin/member as if it were one.
                    // `kind` is where the distinction lives.
                    "role": serde_json::Value::Null,
                    "kind": "frontline",
                }))
            }),
    );

    Ok(serde_json::json!({ "people": people, "total": people.len() }))
}
