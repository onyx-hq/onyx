//! Task router: wakes idle workers when new tasks are enqueued.
//!
//! Two concrete implementations live alongside this trait:
//! - [`PostgresTaskRouter`]: production. Holds a dedicated `LISTEN
//!   oxy_task_enqueued` connection and translates Postgres notifications
//!   into a local `Notify`. Cross-process wake is "for free" because the
//!   notification fans out to every instance with an active listener.
//! - [`NoopTaskRouter`]: tests + CLI paths that don't run a long-lived
//!   process. `wait_for_task` falls through to the caller's backstop
//!   timeout; `notify_enqueued` is a no-op. Workers still work, just
//!   without the latency optimization.
//!
//! ## Why the trait
//!
//! The router is the one place in the runtime that's Postgres-specific
//! (LISTEN/NOTIFY is a vendor extension). Keeping it behind a trait means
//! the rest of the codebase — the worker claim loop, the enqueue helper,
//! the coordinator — talks to a single abstraction. If we ever swap to a
//! different durable store, only the impl needs to move; everything else
//! is unaware of the wake mechanism.
//!
//! The trait is also the natural extension point for worker classes (see
//! `OXY_WORKER_MAX_INFLIGHT` doc in `worker.rs`): `wait_for_task` already
//! takes a class filter, even though today's single-channel impl ignores
//! it. Per-class channels drop in by changing only the impl.
//!
//! ## What the router is NOT
//!
//! - Not a scheduler. The DB queue + `SKIP LOCKED` is the scheduler.
//! - Not a coordinator. Coordinators still own their task trees in
//!   memory; the router just helps their workers wake faster.
//! - Not a reaper. Stale-claim recovery is a sibling background job
//!   (see `background.rs`); see `crud::reap_stale_tasks`. Different
//!   responsibility, different cadence.

mod postgres;
pub use postgres::{
    DEFAULT_LISTENER_KEEPALIVE_INTERVAL, HEALTH_PROBE_CHANNEL, LAST_PROBE_RECEIVED_MILLIS,
    ListenerConfigFactory, PROBES_RECEIVED, PostgresTaskRouter, PostgresTaskRouterOptions,
    TASK_ENQUEUED_CHANNEL, TlsVerification, connect_listener,
};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

/// Wakes workers when work is available.
///
/// The contract is intentionally loose: `wait_for_task` may return for
/// any reason (real notification, spurious wake, timeout). Callers must
/// still call `claim_task` to find out whether work is actually
/// available — the router only promises that *if* something was
/// enqueued, *some* waiter will see a wake before the timeout.
#[async_trait]
pub trait TaskRouter: Send + Sync {
    /// Block until a task in one of the given classes is likely
    /// available, or `timeout` elapses.
    ///
    /// `classes` is currently advisory — the v1 Postgres impl uses a
    /// single channel and wakes every waiter regardless of class. The
    /// parameter is on the trait so per-class channels can be added
    /// without changing every caller. An empty slice means "any class."
    ///
    /// May return early on spurious wakes. After it returns, the caller
    /// should attempt one `claim_task` and, if that yields nothing,
    /// loop back to `wait_for_task`.
    async fn wait_for_task(&self, classes: &[String], timeout: Duration);

    /// Signal that a task was enqueued. Workers listening on `class`
    /// (or any class, if the impl doesn't partition) wake up.
    ///
    /// **Production code MUST NOT call this.** The
    /// `agentic_task_queue_notify_trigger` (see
    /// [`crate::migration`]'s migration 11) fires
    /// `pg_notify('oxy_task_enqueued', '')` automatically on every
    /// `queue_status = 'queued'` row write, atomic with the issuing
    /// transaction's commit. Production wakes come from the trigger
    /// — calling this method on top would be redundant at best and
    /// at worst hides the bug of a new enqueue path that doesn't
    /// actually write the row (listeners wake, find no row, sit idle
    /// until the 10s backstop). The trigger is the design; this
    /// trait method is the test-side injection point.
    ///
    /// Kept on the trait so router unit tests can drive listeners in
    /// isolation without writing a real queue row.
    #[doc(hidden)]
    async fn notify_enqueued(&self, class: Option<&str>);

    /// Fire a health probe so every listener (on this instance and
    /// any peer instance) can confirm the LISTEN/NOTIFY pipeline is
    /// end-to-end functional.
    ///
    /// Distinct from the keepalive ping in the Postgres impl: that
    /// proves the *connection* is alive for queries; this proves
    /// *notification delivery* is alive. A connection can stay up
    /// while LISTEN silently stops draining; the probe is what
    /// catches that.
    ///
    /// Background task fires this on a slow tick (default 60s). The
    /// default impl is a no-op so [`NoopTaskRouter`] callers don't
    /// pay for an unimplementable feature.
    async fn emit_health_probe(&self) {
        // No-op for routers without a NOTIFY pipeline.
    }
}

/// No-op router for callers that don't need cross-process wake.
///
/// `wait_for_task` returns when the timeout elapses, exactly as if
/// nothing was ever notified. `notify_enqueued` discards the signal.
/// Workers paired with this router fall back entirely to their own
/// backstop poll — correct but slow. Suitable for:
///
/// - Unit tests that drive the worker directly and don't care about
///   wake latency.
/// - One-shot CLI paths (`oxy run`, eval harnesses) where no
///   long-lived listener connection exists.
pub struct NoopTaskRouter;

#[async_trait]
impl TaskRouter for NoopTaskRouter {
    async fn wait_for_task(&self, _classes: &[String], timeout: Duration) {
        tokio::time::sleep(timeout).await;
    }

    async fn notify_enqueued(&self, _class: Option<&str>) {
        // Intentionally empty. The DB row is the source of truth; a
        // worker with a backstop poll will find it on the next tick.
    }
}

/// Convenience constructor for callers that just want a stub.
pub fn noop_router() -> Arc<dyn TaskRouter> {
    Arc::new(NoopTaskRouter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_wait_respects_timeout() {
        let router = NoopTaskRouter;
        let start = tokio::time::Instant::now();
        router.wait_for_task(&[], Duration::from_millis(50)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(45),
            "expected ~50ms wait, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn noop_notify_is_no_op() {
        let router = NoopTaskRouter;
        // Just confirm it returns; semantically a no-op.
        router.notify_enqueued(Some("io_bound")).await;
        router.notify_enqueued(None).await;
    }
}
