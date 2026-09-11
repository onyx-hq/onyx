//! Process-level background jobs that the agentic runtime needs to
//! keep healthy.
//!
//! Three responsibilities, deliberately co-located in a single task so
//! we don't accumulate background slots over time:
//!
//! 1. **Reaper.** Periodically calls
//!    [`crate::orchestrator::crud::reap_stale_tasks`] to free tasks
//!    claimed by workers that died without cleanup (OOM, panic,
//!    network partition, hung kernel). Without this, a single dead
//!    worker permanently strands every task it held until the next
//!    server restart triggers
//!    [`crate::orchestrator::transport::DurableTransport::run_reaper`]
//!    as a one-shot pre-pass — which is what the system *used* to
//!    rely on exclusively, and which silently lost work between
//!    restarts.
//!
//! 2. **Matcher health probe.** Periodically fires a self-NOTIFY on
//!    [`crate::orchestrator::router::HEALTH_PROBE_CHANNEL`] so every listener (on
//!    this instance and any peer) can observe that the LISTEN/NOTIFY
//!    pipeline is end-to-end functional. Catches the failure mode
//!    where the connection looks healthy (keepalive succeeds) but
//!    server-side notification delivery has stalled. The router
//!    records `last_probe_received_at` on every receipt and bumps the
//!    process-wide [`crate::orchestrator::router::PROBES_RECEIVED`] /
//!    [`crate::orchestrator::router::LAST_PROBE_RECEIVED_MILLIS`],
//!    which the worker's `/metrics` endpoint exports as
//!    `oxy_router_last_probe_received_timestamp_seconds`. Alerting is a
//!    staleness expression on that gauge in VictoriaMetrics. It used to
//!    be absence-detection over an `info` log line, which cost O(peers
//!    squared) lines per interval and landed in a store that carries no
//!    alerts — see `PROBES_RECEIVED` for the measurements.
//!
//! ## Why one task, not three
//!
//! All cadences are slow (reaper ~30s, probe ~60s) and none are
//! latency-sensitive. Splitting them into separate tokio tasks would
//! buy nothing and add extra `CancellationToken`s to pass around at
//! shutdown. A single `select!` loop with a couple of tickers is
//! simpler to reason about and stop cleanly.

use std::sync::Arc;
use std::time::Duration;

use sea_orm::DatabaseConnection;
use tokio_util::sync::CancellationToken;

use crate::crud;
use crate::orchestrator::router::TaskRouter;

/// How often the reaper scans `agentic_task_queue` for stale claims.
///
/// 30s is the recovery RTO — a worker that dies takes at most one
/// `visibility_timeout_secs` (60s default) + one reaper interval
/// before its tasks reappear as `queued`. Total worst case: ~90s
/// from worker death to another worker picking up the dropped task.
/// Tightening to 5s would shave that to ~65s at the cost of 6× more
/// queries; not worth it for the failure mode (worker crashes are
/// rare in steady state).
pub const REAPER_INTERVAL: Duration = Duration::from_secs(30);

/// How often we self-NOTIFY on the matcher health-probe channel.
///
/// 60s is the alert resolution: monitors typically alarm on three
/// missed probes (~3 min of silence), which is short enough to catch
/// a stuck listener in time to bounce the pod, long enough that a
/// minute-long blip during failover doesn't page anyone.
pub const HEALTH_PROBE_INTERVAL: Duration = Duration::from_secs(60);

/// Tunables for [`start_with_options`]. Production uses [`Default`];
/// tests shrink the intervals so probe / reap behaviour can be
/// exercised inside a fast suite.
#[derive(Debug, Clone)]
pub struct BackgroundJobsOptions {
    pub reaper_interval: Duration,
    pub health_probe_interval: Duration,
}

impl Default for BackgroundJobsOptions {
    fn default() -> Self {
        Self {
            reaper_interval: REAPER_INTERVAL,
            health_probe_interval: HEALTH_PROBE_INTERVAL,
        }
    }
}

/// How often the retention prune scans `agentic_task_queue` for old terminal
/// rows. Hourly — pruning is housekeeping, not latency-sensitive like the
/// reaper.
pub const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);
/// Default age after which `completed`/`cancelled` rows are deleted.
pub const DEFAULT_COMPLETED_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default age after which `failed`/`dead` rows are deleted — longer, because
/// they're the dead-letter triage surface.
pub const DEFAULT_DEAD_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);

const RETENTION_INTERVAL_ENV: &str = "OXY_TASK_QUEUE_RETENTION_INTERVAL_SECS";
const COMPLETED_RETENTION_ENV: &str = "OXY_TASK_QUEUE_RETENTION_DAYS";
const DEAD_RETENTION_ENV: &str = "OXY_TASK_QUEUE_DEAD_RETENTION_DAYS";

/// Operator-tunable retention for the internal-jobs queue. A `None` window
/// means "keep that class forever" (set the env var to `0`/`off`); both `None`
/// disables the prune loop entirely.
#[derive(Debug, Clone)]
pub struct RetentionConfig {
    pub interval: Duration,
    pub completed_ttl: Option<Duration>,
    pub dead_ttl: Option<Duration>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval: RETENTION_INTERVAL,
            completed_ttl: Some(DEFAULT_COMPLETED_RETENTION),
            dead_ttl: Some(DEFAULT_DEAD_RETENTION),
        }
    }
}

impl RetentionConfig {
    /// Read retention settings from the environment, falling back to
    /// [`Default`]. `OXY_TASK_QUEUE_RETENTION_DAYS` /
    /// `OXY_TASK_QUEUE_DEAD_RETENTION_DAYS` accept a day count, or `0`/`off` to
    /// keep that class forever; `OXY_TASK_QUEUE_RETENTION_INTERVAL_SECS`
    /// (min 60) sets the sweep cadence.
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            interval: std::env::var(RETENTION_INTERVAL_ENV)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&n| n >= 60)
                .map(Duration::from_secs)
                .unwrap_or(d.interval),
            completed_ttl: ttl_days_from_env(COMPLETED_RETENTION_ENV, d.completed_ttl),
            dead_ttl: ttl_days_from_env(DEAD_RETENTION_ENV, d.dead_ttl),
        }
    }

    /// True when at least one class is being pruned.
    pub fn enabled(&self) -> bool {
        self.completed_ttl.is_some() || self.dead_ttl.is_some()
    }
}

/// Parse a `<env>=<days>` retention window. `0` / `off` / `never` → `None`
/// (keep forever); unset or unparseable → `default`.
fn ttl_days_from_env(var: &str, default: Option<Duration>) -> Option<Duration> {
    match std::env::var(var) {
        Ok(raw) => {
            let v = raw.trim().to_lowercase();
            if v == "0" || v == "off" || v == "never" {
                return None;
            }
            v.parse::<u64>()
                .ok()
                .filter(|&n| n > 0)
                .map(|days| Duration::from_secs(days * 24 * 3600))
                .or(default)
        }
        Err(_) => default,
    }
}

/// Spawn the process-level background jobs.
///
/// Call this once at app startup, after migrations have run. Holds
/// the returned [`CancellationToken`] for the lifetime of the
/// process; cancel it on shutdown to stop the loop cleanly.
pub fn start(db: DatabaseConnection, router: Arc<dyn TaskRouter>) -> CancellationToken {
    start_with_options(db, router, BackgroundJobsOptions::default())
}

/// Like [`start`] but with configurable intervals. Test-facing.
pub fn start_with_options(
    db: DatabaseConnection,
    router: Arc<dyn TaskRouter>,
    options: BackgroundJobsOptions,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();

    tracing::info!(
        target: "background",
        reaper_interval_secs = options.reaper_interval.as_secs(),
        health_probe_interval_secs = options.health_probe_interval.as_secs(),
        "background jobs started: reaper + health probe"
    );

    tokio::spawn(async move {
        let mut reaper_tick = tokio::time::interval(options.reaper_interval);
        // `Skip` semantics: if the loop falls behind, don't fire
        // queued ticks back-to-back. A reaper is liveness-driven, not
        // count-driven.
        reaper_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut probe_tick = tokio::time::interval(options.health_probe_interval);
        // Same `Skip` semantics + drain the always-immediate first
        // tick. The probe's value is "X seconds since last seen", so
        // an at-startup probe is misleading (no listeners on peer
        // instances might be up yet anyway). Let one interval elapse
        // before the first emission so the signal is meaningful.
        probe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        probe_tick.tick().await;

        // Retention prune — only scheduled when at least one class has a
        // finite window. Drain the immediate first tick so we don't sweep
        // during the startup storm.
        let retention = RetentionConfig::from_env();
        let mut retention_tick = retention.enabled().then(|| {
            let mut t = tokio::time::interval(retention.interval);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            t
        });
        if let Some(t) = retention_tick.as_mut() {
            t.tick().await;
        }

        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => {
                    tracing::debug!(target: "background", "background jobs cancelled");
                    return;
                }
                _ = reaper_tick.tick() => {
                    run_reaper_cycle(&db).await;
                }
                _ = probe_tick.tick() => {
                    router.emit_health_probe().await;
                }
                // A `None` tick parks forever, so this arm is inert when
                // retention is disabled.
                _ = async {
                    match retention_tick.as_mut() {
                        Some(t) => { t.tick().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    run_retention_cycle(&db, &retention).await;
                }
            }
        }
    });

    cancel
}

/// One reaper pass, as its own root span.
///
/// The span is the point: this loop runs on `oxy-worker`, which had **zero**
/// trace-correlated log lines (`internal-docs/2026-09-09-hyperdx-observability-findings.md`
/// §3) because every line it emits comes from a background loop that is
/// outside any span. A `warn!` with no trace id is a line you can only grep;
/// inside a span it is a trace you can open, with the cycle's duration and
/// outcome already on it. The outcome fields are recorded even on the quiet
/// path — the log stays gated on `total() > 0`, but "the reaper ran and found
/// nothing" is exactly the fact you want when asking why a task sat unclaimed.
#[tracing::instrument(
    target = "background",
    name = "reaper_cycle",
    skip_all,
    fields(requeued = tracing::field::Empty, dead_lettered = tracing::field::Empty)
)]
async fn run_reaper_cycle(db: &DatabaseConnection) {
    match crud::reap_stale_tasks(db).await {
        Ok(outcome) => {
            // `TASKS_REQUEUED` / `TASKS_DEAD_LETTERED` are incremented inside
            // `reap_stale_tasks` itself (see `orchestrator::crud::queue`), not
            // here — this is only one of four call sites that reach it.
            // Logging stays gated on `total() > 0` so the 30s loop doesn't
            // flood with "reaper cycle: 0 0" on the happy path.
            let span = tracing::Span::current();
            span.record("requeued", outcome.requeued);
            span.record("dead_lettered", outcome.dead_lettered);
            if outcome.total() > 0 {
                tracing::info!(
                    target: "background",
                    requeued = outcome.requeued,
                    dead_lettered = outcome.dead_lettered,
                    "reaper cycle: reclaimed stale tasks"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "background",
                error = %e,
                "reaper cycle failed; will retry on next interval"
            );
        }
    }
}

/// One retention pass, as its own root span. Same reasoning as
/// [`run_reaper_cycle`]: an hourly prune that fails silently is invisible
/// without a span to hang the failure on.
#[tracing::instrument(
    target = "background",
    name = "retention_cycle",
    skip_all,
    fields(pruned = tracing::field::Empty)
)]
async fn run_retention_cycle(db: &DatabaseConnection, cfg: &RetentionConfig) {
    match crud::purge_old_terminal_tasks(db, cfg.completed_ttl, cfg.dead_ttl).await {
        Ok(0) => {
            // Quiet on the happy path.
            tracing::Span::current().record("pruned", 0u64);
        }
        Ok(n) => {
            tracing::Span::current().record("pruned", n);
            tracing::info!(
                target: "background",
                count = n,
                "retention cycle: pruned old terminal task-queue rows"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "background",
                error = %e,
                "retention cycle failed; will retry on next interval"
            );
        }
    }
}

#[cfg(test)]
mod retention_tests {
    use super::{RetentionConfig, ttl_days_from_env};
    use std::time::Duration;

    #[test]
    fn default_prunes_both_classes() {
        let cfg = RetentionConfig::default();
        assert!(cfg.enabled());
        assert_eq!(cfg.completed_ttl, Some(Duration::from_secs(7 * 24 * 3600)));
        assert_eq!(cfg.dead_ttl, Some(Duration::from_secs(30 * 24 * 3600)));
    }

    #[test]
    fn ttl_days_parses_off_switches_to_none() {
        let var = "OXY_TEST_TTL_OFF";
        let default = Some(Duration::from_secs(99));
        for off in ["0", "off", "never", " OFF "] {
            unsafe { std::env::set_var(var, off) };
            assert_eq!(
                ttl_days_from_env(var, default),
                None,
                "{off} should disable"
            );
        }
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn ttl_days_parses_day_count() {
        let var = "OXY_TEST_TTL_DAYS";
        unsafe { std::env::set_var(var, "3") };
        assert_eq!(
            ttl_days_from_env(var, None),
            Some(Duration::from_secs(3 * 24 * 3600))
        );
        unsafe { std::env::remove_var(var) };
    }

    #[test]
    fn ttl_days_unset_uses_default() {
        assert_eq!(
            ttl_days_from_env("OXY_TEST_TTL_UNSET_XYZ", Some(Duration::from_secs(42))),
            Some(Duration::from_secs(42))
        );
    }

    #[test]
    fn disabled_when_both_none() {
        let cfg = RetentionConfig {
            interval: Duration::from_secs(3600),
            completed_ttl: None,
            dead_ttl: None,
        };
        assert!(!cfg.enabled());
    }
}
