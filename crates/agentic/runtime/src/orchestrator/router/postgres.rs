//! Postgres-backed [`TaskRouter`] using `LISTEN` / `NOTIFY`.
//!
//! ## Architecture
//!
//! - One **dedicated `tokio_postgres` connection** per process, separate
//!   from the SeaORM pool, running `LISTEN oxy_task_enqueued`. `LISTEN`
//!   is per-connection in Postgres, so a pooled connection that rotates
//!   would be unreliable.
//! - A long-lived **driver task** owns that connection. It pumps the
//!   `tokio_postgres::Connection` future (required for the client to
//!   work at all) and observes `AsyncMessage::Notification` events,
//!   which it translates into wakes on a shared `tokio::sync::Notify`.
//! - `wait_for_task` callers race a `notify.notified()` future against
//!   their own timeout. Spurious wakes are fine — the caller's job is
//!   to call `claim_task` and find out.
//! - `notify_enqueued` issues `SELECT pg_notify(...)` through a
//!   *separate* pooled connection (the SeaORM `DatabaseConnection`),
//!   so it composes with whatever transaction the caller is in. Postgres
//!   defers NOTIFY delivery until the issuing txn commits, so even if a
//!   caller commits *after* our `notify_enqueued`, listeners won't see
//!   a wake before the row is visible.
//!
//! ## Reconnection
//!
//! If the listener connection drops (server failover, OS network
//! reset, slow-query timeout), the driver task backs off and reconnects.
//! On reconnect it fires the local `Notify` once so any waiting worker
//! does a catch-up `claim_task` — this covers the window during which
//! notifications could have been missed.
//!
//! ## Credentials & the factory pattern
//!
//! Connection parameters arrive through a [`ListenerConfigFactory`]
//! closure that the reconnect loop calls *every iteration*. The
//! returned [`tokio_postgres::Config`] is consumed immediately for the
//! connect attempt. For password auth this closure trivially returns
//! the same parsed `Config`. For RDS IAM auth it mints a fresh SigV4
//! token and builds a `Config` with that token in the password slot.
//!
//! Important property: once the listener connection authenticates,
//! the token's TTL is moot — Postgres doesn't re-auth mid-stream. A
//! listener authed at t=0 with a 15-minute IAM token stays valid at
//! t=24h. We only need a fresh token at the *next reconnect*. That's
//! why this file has no background refresh loop: the reconnect path
//! already calls the factory at exactly the right moment.
//!
//! ## TLS
//!
//! Built once at construction via [`build_rustls_connector`] and
//! shared across every reconnect. The actual TLS handshake fires only
//! when the server offers it *and* the [`tokio_postgres::Config::ssl_mode`]
//! permits it — `Disable` skips it entirely, `Prefer` falls back to plain
//! on a plain-TCP server, `Require` insists on TLS.
//!
//! Certificate *verification* strictness is a separate axis, set by
//! [`TlsVerification`] and threaded in through
//! [`PostgresTaskRouterOptions`]. It must match the connection pool's
//! `OXY_DATABASE_SSL_MODE` handling: under `require` the pool encrypts
//! without validating the certificate, so the listener does the same
//! ([`TlsVerification::RequireNoVerify`]). Diverging here is what made
//! the listener's handshake fail against AWS RDS (Amazon RDS CA) and
//! in-cluster CloudNativePG (self-signed CA) while the pool connected
//! fine — neither CA is in the Mozilla `webpki-roots` bundle.
//!
//! ## What we deliberately don't do
//!
//! - **No per-class channels yet.** A single channel + ignored class
//!   filter keeps the wire protocol simple. When worker classes land,
//!   only this file changes — the trait and callers are already
//!   class-aware.
//! - **No payload data.** NOTIFY is "go look", not "here's your task."
//!   Storage is the source of truth.
//! - **No notification deduplication.** Postgres may deliver multiple
//!   NOTIFYs back-to-back; we wake once per notification and let
//!   `SKIP LOCKED` sort out the claim race.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use tokio::sync::Notify;
use tokio_postgres::AsyncMessage;
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_util::sync::CancellationToken;

use super::TaskRouter;

/// Postgres NOTIFY channel name. Single global channel for v1 — every
/// worker hears every enqueue regardless of class, and `SKIP LOCKED`
/// resolves which one actually claims. Per-class channels become a
/// concern only when we have meaningfully different claim filters.
pub const TASK_ENQUEUED_CHANNEL: &str = "oxy_task_enqueued";

/// Postgres NOTIFY channel used for end-to-end pipeline health probes.
///
/// Distinct from [`TASK_ENQUEUED_CHANNEL`] so the listener can ignore
/// probes when deciding whether to wake claim-loop waiters. The
/// background task fires probes on a slow tick; every listener
/// (on this instance + every peer) records the receipt time. A
/// stalled [`LAST_PROBE_RECEIVED_MILLIS`] — surfaced as the
/// `oxy_router_last_probe_received_timestamp_seconds` gauge on the
/// worker's `/metrics` — means the NOTIFY pipeline is silently
/// broken even if the connection looks healthy.
///
/// Do **not** watch the `router.health_probe_received` log event for
/// this. It is `debug` and prod runs at `OXY_LOG_LEVEL=info`, so it
/// flat-lines permanently and would detect nothing; see
/// [`PROBES_RECEIVED`] for why it stopped being `info`.
pub const HEALTH_PROBE_CHANNEL: &str = "oxy_health_probe";

/// Process-wide count of health probes received on
/// [`HEALTH_PROBE_CHANNEL`] by any listener in this process.
///
/// **Why a metric and not a log line.** The probe answers "is
/// LISTEN/NOTIFY still delivering?", which is a question about
/// *absence*: the healthy state produces no news. Encoding that as one
/// `info` line per receipt costs O(emitters x listeners) lines per
/// interval — every instance NOTIFYs on a 60s tick and every listener
/// on every instance logs it — so the volume grows quadratically with
/// fleet size. Measured 2026-09-09: 7,260 lines/hour in prod, **67% of
/// every log line the three oxy fleets emitted**, and 56% in dev.
///
/// It also put the signal somewhere no alert can read it. The line was
/// deliberately `info` "because ops alerts on absence of this event",
/// but the store it lands in is HyperDX, and
/// `infrastructure/docs/hyperdx.md` makes HyperDX the *investigation*
/// surface explicitly: no alerts, nothing load-bearing. Alerting is
/// Grafana + VictoriaMetrics, which never saw these lines. So the fleet
/// paid two thirds of its log budget for an alert that could not exist.
///
/// A gauge of "when did I last see one" is the same signal in one
/// series, in the store that does alert:
///
/// ```text
/// (time() - oxy_router_last_probe_received_timestamp_seconds) > 300
/// ```
///
/// Same shape, and the same reasoning, as `crud::TASKS_REQUEUED`: a
/// process-local static read directly by the exporter, because
/// `oxy-app`'s `/metrics` handler holds no reference to the router the
/// orchestrator built and `agentic-runtime` must never depend on
/// `oxy-app`. Exported by `oxy_app::server::worker_metrics`.
///
/// The per-receipt line is still emitted, at `debug`, so an interactive
/// `RUST_LOG=debug` session can still watch individual probes land.
pub static PROBES_RECEIVED: AtomicU64 = AtomicU64::new(0);

/// UNIX milliseconds of the most recent probe received by any listener
/// in this process. See [`PROBES_RECEIVED`] for the rationale.
///
/// `0` means no probe has been received since this process started.
/// The exporter renders that as an **absent series** rather than a zero
/// timestamp, because `time() - 0` is ~57 years and would page every
/// pod during the one probe interval that `background::start`
/// deliberately lets elapse before the first emission.
pub static LAST_PROBE_RECEIVED_MILLIS: AtomicI64 = AtomicI64::new(0);

/// Initial reconnect backoff. Doubles up to [`MAX_RECONNECT_BACKOFF`].
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(200);

/// Cap on reconnect backoff to avoid arbitrarily long outages when the
/// DB is slow to recover. 5s is long enough to avoid thrashing the
/// server during a real failover, short enough that the listener
/// rejoins promptly once the DB is healthy.
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Default for [`PostgresTaskRouterOptions::keepalive_interval`].
///
/// How often the listener pings its own connection with `SELECT 1` to
/// keep middleboxes (NAT, ELB, idle-timeout policies on RDS / PgBouncer)
/// from silently dropping it during quiet periods.
///
/// Sized comfortably under common idle-timeout defaults:
///   - AWS NLB / NAT GW: 350s before idle reset (RST sent on reuse).
///   - PgBouncer: configurable, often 600s.
///   - Cloud SQL / Aurora: typically 600-900s.
///
/// 5 min stays well inside all of these. The cost is ~1 round-trip per
/// app instance every 5 minutes — negligible. We pick `SELECT 1` over a
/// self-NOTIFY so listener-health and matcher-health stay independent;
/// task #16 will repurpose the NOTIFY channel for a proper end-to-end
/// probe.
pub const DEFAULT_LISTENER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(300);

/// TLS certificate-verification posture for the listener connection.
///
/// Mirrors the connection pool's `OXY_DATABASE_SSL_MODE` handling so the
/// router and the pool agree on how strict to be. tokio-postgres
/// delegates *all* certificate checking to the rustls connector, so this
/// is the only place the listener's verification strictness is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsVerification {
    /// Encrypt but do not validate the server certificate. Matches libpq
    /// `sslmode=require` / sqlx `PgSslMode::Require`. Required for servers
    /// whose CA isn't in the Mozilla bundle — AWS RDS (Amazon RDS CA) and
    /// in-cluster CloudNativePG (self-signed internal CA).
    RequireNoVerify,
    /// Full certificate chain + SAN/hostname verification against the
    /// Mozilla root bundle. Matches `sslmode=verify-full`.
    VerifyFull,
}

/// Tunables for [`PostgresTaskRouter::start_with_options`]. Use
/// [`Default`] for production; tests override individual fields.
#[derive(Debug, Clone)]
pub struct PostgresTaskRouterOptions {
    /// See [`DEFAULT_LISTENER_KEEPALIVE_INTERVAL`].
    pub keepalive_interval: Duration,
    /// TLS verification posture for the dedicated listener connection.
    /// Production callers derive this from `OXY_DATABASE_SSL_MODE` (see
    /// `listener_tls_verification_from_env` in the platform crate) so the
    /// listener matches the pool. Defaults to [`TlsVerification::VerifyFull`].
    pub tls_verification: TlsVerification,
}

impl Default for PostgresTaskRouterOptions {
    fn default() -> Self {
        Self {
            keepalive_interval: DEFAULT_LISTENER_KEEPALIVE_INTERVAL,
            tls_verification: TlsVerification::VerifyFull,
        }
    }
}

/// Closure called by the reconnect loop to produce a fresh
/// [`tokio_postgres::Config`] for each connection attempt.
///
/// - **Password auth:** the closure returns a clone of a `Config`
///   parsed once at startup from `OXY_DATABASE_URL`.
/// - **RDS IAM auth:** the closure awaits a fresh SigV4 token mint,
///   then builds a `Config` with that token as the password.
///
/// Failures inside the factory (e.g. AWS credential outage) come back
/// as `Err(String)` and trip the same reconnect-with-backoff path as
/// a TCP-level connect error. Callers should not panic on factory
/// failure — keep retrying.
pub type ListenerConfigFactory = Arc<
    dyn Fn() -> BoxFuture<'static, Result<tokio_postgres::Config, String>> + Send + Sync + 'static,
>;

/// Postgres-backed task router.
///
/// Construct via [`PostgresTaskRouter::start`], which spawns the
/// background listener task and returns a handle that's safe to clone
/// across worker constructions. Cancel the returned [`CancellationToken`]
/// at shutdown to stop the listener cleanly.
pub struct PostgresTaskRouter {
    /// Used for `notify_enqueued` and `emit_health_probe` (both call
    /// `SELECT pg_notify(...)` via SeaORM's pooled connection).
    /// Workers' main DB traffic uses the same pool — sharing is fine
    /// because `pg_notify` is a tiny query.
    db: DatabaseConnection,
    /// Wakes everyone parked in `wait_for_task`. The listener task
    /// calls `notify_waiters()` on every received `task_enqueued`
    /// notification + once per reconnect. Health probes update
    /// `last_probe_at_millis` instead — they do NOT wake workers,
    /// since a probe doesn't indicate new claimable work.
    notify: Arc<Notify>,
    /// UNIX millis of the most recent health probe this listener saw
    /// (from any instance, including itself). `0` means "never".
    /// Updated atomically from the driver task; readable from any
    /// thread for ops dashboards and test assertions.
    last_probe_at_millis: Arc<AtomicI64>,
    /// Stable per-router id stamped onto the probe payload so an
    /// instance can tell its own probes apart from peer probes in
    /// logs. Diagnostic only; not used for any logic.
    instance_id: String,
}

impl PostgresTaskRouter {
    /// Start the listener task and return the router.
    ///
    /// `db` is the SeaORM connection used for `notify_enqueued` only;
    /// LISTEN runs on its own dedicated connection minted via the
    /// `factory` closure each (re)connect. Cancelling the returned
    /// [`CancellationToken`] tears down the listener on shutdown.
    pub fn start(
        db: DatabaseConnection,
        factory: ListenerConfigFactory,
    ) -> (Arc<Self>, CancellationToken) {
        Self::start_with_options(db, factory, PostgresTaskRouterOptions::default())
    }

    /// Like [`Self::start`] but with configurable tunables. The default
    /// constructor calls this with `PostgresTaskRouterOptions::default()`;
    /// tests use it to shrink the keepalive interval so the keepalive
    /// path can be exercised without a multi-minute wait.
    pub fn start_with_options(
        db: DatabaseConnection,
        factory: ListenerConfigFactory,
        options: PostgresTaskRouterOptions,
    ) -> (Arc<Self>, CancellationToken) {
        let notify = Arc::new(Notify::new());
        let last_probe_at_millis = Arc::new(AtomicI64::new(0));
        let instance_id = format!("router-{}", uuid::Uuid::new_v4());
        let cancel = CancellationToken::new();

        let router = Arc::new(Self {
            db,
            notify: Arc::clone(&notify),
            last_probe_at_millis: Arc::clone(&last_probe_at_millis),
            instance_id: instance_id.clone(),
        });

        let listener_cancel = cancel.clone();
        let tls = build_rustls_connector(options.tls_verification);
        tokio::spawn(async move {
            run_listener(
                factory,
                tls,
                notify,
                last_probe_at_millis,
                listener_cancel,
                options,
            )
            .await;
        });

        (router, cancel)
    }

    /// UNIX time of the most recently received health probe on this
    /// listener (from any instance — self or peer). `None` means no
    /// probe has been seen since startup.
    ///
    /// External monitoring should alert when this grows stale (default
    /// production probe interval is 60s; alert at ~3 missed probes) —
    /// and now can: the same receipt updates the process-wide
    /// [`LAST_PROBE_RECEIVED_MILLIS`], which the worker's `/metrics`
    /// endpoint exports as
    /// `oxy_router_last_probe_received_timestamp_seconds`. This
    /// accessor stays for in-process callers and tests that hold the
    /// router; the static is what the exporter reads, because it has no
    /// handle on this instance.
    pub fn last_probe_received_at(&self) -> Option<SystemTime> {
        let m = self.last_probe_at_millis.load(Ordering::Relaxed);
        if m <= 0 {
            None
        } else {
            Some(UNIX_EPOCH + Duration::from_millis(m as u64))
        }
    }

    /// This router's stable instance id. Stamped onto the payload of
    /// every probe this instance emits so peer listeners can tell
    /// whose probe they're seeing in logs.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Convenience helper: build a factory from a static
    /// `postgres://` URL string. Used by password-auth callers and
    /// tests. Parses once and clones the resulting `Config` on every
    /// factory call.
    ///
    /// Returns `Err` only at construction time if the URL is malformed
    /// — runtime calls are infallible.
    pub fn password_factory_from_url(url: &str) -> Result<ListenerConfigFactory, String> {
        let config: tokio_postgres::Config = url
            .parse()
            .map_err(|e: tokio_postgres::Error| format!("invalid database url: {e}"))?;
        Ok(Arc::new(move || {
            let config = config.clone();
            Box::pin(async move { Ok(config) })
        }))
    }
}

#[async_trait]
impl TaskRouter for PostgresTaskRouter {
    async fn wait_for_task(&self, _classes: &[String], timeout: Duration) {
        // `notified()` must be created *before* we check / wait, because
        // it captures a permit position relative to current state. Race
        // it against the backstop timeout — whichever fires first.
        tokio::select! {
            _ = self.notify.notified() => {}
            _ = tokio::time::sleep(timeout) => {}
        }
    }

    async fn notify_enqueued(&self, class: Option<&str>) {
        let payload = class.unwrap_or("");
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT pg_notify($1, $2)",
            [TASK_ENQUEUED_CHANNEL.into(), payload.into()],
        );
        if let Err(e) = self.db.execute_raw(stmt).await {
            // Best-effort: a failed NOTIFY is a latency regression, not
            // a correctness bug — workers fall through to backstop poll
            // and pick up the task within the poll interval. Log and
            // move on so the caller's enqueue path isn't blocked.
            tracing::warn!(
                target: "router",
                channel = TASK_ENQUEUED_CHANNEL,
                error = %e,
                "pg_notify failed; workers will rely on backstop poll"
            );
        }
    }

    async fn emit_health_probe(&self) {
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT pg_notify($1, $2)",
            [HEALTH_PROBE_CHANNEL.into(), self.instance_id.clone().into()],
        );
        if let Err(e) = self.db.execute_raw(stmt).await {
            // Probe failures are operational signal, not correctness
            // issues. Log + carry on — the next tick will retry. Don't
            // bump tracing to error level; flapping AWS / DB issues
            // would spam alerts otherwise.
            tracing::warn!(
                target: "router.probe",
                channel = HEALTH_PROBE_CHANNEL,
                error = %e,
                "health-probe pg_notify failed"
            );
        }
    }
}

/// Open a dedicated LISTEN connection and hand the caller BOTH halves.
///
/// `LISTEN` notifications are only reachable by polling the connection future,
/// so a caller that wants them has to drive it — which is why this returns the
/// connection instead of spawning it the way a query client would.
///
/// It lives here because this module already owns the two decisions a listener
/// gets wrong on its own: the [`ListenerConfigFactory`] (which is what makes
/// RDS IAM auth work at all — the token is only checked at connect time, so
/// every reconnect needs a fresh one) and [`TlsVerification`] (our RDS and
/// CloudNativePG CAs are not in the Mozilla bundle, so `require` must mean
/// encrypt-without-validating rather than fail).
///
/// A second listener choosing either for itself would work on a laptop and fail
/// in production, which is the expensive shape of wrong. Today's second caller
/// is chat's cross-replica delivery listener.
pub async fn connect_listener(
    factory: &ListenerConfigFactory,
    verification: TlsVerification,
) -> Result<
    (
        tokio_postgres::Client,
        tokio_postgres::Connection<
            tokio_postgres::Socket,
            <MakeRustlsConnect as tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket>>::Stream,
        >,
    ),
    String,
>{
    let config = factory().await?;
    let tls = build_rustls_connector(verification);
    config
        .connect(tls)
        .await
        .map_err(|e| format!("listener connect failed: {e}"))
}

/// Build the shared rustls connector, honouring the requested
/// [`TlsVerification`] posture so the listener matches the connection
/// pool's `OXY_DATABASE_SSL_MODE` handling exactly.
///
/// - [`TlsVerification::VerifyFull`] verifies the full certificate chain
///   + SAN/hostname against the `webpki-roots` Mozilla CA bundle.
/// - [`TlsVerification::RequireNoVerify`] still encrypts (real TLS
///   handshake, real session keys) but accepts any server certificate —
///   matching libpq `sslmode=require` / sqlx `PgSslMode::Require`. This
///   is mandatory for our deployments: AWS RDS presents the Amazon RDS
///   CA and in-cluster CloudNativePG presents a self-signed internal CA,
///   neither of which is in the Mozilla bundle. The pool connects to
///   these same servers under `require` without validating the cert, so
///   the listener must do the same or its handshake fails where the
///   pool's succeeds.
///
/// Rustls 0.23 requires an explicit crypto provider; we install
/// `ring` as the process default. `install_default` errors on the
/// second call but doesn't panic, so the `let _ =` is safe and lets
/// multiple routers in the same process coexist (e.g. a test that
/// constructs more than one).
fn build_rustls_connector(verification: TlsVerification) -> MakeRustlsConnect {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let client_config = match verification {
        TlsVerification::VerifyFull => {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
        TlsVerification::RequireNoVerify => {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerification::new(provider)))
                .with_no_client_auth()
        }
    };

    MakeRustlsConnect::new(client_config)
}

/// rustls [`ServerCertVerifier`] that encrypts but does not validate the
/// server's certificate — the rustls equivalent of libpq
/// `sslmode=require`. The TLS handshake (key exchange + signature over
/// the handshake transcript) is still performed and verified; only the
/// certificate *chain / identity* check is skipped.
///
/// Used by [`TlsVerification::RequireNoVerify`]. See
/// [`build_rustls_connector`] for why our RDS / CloudNativePG backends
/// require this.
#[derive(Debug)]
struct NoCertVerification {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl NoCertVerification {
    fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self { provider }
    }
}

impl rustls::client::danger::ServerCertVerifier for NoCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Owns the dedicated listener connection. Reconnects with backoff
/// when the connection drops.
async fn run_listener(
    factory: ListenerConfigFactory,
    tls: MakeRustlsConnect,
    notify: Arc<Notify>,
    last_probe_at_millis: Arc<AtomicI64>,
    cancel: CancellationToken,
    options: PostgresTaskRouterOptions,
) {
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    loop {
        if cancel.is_cancelled() {
            return;
        }

        match listen_once(
            &factory,
            &tls,
            &notify,
            &last_probe_at_millis,
            &cancel,
            &options,
        )
        .await
        {
            ListenExit::Cancelled => {
                tracing::debug!(target: "router", "listener exited cleanly");
                return;
            }
            exit => {
                // Reconnect attempts are surfaced at warn (not info)
                // because a healthy production deployment should see
                // exactly one of these per Postgres failover. Repeated
                // logs at >30s cadence are a real signal.
                //
                // During shutdown, though, the co-located DB dies with the
                // process (Ctrl-C in local/dev), so a lost listener is
                // expected, not a signal — downgrade to debug so a clean
                // shutdown stays quiet.
                if crate::orchestrator::transport::is_shutting_down() {
                    tracing::debug!(
                        target: "router.reconnect",
                        reason = %exit,
                        "listener connection lost during shutdown; not a signal"
                    );
                } else {
                    tracing::warn!(
                        target: "router.reconnect",
                        reason = %exit,
                        backoff_ms = backoff.as_millis() as u64,
                        "listener connection lost; reconnecting"
                    );
                }
                // Wake any waiting worker once on disconnect — the
                // catch-up claim covers the window between when the
                // connection died and when we'll reconnect. Without
                // this, a notification arriving during the outage
                // would be lost without a wake.
                notify.notify_waiters();

                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => return,
                }
                backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
            }
        }
    }
}

/// Why `listen_once` exited. `Cancelled` is a clean shutdown; the
/// `Factory`, `Pg`, and `ServerClosed` variants all trigger the
/// reconnect path.
#[derive(Debug)]
enum ListenExit {
    Cancelled,
    /// Factory failed to produce a `Config` — typically AWS credential
    /// outage in IAM mode. We can't connect without one, so wait and
    /// retry on the next backoff tick.
    Factory(String),
    Pg(tokio_postgres::Error),
    ServerClosed,
}

impl std::fmt::Display for ListenExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenExit::Cancelled => write!(f, "cancelled"),
            ListenExit::Factory(msg) => write!(f, "factory failed: {msg}"),
            ListenExit::Pg(e) => write!(f, "{e}"),
            ListenExit::ServerClosed => write!(f, "server closed connection"),
        }
    }
}

impl From<tokio_postgres::Error> for ListenExit {
    fn from(e: tokio_postgres::Error) -> Self {
        ListenExit::Pg(e)
    }
}

/// One connection lifetime: mint config, connect, LISTEN, pump until
/// disconnect or cancel. Returns `Cancelled` on clean shutdown, any
/// other variant triggers the reconnect path.
///
/// `tokio_postgres::Client` methods (like `batch_execute`) only make
/// progress if the `Connection` future is *concurrently* being polled
/// — they communicate via an internal channel. We can't poll the
/// connection inline before LISTEN because `batch_execute(LISTEN)` is
/// the very thing that needs the connection running. The standard
/// fix is to spawn the connection driver as a sibling task.
///
/// That sibling task pulls async messages off the connection and
/// forwards `Notification`s to the shared `Notify`. It also doubles
/// as the disconnect detector: when the stream ends, the task sends
/// the reason through a oneshot, and the main task here translates
/// that into a `ListenExit` for the reconnect loop.
async fn listen_once(
    factory: &ListenerConfigFactory,
    tls: &MakeRustlsConnect,
    notify: &Arc<Notify>,
    last_probe_at_millis: &Arc<AtomicI64>,
    cancel: &CancellationToken,
    options: &PostgresTaskRouterOptions,
) -> ListenExit {
    // Mint fresh config (and, for IAM, a fresh token). Factory failure
    // is just another reason to back off and retry.
    let config = match factory().await {
        Ok(c) => c,
        Err(e) => return ListenExit::Factory(e),
    };

    let (client, mut connection) = match config.connect(tls.clone()).await {
        Ok(c) => c,
        Err(e) => return ListenExit::Pg(e),
    };

    // Drive the connection in a separate task. The task owns the
    // `Connection`, dispatches notifications by channel (waking
    // workers for task_enqueued, recording timestamps for health
    // probes), and sends its exit reason (server-close vs error)
    // when the stream dries up. Cancelling the parent (via
    // `cancel.cancelled()`) is signalled by aborting its JoinHandle.
    let driver_notify = Arc::clone(notify);
    let driver_probe = Arc::clone(last_probe_at_millis);
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<ListenExit>();
    let driver = tokio::spawn(async move {
        let mut stream = futures::stream::poll_fn(move |cx| connection.poll_message(cx));
        let exit = loop {
            match stream.next().await {
                Some(Ok(AsyncMessage::Notification(n))) => match n.channel() {
                    TASK_ENQUEUED_CHANNEL => {
                        // Trace event so a flat-line metric on
                        // `router.notification.delivered` flags a
                        // sick listener even while the connection
                        // looks alive. Channel + payload are tiny
                        // strings, safe to log unconditionally.
                        tracing::trace!(
                            target: "router",
                            channel = %n.channel(),
                            payload = %n.payload(),
                            "notification delivered"
                        );
                        // Wake all parked waiters. `SKIP LOCKED`
                        // resolves which one actually claims.
                        driver_notify.notify_waiters();
                    }
                    HEALTH_PROBE_CHANNEL => {
                        let now_millis = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        driver_probe.store(now_millis, Ordering::Relaxed);
                        // The alertable form of this signal is the
                        // pair of process-wide statics, exported as
                        // Prometheus series by the worker's /metrics
                        // endpoint and alerted on in VictoriaMetrics.
                        // See PROBES_RECEIVED for why this is a
                        // metric and no longer an `info` line.
                        PROBES_RECEIVED.fetch_add(1, Ordering::Relaxed);
                        LAST_PROBE_RECEIVED_MILLIS.store(now_millis, Ordering::Relaxed);
                        // DEBUG, not info: one line per receipt is
                        // O(emitters x listeners) per interval and
                        // was 67% of prod's oxy log volume. Kept at
                        // debug so `RUST_LOG=debug` can still watch
                        // individual probes land during a triage.
                        tracing::debug!(
                            target: "router.health_probe_received",
                            from = %n.payload(),
                            "health probe received"
                        );
                        // Deliberately do NOT call notify_waiters:
                        // a probe doesn't represent claimable work.
                    }
                    other => {
                        // Unknown channel — we LISTEN on a closed
                        // set so this only fires if Postgres routes
                        // something unexpected. Log + ignore.
                        tracing::debug!(
                            target: "router",
                            channel = %other,
                            "ignoring notification on unsubscribed channel"
                        );
                    }
                },
                Some(Ok(AsyncMessage::Notice(notice))) => {
                    tracing::debug!(
                        target: "router",
                        message = %notice.message(),
                        "postgres notice"
                    );
                }
                Some(Ok(_)) => {
                    // `AsyncMessage` is `#[non_exhaustive]`; tolerate
                    // future variants by ignoring them.
                }
                Some(Err(e)) => break ListenExit::Pg(e),
                None => break ListenExit::ServerClosed,
            }
        };
        let _ = exit_tx.send(exit);
    });

    // Subscribe to both channels in one batch. Server-side
    // bookkeeping is per-channel, so we issue two LISTEN commands;
    // semicolon-joining them in a single batch_execute lets the
    // driver task observe them as one atomic round-trip.
    if let Err(e) = client
        .batch_execute(&format!(
            "LISTEN {TASK_ENQUEUED_CHANNEL}; LISTEN {HEALTH_PROBE_CHANNEL}"
        ))
        .await
    {
        driver.abort();
        return ListenExit::Pg(e);
    }
    tracing::info!(
        target: "router",
        task_channel = TASK_ENQUEUED_CHANNEL,
        probe_channel = HEALTH_PROBE_CHANNEL,
        "listener connected; LISTEN issued"
    );
    // On a fresh connection, wake anyone waiting to do an immediate
    // catch-up `claim_task`. If a row was enqueued during the gap
    // between connection death and now, the corresponding NOTIFY is
    // lost server-side; this wake gives waiters one chance to find
    // the row themselves.
    notify.notify_waiters();

    // Keepalive ticker. First tick fires after the full interval (not
    // immediately) — `Burst` would re-fire missed ticks back-to-back
    // if the select! loop ever fell behind, which would just hammer
    // the DB pointlessly. `Skip` is the right semantic for a liveness
    // probe.
    let mut keepalive = tokio::time::interval(options.keepalive_interval);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Drain the always-immediate first tick — we just connected; no
    // value in pinging right away.
    keepalive.tick().await;

    let mut exit_rx = exit_rx;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Drop the client so the server-side connection drops too.
                // Aborting the driver is what stops `poll_message` from
                // pumping any further; the `Connection` it owns is dropped
                // with the task, which closes the socket.
                drop(client);
                driver.abort();
                return ListenExit::Cancelled;
            }
            result = &mut exit_rx => {
                // The driver exited (server closed or PG error). The client
                // is now useless; drop it. Returning the exit reason
                // propagates the reconnect signal upward.
                drop(client);
                return result.unwrap_or(ListenExit::ServerClosed);
            }
            _ = keepalive.tick() => {
                // Tiny ping to keep middleboxes from idling the
                // socket out. A failure here means the connection is
                // already broken — propagate as a Pg exit so the
                // reconnect loop kicks in *now* rather than waiting
                // for the next NOTIFY-driven activity to discover it.
                if let Err(e) = client.simple_query("SELECT 1").await {
                    tracing::warn!(
                        target: "router.keepalive",
                        error = %e,
                        "listener keepalive ping failed; triggering reconnect"
                    );
                    drop(client);
                    driver.abort();
                    return ListenExit::Pg(e);
                }
                tracing::trace!(
                    target: "router.keepalive",
                    interval_secs = options.keepalive_interval.as_secs(),
                    "listener keepalive ping ok"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    /// `RequireNoVerify` must accept a certificate it cannot chain to any
    /// trust root — that's the whole point. This is the regression guard
    /// for the listener flapping `error performing TLS handshake` against
    /// AWS RDS / CloudNativePG, whose CAs aren't in the Mozilla bundle.
    #[test]
    fn no_verify_verifier_accepts_untrusted_cert() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = NoCertVerification::new(provider);

        // Bytes don't need to be a real cert — verify_server_cert must
        // not look at them under the no-verify posture.
        let cert = CertificateDer::from(vec![0x30, 0x00]);
        let server_name =
            ServerName::try_from("oxy-staging-postgres.example.com").expect("valid server name");

        let result = verifier.verify_server_cert(&cert, &[], &server_name, &[], UnixTime::now());

        assert!(
            result.is_ok(),
            "RequireNoVerify must accept an untrusted certificate; got {result:?}"
        );
    }

    /// Both postures must build a connector without panicking (covers the
    /// crypto-provider install + the dangerous verifier wiring).
    #[test]
    fn build_rustls_connector_builds_for_both_postures() {
        let _ = build_rustls_connector(TlsVerification::RequireNoVerify);
        let _ = build_rustls_connector(TlsVerification::VerifyFull);
    }
}
