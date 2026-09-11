//! Durable transport backed by the `agentic_task_queue` database table.
//!
//! Assignments are INSERT-ed by the coordinator and claimed by workers via
//! `FOR UPDATE SKIP LOCKED`. Worker→coordinator messages still flow through
//! an in-memory channel (they are already persisted by the coordinator on
//! receipt). Only the assignment direction needs durability — the single gap
//! that existed with [`super::LocalTransport`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use agentic_core::delegation::{TaskAssignment, TaskOutcome, TaskSpec};
use agentic_core::transport::{
    CoordinatorTransport, TransportError, WorkerMessage, WorkerTransport,
};
use async_trait::async_trait;
use dashmap::DashMap;
use sea_orm::DatabaseConnection;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::crud;
use crate::orchestrator::router::{NoopTaskRouter, TaskRouter};

/// Default interval for polling the queue when no notification arrives.
///
/// 10s is the backstop, *not* the dispatch latency target. With a real
/// [`crate::orchestrator::router::PostgresTaskRouter`] wired in, NOTIFY-driven wakes
/// land within milliseconds; this fallback only fires when a wake was
/// missed (listener disconnected, in-process Notify permit consumed
/// before a waiter parked, etc). Tightening it back to 1s would burn
/// ~1 DB query/sec per worker process at idle for no real-world gain
/// — most of the time we should be parked on `Notify::notified()`.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Durable transport that persists task assignments in the database.
///
/// The coordinator inserts assignments; workers poll the table. A `Notify`
/// provides instant wake-up when a new task is enqueued, with a fallback
/// poll interval.
pub struct DurableTransport {
    db: DatabaseConnection,
    /// Unique identifier for this worker process.
    worker_id: String,

    /// Worker → Coordinator: events and outcomes (in-memory, ephemeral).
    message_tx: mpsc::Sender<WorkerMessage>,
    message_rx: Mutex<mpsc::Receiver<WorkerMessage>>,

    /// Wake signal: coordinator notifies when a new task is enqueued.
    new_task_notify: Notify,

    /// Per-task cancellation tokens, keyed by task_id.
    cancel_tokens: DashMap<String, CancellationToken>,

    /// The live heartbeat ticker for each claimed task, keyed by task_id.
    ///
    /// **At most one ticker per `task_id`, and this map is what enforces it.**
    /// A suspended task keeps its ticker running (see `Worker::handle_task`),
    /// so the resume path can hand the same `task_id` to a *second* driver
    /// while the first driver's ticker is still alive. Nothing else would stop
    /// the first one: [`Self::worker_id`] is process-stable, and
    /// `update_queue_heartbeat` authorizes on `worker_id` alone with no
    /// fencing token — so once the same process re-claims the row (typically
    /// within milliseconds, far inside one tick), the stale ticker's predicate
    /// matches again and it beats on forever.
    ///
    /// Left unbounded that accumulates one ticker per suspension for the life
    /// of the run, all writing `last_heartbeat` to the same row every tick —
    /// and worse, a ticker outliving the driver it belonged to keeps a claim
    /// looking alive that nobody is working, which is precisely the liveness
    /// signal the reaper depends on.
    ///
    /// **Scope: per transport instance, while the authority it writes with
    /// (`worker_id`) is per process.** A second transport in this process would
    /// not see — and so could not retire — this one's tickers. That gap is not
    /// reachable today: the delegating-step resume re-claims through the *same*
    /// transport (`claim_task_under_root` matches the root itself), and recovery
    /// only selects runs with no active queue entry, which already implies the
    /// old ticker broke on `Ok(false)`. If it ever does bite, the class-killing
    /// fix is a fencing token rather than a wider map — `claim_task` already
    /// returns `claim_count`, so `update_queue_heartbeat` could carry
    /// `AND claim_count = $3` and stale tickers would fence themselves out.
    ///
    /// Entries are removed at every point this process hands the claim back, so
    /// the map tracks live claims rather than history — with one exception: a
    /// driver that dies without any terminal write through [`Self::send`] (the
    /// `agentic-pipeline` virtual workers can only cancel their own token, since
    /// `retire_heartbeat` is private here) leaves a cancelled token behind until
    /// that `task_id` is claimed again. Bounded by run lifetime for the per-run
    /// transports, and inert either way — a cancelled token holds nothing but
    /// its own key.
    heartbeats: DashMap<String, CancellationToken>,

    /// Poll interval when no notification arrives.
    poll_interval: Duration,

    /// Optional task-tree scope. When set, [`recv_assignment`] only claims
    /// tasks whose `task_id` is exactly this value or starts with
    /// `"<value>."` (children/grandchildren by the
    /// `<parent>.<seq>` naming convention).
    ///
    /// The per-request `spawn_automation_run_drive` path sets this to the
    /// run id so its worker doesn't poach tasks from a sibling run's
    /// coordinator — when that happens, the wrong coordinator receives the
    /// outcome (the task isn't in its in-memory `self.tasks` map),
    /// `handle_done` early-returns, and the right coordinator hangs
    /// waiting for an outcome that already arrived elsewhere.
    task_id_root: Option<String>,

    /// Cross-process wake source.
    ///
    /// In a single process, [`Self::new_task_notify`] handles wakes
    /// instantly — `assign()` calls `notify_one()` in the same address
    /// space. But assignments enqueued by a *different* app instance
    /// never touch our in-memory Notify; the only signal is the row
    /// landing in `agentic_task_queue`. The router (via Postgres
    /// LISTEN/NOTIFY in production, [`NoopTaskRouter`] in tests) is
    /// what makes those cross-process wakes fast — without it, workers
    /// would wait the full [`DEFAULT_POLL_INTERVAL`] before noticing.
    router: Arc<dyn TaskRouter>,

    /// Retires this transport's worker loop.
    ///
    /// [`WorkerTransport::recv_assignment`] is an infinite loop with no other
    /// exit: it returns `Some` on a claim and `continue`s on everything else.
    /// `Worker::run` breaks only on `None`, so without this a worker spawned
    /// for one run polls the queue **forever after that run is terminal** —
    /// and the per-run drive path spawns one worker per run. Each survivor
    /// costs a `claim_task_under_root` round-trip per poll interval and, more
    /// expensively, a standing place in sqlx's FIFO pool-waiter queue.
    ///
    /// That is the shape behind the 2026-09-01 outage: connections climbed with
    /// the number of runs a process had ever driven — 113 to the server's
    /// ceiling over a day — until Postgres refused new clients and every API
    /// request waited the full `ACQUIRE_TIMEOUT` behind retired pollers.
    ///
    /// Cancelled by whoever owns the run once its coordinator is finished.
    worker_loop_cancel: CancellationToken,

    /// Sub-second offset added to this transport's pool-starvation backoff.
    ///
    /// Per **instance**, not per worker id: [`Self::worker_id`] is
    /// process-stable (`process_worker_id`), so deriving the offset from it
    /// would hand every co-located claimer the identical delay and desynchronise
    /// nothing — which is precisely the pile-up the backoff exists to break up.
    backoff_offset_ms: u64,
}

/// Hands each `DurableTransport` in this process a distinct backoff offset.
static BACKOFF_OFFSET_SEQ: AtomicU64 = AtomicU64::new(0);

/// The process-wide worker identity, generated once on first use.
///
/// Every `DurableTransport` in this process shares it, so
/// `WHERE worker_id = process_worker_id()` selects exactly the claims this
/// process holds — the predicate graceful release is built on.
static PROCESS_WORKER_ID: OnceLock<String> = OnceLock::new();

/// The process-wide worker identity. See [`build_worker_id`] for the format.
pub fn process_worker_id() -> &'static str {
    PROCESS_WORKER_ID.get_or_init(compute_worker_identity)
}

/// The process worker identity, but **only if it has already been minted**.
///
/// [`process_worker_id`] is lazily initialized, so calling it from a shutdown
/// path is not free of consequence: a process that never constructed a
/// `DurableTransport` has never written a `worker_id` into
/// `agentic_task_queue`, and asking for the id at shutdown would mint a brand
/// new one that matches zero rows by construction — a pointless UPDATE against
/// a database that may be exactly the thing making shutdown slow.
///
/// Shutdown paths use this instead. `None` is a *fact* ("this process holds no
/// claims, because it never claimed anything"), not a guess, so skipping the
/// release on `None` cannot lose a claim.
pub fn process_worker_id_if_initialized() -> Option<&'static str> {
    PROCESS_WORKER_ID.get().map(String::as_str)
}

/// Set once this process starts shutting down; never cleared.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Edge-triggered companion to [`SHUTTING_DOWN`], cancelled by
/// [`begin_shutdown`] and never reset.
///
/// The bool is the *level-triggered gate* a loop checks at its top; this token
/// is the *edge-triggered wake* that lets an operation already in flight
/// abandon promptly. During a co-located-DB shutdown (Ctrl-C in local/dev),
/// the database dies at the same instant as the app, so a DB poll already
/// awaiting would otherwise block the full pool `ACQUIRE_TIMEOUT` (~30s) before
/// failing. Racing it against `shutdown_signal().cancelled()` collapses that
/// wait to nothing — the loop parks on its bool gate instead.
static SHUTDOWN_SIGNAL: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);

/// The process-wide shutdown signal, cancelled exactly when [`begin_shutdown`]
/// is called.
///
/// `select!` an in-flight DB round-trip against `shutdown_signal().cancelled()`
/// so a shutdown that races the poll abandons it instead of waiting out the
/// pool acquire timeout. It is *only* for background polls (claim / heartbeat /
/// latency); the graceful *release* itself must still run to completion, so it
/// is bounded by its own hook timeout, not by this signal.
pub fn shutdown_signal() -> &'static CancellationToken {
    &SHUTDOWN_SIGNAL
}

/// Declare that this process is shutting down: no worker in it will claim
/// another task from the durable queue.
///
/// **Must be called before [`crate::crud::release_claims_for_worker`].** The
/// release's `claimed -> queued` transition fires the `agentic_task_queue`
/// NOTIFY trigger, which actively *wakes* every in-process worker through the
/// shared `PostgresTaskRouter`. Without this gate they immediately re-claim
/// the rows just released (recharging `claim_count`), and then the process
/// exits leaving those rows `claimed` by a dead worker with the budget spent —
/// precisely the failure graceful release exists to prevent, relocated into a
/// smaller window.
///
/// Sets the level-triggered [`SHUTTING_DOWN`] gate *and* cancels the
/// edge-triggered [`shutdown_signal`] so an in-flight DB poll can abandon at
/// once. The store happens before the cancel, so any task woken by the signal
/// observes the flag already `true`.
///
/// Monotonic and idempotent: shutdown is a one-way door, and both `oxy serve`
/// and `oxy worker` may reach it more than once.
pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    SHUTDOWN_SIGNAL.cancel();
}

/// Whether [`begin_shutdown`] has been called in this process.
pub fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::SeqCst)
}

/// Build a human-readable worker identity, used as the `worker_id` column of
/// `agentic_task_queue` and surfaced in the admin "Internal jobs" console.
///
/// Format: `{env}·{host}·{short}` — e.g. `prod·ip-10-2-3-4·5f3a9c1b`.
///
/// - `env`   — deployment environment from `OXY_ENV` / `ENVIRONMENT`, else
///   `local`. Lets an operator tell a prod worker from a staging one at a
///   glance.
/// - `host`  — pod / container host from `POD_NAME` / `HOSTNAME`. In
///   Kubernetes `HOSTNAME` defaults to the pod name, so this correlates a
///   worker straight to real infrastructure (the whole point of the rename
///   away from the opaque `worker-<uuid>`). Falls back to `unknown`.
/// - `short` — first 8 hex chars of a fresh UUID, so multiple worker
///   processes on one host stay distinct.
///
/// Process-stable: every transport in this process returns the same id.
///
/// Legacy `worker-<uuid>` ids already in the DB are unaffected: `worker_id` is
/// an opaque display string everywhere it is read.
fn build_worker_id() -> String {
    process_worker_id().to_string()
}

/// Generate a fresh identity. Called exactly once per process, via
/// [`PROCESS_WORKER_ID`].
fn compute_worker_identity() -> String {
    let env =
        first_nonempty_env(&["OXY_ENV", "ENVIRONMENT"]).unwrap_or_else(|| "local".to_string());
    let host =
        first_nonempty_env(&["POD_NAME", "HOSTNAME"]).unwrap_or_else(|| "unknown".to_string());
    let uuid = uuid::Uuid::new_v4().simple().to_string();
    let short = &uuid[..8];
    format!(
        "{}·{}·{}",
        sanitize_segment(&env),
        sanitize_segment(&host),
        short
    )
}

/// Return the first env var in `keys` that is set and non-empty (trimmed).
fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// Lowercase and keep only `[a-z0-9._-]`, mapping anything else to `-`,
/// trimming stray dashes, and capping length so a pathological hostname can't
/// bloat the id. Empty input collapses to `unknown` so a segment is never
/// blank.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let capped: String = cleaned.trim_matches('-').chars().take(40).collect();
    if capped.is_empty() {
        "unknown".to_string()
    } else {
        capped
    }
}

impl DurableTransport {
    /// Create a new durable transport.
    ///
    /// Uses [`NoopTaskRouter`] — workers wake only on in-process
    /// `assign()` or the 10s backstop poll. For production deployments
    /// with multi-instance scaling, use [`Self::with_router`] and pass
    /// a [`crate::orchestrator::router::PostgresTaskRouter`] so cross-instance
    /// enqueues wake workers within milliseconds.
    pub fn new(db: DatabaseConnection) -> Arc<Self> {
        Self::with_config(db, DEFAULT_POLL_INTERVAL)
    }

    /// Create with custom poll interval (useful for testing).
    pub fn with_config(db: DatabaseConnection, poll_interval: Duration) -> Arc<Self> {
        Self::with_config_and_root(db, poll_interval, None, Arc::new(NoopTaskRouter))
    }

    /// Create a transport whose worker only sees tasks under the given root.
    ///
    /// `root_task_id` typically equals the run id of the automation this
    /// transport's coordinator owns. Children spawned by that coordinator
    /// follow the `<root>.<n>` naming convention and are also claimed by
    /// this transport's worker; tasks belonging to other runs are skipped.
    pub fn new_scoped(db: DatabaseConnection, root_task_id: String) -> Arc<Self> {
        Self::with_config_and_root(
            db,
            DEFAULT_POLL_INTERVAL,
            Some(root_task_id),
            Arc::new(NoopTaskRouter),
        )
    }

    /// Create a transport that uses the given router for cross-process
    /// wake notifications.
    ///
    /// Production callers: pass a process-shared
    /// [`crate::orchestrator::router::PostgresTaskRouter`] so every automation run on
    /// this instance benefits from the same LISTEN connection rather
    /// than opening one per run. The router is cheap to clone (it's
    /// just an `Arc` to the shared listener state).
    pub fn with_router(
        db: DatabaseConnection,
        router: Arc<dyn TaskRouter>,
        root_task_id: Option<String>,
    ) -> Arc<Self> {
        Self::with_config_and_root(db, DEFAULT_POLL_INTERVAL, root_task_id, router)
    }

    fn with_config_and_root(
        db: DatabaseConnection,
        poll_interval: Duration,
        task_id_root: Option<String>,
        router: Arc<dyn TaskRouter>,
    ) -> Arc<Self> {
        let (message_tx, message_rx) = mpsc::channel(1024);
        let worker_id = build_worker_id();
        Arc::new(Self {
            db,
            worker_id,
            message_tx,
            message_rx: Mutex::new(message_rx),
            new_task_notify: Notify::new(),
            cancel_tokens: DashMap::new(),
            heartbeats: DashMap::new(),
            poll_interval,
            task_id_root,
            router,
            worker_loop_cancel: CancellationToken::new(),
            // Spread across the sub-second window; wraps harmlessly.
            backoff_offset_ms: (BACKOFF_OFFSET_SEQ.fetch_add(97, Ordering::Relaxed)) % 1_000,
        })
    }

    /// Retire this transport's worker loop: the next [`recv_assignment`] returns
    /// `None`, so `Worker::run` breaks and the task ends.
    ///
    /// Call once the coordinator that owns this transport's run tree has
    /// finished — at that point nothing can legitimately be claimed under this
    /// root again, and a poller that keeps asking is pure cost. See
    /// [`Self::worker_loop_cancel`] for what that cost was.
    ///
    /// Idempotent, and safe to call from any task.
    ///
    /// [`recv_assignment`]: WorkerTransport::recv_assignment
    pub fn retire_worker_loop(&self) {
        self.worker_loop_cancel.cancel();
    }

    /// Wake any polling workers so they check the queue immediately.
    ///
    /// Used by recovery after re-queuing tasks — the normal `assign()` path
    /// calls this internally, but `requeue_task()` bypasses the transport.
    pub fn notify_new_task(&self) {
        self.new_task_notify.notify_waiters();
    }

    /// Stop the heartbeat ticker registered for `task_id`, if any.
    ///
    /// Called from exactly the points where this process stops owning the
    /// claim — a terminal outcome, a deferral that hands the row back, and the
    /// re-spawn path in [`WorkerTransport::spawn_heartbeat`]. A *suspension*
    /// is deliberately not one of them: the row stays `claimed` and the ticker
    /// is what says so.
    ///
    /// Removing the entry here is also what bounds [`Self::heartbeats`] — it
    /// holds live claims, not history.
    fn retire_heartbeat(&self, task_id: &str, why: &'static str) {
        if let Some((_, token)) = self.heartbeats.remove(task_id) {
            tracing::debug!(task_id, why, "retiring heartbeat ticker");
            token.cancel();
        }
    }

    // `heartbeat` / `spawn_heartbeat` inherent methods removed: they duplicated
    // the `WorkerTransport` impl below with a *divergent* contract (the
    // inherent one returned `Result<bool>` where the trait returns
    // `Result<()>`), so `t.heartbeat(&id)` meant different things depending
    // only on whether the receiver was concrete or `dyn`. Every production
    // call site goes through the trait — `Worker::handle_task`,
    // `pipeline::recovery::spawn_virtual_worker`, and `pipeline`'s root-task
    // driver all hold `Arc<dyn WorkerTransport>` — so the inherent pair was
    // reachable only from itself.

    /// Run a single reaper cycle: re-queue stale tasks, dead-letter exhausted ones.
    ///
    /// Returns the split requeued/dead-lettered counts.
    pub async fn run_reaper(&self) -> crud::ReapOutcome {
        match crud::reap_stale_tasks(&self.db).await {
            Ok(outcome) => {
                if outcome.total() > 0 {
                    tracing::info!(
                        target: "agentic",
                        requeued = outcome.requeued,
                        dead_lettered = outcome.dead_lettered,
                        "reaper: reclaimed stale tasks"
                    );
                    // Wake workers so they can pick up re-queued tasks.
                    self.new_task_notify.notify_waiters();
                }
                outcome
            }
            Err(e) => {
                tracing::error!("reaper failed: {e}");
                crud::ReapOutcome::default()
            }
        }
    }

    /// Prune old terminal rows from `agentic_task_queue`. Mirrors
    /// [`Self::run_reaper`] for the admin manual-trigger path; the periodic
    /// version lives in `orchestrator::background`. Returns rows deleted.
    pub async fn run_retention(
        &self,
        completed_ttl: Option<std::time::Duration>,
        dead_ttl: Option<std::time::Duration>,
    ) -> u64 {
        match crud::purge_old_terminal_tasks(&self.db, completed_ttl, dead_ttl).await {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(count, "retention: pruned old terminal task-queue rows");
                }
                count
            }
            Err(e) => {
                tracing::error!("retention failed: {e}");
                0
            }
        }
    }

    // `spawn_reaper` removed: live reaping is now the responsibility of
    // [`crate::orchestrator::background::start`], which runs a single process-level
    // task per app instance. The old per-transport spawn was dead code
    // (no production caller) — see commit history for the audit trail.

    /// Run a single stuck-automation-run sweep.
    ///
    /// Finds automation runs in non-terminal `task_status` with no queue entry
    /// for themselves or any descendant, and re-enqueues a fresh
    /// `AutomationDecision` for each. The decider is idempotent under the
    /// `decision_version` CAS, so a race where two sweepers (or a sweeper and
    /// a real worker) both re-enqueue is safe — one will win the CAS, the
    /// other will return `VersionConflict` and exit cleanly.
    ///
    /// `grace_secs` is the minimum `updated_at` age before a run is eligible,
    /// to avoid racing with an in-flight commit. Returns the number of runs
    /// rescued in this pass.
    #[tracing::instrument(
        target = "transport",
        name = "stuck_run_sweeper_cycle",
        skip(self),
        fields(rescued = tracing::field::Empty, stuck = tracing::field::Empty)
    )]
    pub async fn run_stuck_run_sweeper(&self, grace_secs: u64) -> u64 {
        let stuck = match crud::find_stuck_automation_runs(&self.db, grace_secs).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("stuck-run sweeper: query failed: {e}");
                return 0;
            }
        };

        tracing::Span::current().record("stuck", stuck.len() as u64);
        let mut rescued: u64 = 0;
        for run in &stuck {
            let spec = TaskSpec::AutomationDecision {
                run_id: run.run_id.clone(),
                pending_child_answer: None,
            };
            // Re-enqueue as an AutomationDecision. `enqueue_task` upserts on
            // conflict — if another writer has already re-driven the run
            // between our query and this call, we harmlessly overwrite with
            // the same spec shape. The stuck-run sweeper rescues orphaned
            // runs that have no in-process driver, so this is `Global`:
            // the recovery/global loop is responsible for driving it.
            if let Err(e) = crud::enqueue_task(
                &self.db,
                &run.run_id,
                &run.run_id,
                None,
                &spec,
                None,
                crud::TaskScope::Global,
            )
            .await
            {
                tracing::error!(
                    run_id = %run.run_id,
                    error = %e,
                    "stuck-run sweeper: failed to re-enqueue AutomationDecision"
                );
                continue;
            }
            tracing::warn!(
                run_id = %run.run_id,
                task_status = ?run.task_status,
                "stuck-run sweeper: re-enqueued AutomationDecision"
            );
            rescued += 1;
        }

        tracing::Span::current().record("rescued", rescued);
        if rescued > 0 {
            self.new_task_notify.notify_waiters();
        }
        rescued
    }

    /// Spawn a background sweeper that periodically calls
    /// [`run_stuck_run_sweeper`](Self::run_stuck_run_sweeper).
    ///
    /// Use `grace_secs >= interval` so the sweeper never acts on a run it
    /// just observed in the previous pass.
    pub fn spawn_stuck_run_sweeper(
        self: &Arc<Self>,
        interval: Duration,
        grace_secs: u64,
    ) -> CancellationToken {
        let cancel = CancellationToken::new();
        let transport = Arc::clone(self);
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // first tick is immediate, skip it
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        transport.run_stuck_run_sweeper(grace_secs).await;
                    }
                    _ = cancel_clone.cancelled() => break,
                }
            }
        });
        cancel
    }
}

#[async_trait]
impl CoordinatorTransport for DurableTransport {
    async fn assign(&self, assignment: TaskAssignment) -> Result<(), TransportError> {
        self.cancel_tokens
            .insert(assignment.task_id.clone(), CancellationToken::new());

        // Persist the assignment in the queue. A scoped transport
        // (`task_id_root` set — every per-run interactive coordinator)
        // owns this task's tree, so the global claim path must not poach
        // it; an unscoped transport (the global/recovery driver) enqueues
        // work that no co-located coordinator owns.
        let scope = if self.task_id_root.is_some() {
            crud::TaskScope::Scoped
        } else {
            crud::TaskScope::Global
        };
        crud::enqueue_task(
            &self.db,
            &assignment.task_id,
            &assignment.run_id,
            assignment.parent_task_id.as_deref(),
            &assignment.spec,
            assignment.policy.as_ref(),
            scope,
        )
        .await
        .map_err(|e| TransportError::Other(format!("enqueue failed: {e}")))?;

        // Wake any polling worker immediately.
        self.new_task_notify.notify_one();

        Ok(())
    }

    async fn recv(&self) -> Option<WorkerMessage> {
        self.message_rx.lock().await.recv().await
    }

    async fn cancel(&self, task_id: &str) -> Result<(), TransportError> {
        // Update queue status in DB.
        crud::cancel_queued_task(&self.db, task_id)
            .await
            .map_err(|e| TransportError::Other(format!("cancel failed: {e}")))?;

        // Also fire the in-memory cancellation token for already-running tasks.
        if let Some(token) = self.cancel_tokens.get(task_id) {
            token.cancel();
        }

        Ok(())
    }

    async fn cancel_subtree(&self, root_task_id: &str) -> Result<(), TransportError> {
        // Cancel the root's queue entry + token.
        self.cancel(root_task_id).await?;

        // Fire tokens for every descendant. Child ids are formatted as
        // `{parent_id}.{counter}` by `Coordinator::handle_suspended`, so
        // every descendant's task_id starts with `"{root_task_id}."`.
        let prefix = format!("{root_task_id}.");
        for entry in self.cancel_tokens.iter() {
            if entry.key().starts_with(&prefix) {
                entry.value().cancel();
            }
        }

        Ok(())
    }
}

/// Is this the connection pool refusing to hand out a connection, rather than
/// a query or connectivity error?
///
/// `sea_orm` surfaces a pool-acquire timeout as [`sea_orm::DbErr::ConnectionAcquire`].
/// The string fallback catches the same condition arriving wrapped (e.g. as a
/// `Conn(SqlxError(PoolTimedOut))`), which is what the prod logs actually
/// showed: `"Failed to acquire connection from pool: Connection pool timed out"`.
/// The classification only chooses a backoff, so a false negative costs the old
/// behaviour and nothing worse.
fn is_pool_acquire_failure(err: &sea_orm::DbErr) -> bool {
    if matches!(err, sea_orm::DbErr::ConnectionAcquire(_)) {
        return true;
    }
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("acquire connection from pool") || msg.contains("pool timed out")
}

/// Backoff for a claim poll that could not get a connection.
///
/// Exponential from 1s to a 30s ceiling, plus this transport's own sub-second
/// offset so co-located claimers do not re-synchronise onto the same instant
/// and rebuild the pile-up this exists to break up. The offset is assigned per
/// instance at construction rather than drawn from an RNG, so the schedule
/// stays reproducible under test and this needs no new dependency.
fn pool_starvation_backoff(streak: u32, offset_ms: u64) -> std::time::Duration {
    const BASE_MS: u64 = 1_000;
    const CEILING_MS: u64 = 30_000;
    // `streak` is 1-based; shift by at most 5 so the multiply can never wrap.
    let shift = streak.saturating_sub(1).min(5);
    let capped = BASE_MS.saturating_mul(1u64 << shift).min(CEILING_MS);
    std::time::Duration::from_millis(capped + offset_ms)
}

#[async_trait]
impl WorkerTransport for DurableTransport {
    async fn recv_assignment(&self) -> Option<TaskAssignment> {
        // Consecutive pool-acquire failures, driving the backoff in the `Err`
        // arm below. Reset the moment a claim round-trip succeeds.
        let mut pool_starvation_streak: u32 = 0;
        loop {
            // The ONLY exit. Checked first so a retired transport stops asking
            // the database anything at all, including on the paths below that
            // `continue` without ever reaching a claim.
            if self.worker_loop_cancel.is_cancelled() {
                tracing::debug!(
                    worker_id = %self.worker_id,
                    task_id_root = ?self.task_id_root,
                    "worker loop retired; no longer polling the queue"
                );
                return None;
            }
            // Refuse to take on new work once this process has begun shutting
            // down. Graceful release flips our claims `claimed -> queued`,
            // which fires the queue's NOTIFY trigger and wakes *this* loop —
            // so without the gate we would re-claim the very rows we just
            // gave back, recharge `claim_count`, and then exit holding them.
            // See [`begin_shutdown`].
            //
            // Park on the poll interval rather than spinning: nothing can
            // clear the flag, and the process is on its way out anyway.
            if is_shutting_down() {
                tokio::time::sleep(self.poll_interval).await;
                continue;
            }

            // Try to claim a task from the queue. Scoped transports only
            // see tasks under their root — see `task_id_root` for why.
            //
            // Race the claim round-trip against the process shutdown signal: a
            // poll already in flight when a co-located DB dies on shutdown
            // (Ctrl-C in local/dev) would otherwise block the full pool
            // `ACQUIRE_TIMEOUT` (~30s) before erroring, pushing shutdown past
            // the grace period. On shutdown we abandon the poll and loop; the
            // `is_shutting_down()` gate at the top then parks us for good.
            // `begin_shutdown` sets that gate before cancelling this signal, so
            // the next iteration is guaranteed to see it.
            let claim_result = tokio::select! {
                _ = shutdown_signal().cancelled() => continue,
                r = async {
                    match &self.task_id_root {
                        Some(root) => {
                            crud::claim_task_under_root(&self.db, &self.worker_id, root).await
                        }
                        None => crud::claim_task(&self.db, &self.worker_id).await,
                    }
                } => r,
            };
            if claim_result.is_ok() {
                pool_starvation_streak = 0;
            }
            match claim_result {
                Ok(Some(entry)) => {
                    // Close the gate-to-claim race *at its cause*. The gate
                    // above was clear when this iteration started, but the
                    // claim is a round-trip: shutdown can begin while it is in
                    // flight, and the release pass can therefore run before
                    // this row lands `claimed`. The row would then be owned by
                    // a process that is already gone, with `claim_count`
                    // charged — exactly the state graceful release exists to
                    // prevent, relocated into a smaller window.
                    //
                    // This is the only point in the system that knows both
                    // that the claim exists and that the process is leaving,
                    // so it is the only place the race can actually be closed.
                    // `drain_claims_for_worker` can only *narrow* it, and if
                    // this process held no other claims the drain's first pass
                    // returns 0 and it stops before the straggler lands —
                    // which is the likeliest shape of the race, since an idle
                    // poller is precisely the worker sitting in `claim_task`.
                    if is_shutting_down() {
                        match crud::release_claim(&self.db, &entry.task_id, &self.worker_id).await {
                            Ok(_) => tracing::info!(
                                task_id = %entry.task_id,
                                "claim landed during shutdown; handed straight back to the queue"
                            ),
                            Err(e) => tracing::warn!(
                                task_id = %entry.task_id,
                                "failed to hand back a claim taken during shutdown: {e} \
                                 (the reaper will reclaim it after the visibility timeout)"
                            ),
                        }
                        continue;
                    }

                    // Deserialize spec and policy back into the assignment.
                    let spec: TaskSpec = match serde_json::from_value(entry.spec) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!(task_id = %entry.task_id, "failed to deserialize task spec: {e}");
                            // Mark as failed and try the next task. We hold the
                            // claim — the UPDATE above just granted it — so the
                            // ownership predicate matches.
                            let _ =
                                crud::fail_queue_task(&self.db, &entry.task_id, &self.worker_id)
                                    .await;
                            continue;
                        }
                    };
                    let policy = entry.policy.and_then(|p| {
                        serde_json::from_value(p)
                            .map_err(|e| {
                                tracing::warn!(task_id = %entry.task_id, "failed to deserialize task policy: {e}");
                            })
                            .ok()
                    });

                    return Some(TaskAssignment {
                        task_id: entry.task_id,
                        parent_task_id: entry.parent_task_id,
                        run_id: entry.run_id,
                        spec,
                        policy,
                    });
                }
                Ok(None) => {
                    // No tasks available. Wait for any wake signal:
                    //   - `new_task_notify`: same-process `assign()` call
                    //   - `router.wait_for_task`: cross-process LISTEN wake
                    //     (or its own internal backstop timeout)
                    //
                    // The router's `wait_for_task` already enforces the
                    // backstop via its `timeout` arg, so we don't need
                    // a separate `tokio::time::sleep` here — that would
                    // double up the timeout and waste a poll-interval's
                    // worth of latency on every loop iteration.
                    tokio::select! {
                        _ = self.new_task_notify.notified() => {}
                        _ = self.router.wait_for_task(&[], self.poll_interval) => {}
                        // Retirement lands here too, not just at the top of the
                        // loop: an idle poller is parked in exactly this select,
                        // so without this arm it would sit out a full poll
                        // interval before noticing it has been retired.
                        _ = self.worker_loop_cancel.cancelled() => {}
                    }
                }
                Err(e) => {
                    // A DB failure while shutting down is expected, not an
                    // error: the co-located database is going down with us.
                    // Keep the original ERROR level in steady state so a real
                    // queue-connectivity problem is still loud.
                    if is_shutting_down() {
                        tracing::debug!("failed to claim task from queue during shutdown: {e}");
                        tokio::time::sleep(self.poll_interval).await;
                    } else if is_pool_acquire_failure(&e) {
                        // The pool is starved, and this poll already spent the
                        // full `ACQUIRE_TIMEOUT` (~30s) parked in its waiter
                        // queue to find that out. Re-entering on the ordinary
                        // poll interval is the wrong move: sqlx's waiter queue
                        // is FIFO, so a claimer that immediately re-queues sits
                        // permanently *ahead of request traffic*, and N
                        // claimers in one process hold N such places. In prod
                        // (2026-09-01) seven of them on the ide node kept the
                        // API waiting the whole timeout behind background work
                        // that had nothing to do.
                        //
                        // Back off instead, so a starved pool drains toward the
                        // callers a human is waiting on.
                        pool_starvation_streak = pool_starvation_streak.saturating_add(1);
                        let backoff =
                            pool_starvation_backoff(pool_starvation_streak, self.backoff_offset_ms);
                        tracing::error!(
                            worker_id = %self.worker_id,
                            streak = pool_starvation_streak,
                            backoff_ms = backoff.as_millis() as u64,
                            "failed to claim task from queue: {e} (pool starved; backing off \
                             so request traffic is not queued behind this poller)"
                        );
                        tokio::time::sleep(backoff).await;
                    } else {
                        tracing::error!("failed to claim task from queue: {e}");
                        tokio::time::sleep(self.poll_interval).await;
                    }
                }
            }
        }
    }

    async fn send(&self, msg: WorkerMessage) -> Result<(), TransportError> {
        // On terminal outcomes, update the queue entry.
        match &msg {
            // A deferral returns the task to the queue unrun, invisible for
            // `delay_secs`. Scoped to `self.worker_id` for the same reason
            // every terminal write is: this can arrive after graceful release
            // handed our claims back and a peer re-claimed the row, and
            // deferring a peer's live task would strand it.
            //
            // Deliberately NOT forwarded to the coordinator: it produced no
            // outcome, and a coordinator that saw one would accumulate a
            // result for a task that never ran.
            WorkerMessage::Defer {
                task_id,
                delay_secs,
                max_wait_secs,
                reason,
            } => {
                use crud::DeferOutcome;
                // The row goes back to the queue unrun, so this process stops
                // owning the claim — stop proving otherwise.
                self.retire_heartbeat(task_id, "task deferred");
                match crud::defer_task(
                    &self.db,
                    task_id,
                    &self.worker_id,
                    *delay_secs as i64,
                    *max_wait_secs as i64,
                )
                .await
                {
                    Ok(DeferOutcome::Deferred) => {
                        tracing::info!(
                            target: "transport",
                            %task_id, delay_secs, %reason,
                            "task returned to the queue, deferred"
                        );
                    }
                    Ok(DeferOutcome::DeadLettered) => {
                        // Waited past the domain's ceiling. Loud on purpose:
                        // a pipeline blocked this long is a failure someone
                        // has to see, and the whole reason to bound the wait
                        // was that a silently growing queue looks healthy.
                        tracing::error!(
                            target: "transport",
                            %task_id, max_wait_secs, %reason,
                            "task dead-lettered: waited past its ceiling without \
                             ever being able to run"
                        );
                    }
                    Ok(DeferOutcome::NotHeld) => {
                        tracing::warn!(
                            target: "transport",
                            %task_id,
                            "defer skipped: this worker no longer holds the claim"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            target: "transport",
                            %task_id, error = %e,
                            "failed to defer task; it stays claimed until the reaper \
                             reclaims it at the visibility timeout"
                        );
                    }
                }
                return Ok(());
            }
            WorkerMessage::Outcome { task_id, outcome } => {
                // Every terminal write is scoped to `self.worker_id`. This
                // outcome can arrive after graceful release handed our claims
                // back and a peer re-claimed the row — stamping it terminal
                // then would strand the peer's live work. See
                // `crud::queue::set_terminal_status_owned`.
                let result = match outcome {
                    // Intercepted by the worker before it reaches a transport;
                    // if it gets here the worker's translation was bypassed.
                    TaskOutcome::Deferred { .. } => {
                        tracing::error!(
                            target: "transport",
                            %task_id,
                            "Deferred arrived as an Outcome; worker translation bypassed. \
                             Leaving the claim alone rather than stamping it terminal."
                        );
                        return Ok(());
                    }
                    TaskOutcome::Done { .. } => {
                        self.retire_heartbeat(task_id, "task done");
                        crud::complete_queue_task(&self.db, task_id, &self.worker_id).await
                    }
                    TaskOutcome::Failed(_) => {
                        self.retire_heartbeat(task_id, "task failed");
                        crud::fail_queue_task(&self.db, task_id, &self.worker_id).await
                    }
                    TaskOutcome::Cancelled => {
                        self.retire_heartbeat(task_id, "task cancelled");
                        crud::cancel_queued_task_owned(&self.db, task_id, &self.worker_id).await
                    }
                    // Suspended is not terminal — the row stays `claimed` so it
                    // can resume, and the heartbeat ticker stays running to say
                    // so. Retiring it here is exactly the bug this arm's
                    // silence used to cause; see `Worker::handle_task`.
                    TaskOutcome::Suspended { .. } => Ok(crud::TerminalWrite::Stamped),
                };
                match result {
                    Ok(crud::TerminalWrite::Stamped) => {}
                    // The requester already stamped this row terminal — an
                    // ordinary user-pressed Stop beats the worker's own
                    // `Cancelled` to the row on every single cancel, because
                    // `CoordinatorTransport::cancel` awaits the DB write before
                    // firing the in-memory token. Nothing is lost and nobody
                    // else is involved; first terminal status wins.
                    Ok(crud::TerminalWrite::AlreadyTerminal) => tracing::debug!(
                        task_id,
                        "queue row was already terminal; keeping the first status"
                    ),
                    // Root tasks registered via `Coordinator::register_root`
                    // are driven by a virtual worker and never published as an
                    // assignment, so they have no queue row to stamp. Routine.
                    Ok(crud::TerminalWrite::NoRow) => {
                        tracing::trace!(task_id, "no queue row for this task; nothing to stamp")
                    }
                    // The one case worth a warning: a *different* worker holds
                    // the row, so this process just finished work the queue is
                    // discarding.
                    Ok(crud::TerminalWrite::NotOwned) => tracing::warn!(
                        task_id,
                        worker_id = %self.worker_id,
                        "outcome dropped: another worker now holds this claim \
                         (released on shutdown and re-claimed by a peer, or reaped)"
                    ),
                    Err(e) => tracing::warn!(task_id, "failed to update queue status: {e}"),
                }
            }
            WorkerMessage::Event { .. } => {
                // Events don't affect queue status.
            }
        }

        // Forward to coordinator via in-memory channel.
        self.message_tx
            .send(msg)
            .await
            .map_err(|_| TransportError::ChannelClosed)
    }

    fn cancellation_token(&self, task_id: &str) -> CancellationToken {
        self.cancel_tokens
            .entry(task_id.to_string())
            .or_default()
            .clone()
    }

    async fn heartbeat(&self, task_id: &str) -> Result<(), TransportError> {
        // The trait signature can't carry "you no longer own this"; a lost
        // claim is a legitimate shutdown race, not a transport failure, so it
        // maps to `Ok`. The ticker below is what actually needs to react.
        crud::update_queue_heartbeat(&self.db, task_id, &self.worker_id)
            .await
            .map(|_| ())
            .map_err(|e| TransportError::Other(format!("heartbeat failed: {e}")))
    }

    fn spawn_heartbeat(&self, task_id: &str, interval: Duration) -> CancellationToken {
        let cancel = CancellationToken::new();
        // Retire any ticker still beating for this task_id before registering
        // the new one. See [`Self::heartbeats`]: a process-stable `worker_id`
        // plus an unfenced heartbeat predicate means the previous driver's
        // ticker would otherwise keep matching this row after the resume path
        // re-enqueued it and this process re-claimed it.
        self.retire_heartbeat(task_id, "superseded by a new claim");
        self.heartbeats.insert(task_id.to_string(), cancel.clone());

        let db = self.db.clone();
        let worker_id = self.worker_id.clone();
        let task_id = task_id.to_string();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        // Race the heartbeat write against shutdown too: an
                        // in-flight write when the co-located DB dies would
                        // otherwise block the full pool acquire timeout. On
                        // shutdown, stop the ticker — there is nothing left to
                        // keep alive.
                        let beat = tokio::select! {
                            _ = shutdown_signal().cancelled() => break,
                            r = crud::update_queue_heartbeat(&db, &task_id, &worker_id) => r,
                        };
                        match beat {
                            Ok(true) => {}
                            // This ticker outlives `handle_task` only by the
                            // width of one tick, but that is enough to land
                            // after graceful release. Stop rather than stamp a
                            // row we no longer hold.
                            Ok(false) => {
                                tracing::debug!(
                                    task_id = %task_id,
                                    "heartbeat: claim no longer held; stopping ticker"
                                );
                                break;
                            }
                            Err(e) => {
                                // Expected once the co-located DB goes down on
                                // shutdown; a real failure otherwise.
                                if is_shutting_down() {
                                    tracing::debug!(
                                        task_id = %task_id,
                                        "heartbeat failed during shutdown: {e}"
                                    );
                                } else {
                                    tracing::warn!(task_id = %task_id, "heartbeat failed: {e}");
                                }
                                break;
                            }
                        }
                    }
                    _ = cancel_clone.cancelled() => break,
                    // Idle between ticks: abandon at once rather than waiting a
                    // full interval to notice shutdown.
                    _ = shutdown_signal().cancelled() => break,
                }
            }
        });
        cancel
    }
}

#[cfg(test)]
mod worker_id_tests {
    use super::{build_worker_id, sanitize_segment};

    #[test]
    fn sanitize_keeps_safe_chars_and_lowercases() {
        assert_eq!(sanitize_segment("Prod"), "prod");
        assert_eq!(sanitize_segment("ip-10-2-3-4"), "ip-10-2-3-4");
        assert_eq!(sanitize_segment("oxy_worker.1"), "oxy_worker.1");
    }

    #[test]
    fn sanitize_maps_unsafe_chars_and_trims_dashes() {
        assert_eq!(sanitize_segment("a/b:c d"), "a-b-c-d");
        assert_eq!(sanitize_segment("--edge--"), "edge");
        assert_eq!(sanitize_segment("staging!!"), "staging");
    }

    #[test]
    fn sanitize_empty_becomes_unknown() {
        assert_eq!(sanitize_segment(""), "unknown");
        assert_eq!(sanitize_segment("   "), "unknown");
        assert_eq!(sanitize_segment("///"), "unknown");
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "h".repeat(100);
        assert_eq!(sanitize_segment(&long).len(), 40);
    }

    #[test]
    fn worker_id_has_three_dot_separated_segments() {
        let id = build_worker_id();
        let parts: Vec<&str> = id.split('·').collect();
        assert_eq!(parts.len(), 3, "worker id should be env·host·short: {id}");
        // short suffix is 8 hex chars
        assert_eq!(parts[2].len(), 8);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
        // env + host segments are never blank
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
    }

    #[test]
    fn worker_id_is_stable_within_a_process() {
        let a = build_worker_id();
        let b = build_worker_id();
        assert_eq!(a, b, "worker_id must be process-stable, not per-transport");
    }

    #[test]
    fn process_worker_id_matches_build_worker_id() {
        assert_eq!(super::process_worker_id(), build_worker_id());
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::{begin_shutdown, is_shutting_down, shutdown_signal};

    /// `begin_shutdown` must set BOTH the level-triggered gate and cancel the
    /// edge-triggered signal — the signal is what lets an in-flight DB poll
    /// abandon promptly instead of blocking the pool acquire timeout.
    ///
    /// Relies on nextest's process-per-test isolation: the two globals are
    /// process-wide and monotonic (never reset), so this test owns a fresh
    /// process where both start clear. Under a shared-process runner it would
    /// still pass on a first run but is not re-entrant by design — shutdown is
    /// a one-way door.
    #[test]
    fn begin_shutdown_sets_flag_and_cancels_signal() {
        assert!(
            !is_shutting_down(),
            "flag must start clear in a fresh process"
        );
        assert!(
            !shutdown_signal().is_cancelled(),
            "signal must start un-cancelled in a fresh process"
        );

        begin_shutdown();

        assert!(
            is_shutting_down(),
            "begin_shutdown must set the shutting-down flag"
        );
        assert!(
            shutdown_signal().is_cancelled(),
            "begin_shutdown must cancel the shutdown signal so in-flight polls can race it"
        );

        // Idempotent: shutdown is monotonic, so a second call is a no-op, not a
        // panic or a reset.
        begin_shutdown();
        assert!(is_shutting_down());
        assert!(shutdown_signal().is_cancelled());
    }
}

#[cfg(test)]
mod pool_backoff_tests {
    use super::*;

    #[test]
    fn pool_acquire_failure_is_recognised() {
        // The shape prod actually logged, arriving as a wrapped conn error.
        let wrapped = sea_orm::DbErr::Custom(
            "Failed to acquire connection from pool: Connection pool timed out".to_string(),
        );
        assert!(is_pool_acquire_failure(&wrapped));
        // A query error must NOT get the long backoff — it is not starvation
        // and delaying it would slow the queue down for no reason.
        let query = sea_orm::DbErr::Custom("relation \"agentic_task_queue\" does not exist".into());
        assert!(!is_pool_acquire_failure(&query));
    }

    #[test]
    fn backoff_grows_then_caps_at_30s() {
        let off: u64 = 42;
        assert_eq!(pool_starvation_backoff(1, off).as_millis() as u64, 1_042);
        assert_eq!(pool_starvation_backoff(2, off).as_millis() as u64, 2_042);
        assert_eq!(pool_starvation_backoff(5, off).as_millis() as u64, 16_042);
        // Ceiling holds, and holds for an absurd streak without overflowing.
        assert_eq!(pool_starvation_backoff(6, off).as_millis() as u64, 30_042);
        assert_eq!(
            pool_starvation_backoff(u32::MAX, off).as_millis() as u64,
            30_042
        );
    }

    #[test]
    fn backoff_is_always_longer_than_a_fast_poll_interval() {
        // The whole point: a starved claimer must not re-enter the pool's FIFO
        // waiter queue on the ordinary poll cadence.
        for streak in 1..=8 {
            assert!(pool_starvation_backoff(streak, 0).as_millis() >= 1_000);
        }
    }

    #[tokio::test]
    async fn retired_transport_returns_none_without_touching_the_db() {
        use agentic_core::transport::WorkerTransport;
        // A `Disconnected` handle errors on ANY query, so reaching `None` at all
        // proves the retirement check runs before the claim round-trip.
        let t =
            DurableTransport::with_config(DatabaseConnection::default(), Duration::from_millis(10));
        t.retire_worker_loop();
        assert!(
            t.recv_assignment().await.is_none(),
            "a retired transport must end its worker loop"
        );
    }

    #[tokio::test]
    async fn retirement_is_the_only_exit() {
        use agentic_core::transport::WorkerTransport;
        // The regression this guards: before retirement existed `recv_assignment`
        // had no `None` path at all, so `Worker::run` never broke and every
        // finished run left a poller hitting the queue for the life of the
        // process. A live transport must still never exit on its own...
        let t =
            DurableTransport::with_config(DatabaseConnection::default(), Duration::from_millis(10));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), t.recv_assignment())
                .await
                .is_err(),
            "recv_assignment must not return None while the transport is live"
        );
        // ...and must exit promptly once retired, even though the call above
        // left it parked mid-loop.
        t.retire_worker_loop();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), t.recv_assignment())
                .await
                .expect("retirement must be observed promptly")
                .is_none()
        );
    }

    #[test]
    fn co_located_transports_desynchronise() {
        // The regression this guards: an offset derived from `worker_id` would be
        // IDENTICAL for every transport in this process — `process_worker_id` is
        // process-stable — and would therefore desynchronise nothing, which is
        // the entire point of having an offset.
        let a =
            DurableTransport::with_config(DatabaseConnection::default(), Duration::from_secs(1));
        let b =
            DurableTransport::with_config(DatabaseConnection::default(), Duration::from_secs(1));
        assert_eq!(
            a.worker_id, b.worker_id,
            "worker_id is process-stable; that is exactly why it cannot carry the offset"
        );
        assert_ne!(
            a.backoff_offset_ms, b.backoff_offset_ms,
            "co-located transports must back off at different instants"
        );
        assert_ne!(
            pool_starvation_backoff(3, a.backoff_offset_ms),
            pool_starvation_backoff(3, b.backoff_offset_ms)
        );
        // …and both stay sub-second, so the backoff itself stays bounded.
        assert!(a.backoff_offset_ms < 1_000 && b.backoff_offset_ms < 1_000);
    }
}
