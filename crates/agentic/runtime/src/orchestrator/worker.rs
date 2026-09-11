//! Worker: pulls task assignments from the transport and executes them.
//!
//! The worker is domain-agnostic — it delegates actual pipeline/automation
//! execution to a [`TaskExecutor`] injected by the pipeline layer.

use std::sync::Arc;

use agentic_core::delegation::{TaskAssignment, TaskOutcome};
use agentic_core::transport::{WorkerMessage, WorkerTransport};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

/// Env var controlling the per-worker in-flight task cap.
///
/// "Per-worker" deliberately, not per-run: a single worker process holds
/// at most this many tasks executing concurrently. With multiple worker
/// processes (today: one per HTTP automation run; future: a remote worker
/// pool), the global ceiling scales as `n_workers * MAX_INFLIGHT` — same
/// semantic Sidekiq / Celery use. A future per-run or per-tenant cap
/// would layer on top via DB-side claim predicates, not by reinterpreting
/// this value.
pub const MAX_INFLIGHT_ENV: &str = "OXY_WORKER_MAX_INFLIGHT";

/// Default in-flight cap when [`MAX_INFLIGHT_ENV`] is unset or unparsable.
///
/// 32 gives a single local run enough headroom for IO-bound loops
/// (HTTP fetches, fast warehouse queries) without exhausting the
/// Postgres pool (DEFAULT_MAX_CONNECTIONS = 80, and it must stay
/// comfortably above this cap — see the floor discussion in
/// internal-docs/worker-fleet.md) or stampeding a downstream
/// LLM provider. The previous value of 16 was conservative for the
/// agentic chat path; raised when audit showed the LLM round-trip,
/// not the local semaphore, is the actual bottleneck for normal runs.
///
/// Operators with tighter downstream limits override via
/// [`MAX_INFLIGHT_ENV`].
pub const DEFAULT_MAX_INFLIGHT: usize = 32;

/// How often a worker re-stamps `agentic_task_queue.last_heartbeat` for a task
/// it holds.
///
/// **Must stay well under `visibility_timeout_secs`** (default 60s, set in
/// `crud::queue::enqueue_task`): that ratio is the entire liveness contract
/// between a worker and the reaper. Every driver that claims a task — the
/// pooled [`Worker`] here and the two virtual workers in `agentic-pipeline` —
/// uses this same value, so the relationship is stated once rather than
/// re-derived per call site.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

fn read_max_inflight() -> usize {
    std::env::var(MAX_INFLIGHT_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_INFLIGHT)
}

// ── ExecutingTask ────────────────────────────────────────────────────────────

/// Handle to a running task, returned by [`TaskExecutor::execute`].
///
/// Events are pre-serialized `(event_type, payload)` pairs — the worker
/// forwards them to the coordinator without inspecting domain-specific content.
pub struct ExecutingTask {
    /// Pre-serialized events from the running task.
    pub events: mpsc::Receiver<(String, Value)>,
    /// Outcomes from the task. A pipeline may produce multiple outcomes
    /// (e.g. `Suspended` followed by `Done` after resume).
    pub outcomes: mpsc::Receiver<TaskOutcome>,
    /// Cancel the task.
    pub cancel: CancellationToken,
    /// Send an answer to resume a suspended task.
    ///
    /// For pipelines, this feeds the orchestrator's internal suspend/resume
    /// loop.  For automations and other tasks that don't suspend, this is `None`.
    pub answers: Option<mpsc::Sender<String>>,
}

// ── TaskExecutor ─────────────────────────────────────────────────────────────

/// Knows how to start pipelines and automations.
///
/// Implemented by the pipeline layer (`agentic-pipeline`), which has access to
/// all domain crates. The runtime only sees this trait.
#[async_trait]
pub trait TaskExecutor: Send + Sync + 'static {
    /// Start executing a task assignment, returning a handle to the running task.
    async fn execute(&self, assignment: TaskAssignment) -> Result<ExecutingTask, String>;

    /// Resume a task from saved state after a server restart.
    ///
    /// Called by the recovery pipeline for tasks that were running or
    /// suspended when the server crashed. The default implementation
    /// returns an error — implementors that support restart-resume
    /// should override this.
    async fn resume_from_state(
        &self,
        _run: &crate::lifecycle::entity::run::Model,
        _suspend_data: Option<agentic_core::human_input::SuspendedRunData>,
    ) -> Result<ExecutingTask, String> {
        Err("resume_from_state not supported".to_string())
    }
}

// ── CustomTaskRegistry ───────────────────────────────────────────────────────

/// Maps `TaskSpec::Custom { kind }` → executor. Built by the host (which owns
/// the concrete executors, e.g. `oxy-app`'s `HealthEvalTaskExecutor`) and
/// injected into the global-run driver. The pipeline layer's
/// `PipelineTaskExecutor` consults it in its `Custom` arm, so a new Custom kind
/// gets a durable fleet execution path without the pipeline crate importing the
/// host. See `internal-docs/agentic-runtime-integration.md` ("One-shot queue
/// work: `TaskSpec::Custom` + `CustomTaskRegistry`").
#[derive(Default, Clone)]
pub struct CustomTaskRegistry {
    handlers: std::collections::HashMap<String, Arc<dyn TaskExecutor>>,
}

impl CustomTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kind: impl Into<String>, exec: Arc<dyn TaskExecutor>) {
        self.handlers.insert(kind.into(), exec);
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn TaskExecutor>> {
        self.handlers.get(kind).cloned()
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    struct Dummy;
    #[async_trait]
    impl TaskExecutor for Dummy {
        async fn execute(&self, _a: TaskAssignment) -> Result<ExecutingTask, String> {
            Err("dummy".into())
        }
    }

    #[test]
    fn register_and_get() {
        let mut reg = CustomTaskRegistry::new();
        reg.register("health_eval_workspace", Arc::new(Dummy));
        assert!(reg.get("health_eval_workspace").is_some());
        assert!(reg.get("nope").is_none());
    }
}

// ── Worker ───────────────────────────────────────────────────────────────────

/// A pull-based worker that receives assignments and executes them.
pub struct Worker {
    transport: Arc<dyn WorkerTransport>,
    executor: Arc<dyn TaskExecutor>,
    /// Caps how many tasks this worker process executes concurrently.
    /// See [`MAX_INFLIGHT_ENV`] / [`DEFAULT_MAX_INFLIGHT`].
    inflight: Arc<Semaphore>,
}

impl Worker {
    pub fn new(transport: Arc<dyn WorkerTransport>, executor: Arc<dyn TaskExecutor>) -> Self {
        Self::with_max_inflight(transport, executor, read_max_inflight())
    }

    /// Constructor with an explicit in-flight cap.
    ///
    /// Tests use this to assert backpressure semantics deterministically;
    /// production code prefers [`Worker::new`] which reads the env var.
    pub fn with_max_inflight(
        transport: Arc<dyn WorkerTransport>,
        executor: Arc<dyn TaskExecutor>,
        max_inflight: usize,
    ) -> Self {
        let cap = max_inflight.max(1);
        tracing::debug!(target: "worker", max_inflight = cap, "worker constructed");
        Self {
            transport,
            executor,
            inflight: Arc::new(Semaphore::new(cap)),
        }
    }

    /// Run the worker loop. Pulls assignments, executes them, and forwards
    /// events and outcomes back to the coordinator via the transport.
    ///
    /// Returns when the transport's assignment channel is closed.
    ///
    /// Backpressure: a per-worker `Semaphore` caps concurrent in-flight
    /// tasks. We acquire the permit *before* claiming the next assignment
    /// so a saturated worker leaves the task in `agentic_task_queue` for
    /// another worker (or this one, later) — claiming first would lock
    /// the row under us while we wait for capacity, defeating the cap.
    pub async fn run(&self) {
        tracing::debug!(target: "worker", "worker run loop started");
        loop {
            // `acquire_owned` only errors if the Semaphore is closed; we
            // never close it, so the `expect` is structurally infallible.
            let permit = Arc::clone(&self.inflight)
                .acquire_owned()
                .await
                .expect("worker inflight semaphore closed");
            let Some(assignment) = self.transport.recv_assignment().await else {
                break;
            };
            let task_id = assignment.task_id.clone();
            let transport = Arc::clone(&self.transport);
            let executor = Arc::clone(&self.executor);

            tokio::spawn(async move {
                Self::handle_task(transport, executor, task_id, assignment).await;
                // Permit released only after handle_task fully returns —
                // not on each Suspended outcome — so a long-suspended
                // pipeline still occupies a slot until terminal.
                drop(permit);
            });
        }
        tracing::debug!(target: "worker", "assignment channel closed, shutting down");
    }

    /// One span per durable task execution — the span this system was most
    /// missing.
    ///
    /// `handle_task` is `tokio::spawn`ed from [`Worker::run`], so until now it
    /// ran with no span and no parent at all. Three things followed, all of
    /// them measured in
    /// `internal-docs/2026-09-09-hyperdx-observability-findings.md`: every
    /// `worker` log line reached HyperDX with no trace id (§3 — 0 of 4,179 on
    /// `oxy-worker`); the `llm_round` spans created below it (`gen_ai.*`,
    /// fully implemented in `agentic_llm::genai`) arrived as orphan roots
    /// rather than as children of the run that made the call (§7); and there
    /// was no span anywhere whose duration was "how long this task took",
    /// which is the first question anyone asks about a queued job.
    ///
    /// Being the root of the task's span tree is the point, so this is
    /// deliberately not `skip_all`-with-nothing: `task_id` and `run_id` are
    /// the two ids the queue, the event stream and the product Traces console
    /// all key on, so a HyperDX trace joins to `agentic_task_queue` rows and
    /// to the tenant-visible run without a lookup.
    #[tracing::instrument(
        // Structural, not incidental: a claimed task is the root of its own
        // trace whatever span happens to be current where it is spawned.
        // `parent` must come BEFORE `target`: tracing-attributes 0.1.31 checks
        // `args.target.is_some()` in its `parent` branch and rejects the pair
        // in the other order ("expected only a single `parent` argument").
        parent = None,
        target = "worker",
        name = "agentic_task",
        skip_all,
        fields(
            task_id = %task_id,
            run_id = %assignment.run_id,
            parent_task_id = assignment.parent_task_id.as_deref().unwrap_or(""),
            spec_kind = tracing::field::Empty,
        )
    )]
    async fn handle_task(
        transport: Arc<dyn WorkerTransport>,
        executor: Arc<dyn TaskExecutor>,
        task_id: String,
        assignment: TaskAssignment,
    ) {
        tracing::info!(target: "worker", task_id = %task_id, spec_type = ?assignment.spec, "received task assignment");

        // Surface the claim on the run's event stream. Admins debugging a
        // run can see exactly when a worker picked up each task, which is
        // helpful for triaging "stuck claiming" vs "stuck executing"
        // cases that the existing tracing logs make hard to distinguish.
        let spec_kind = crate::orchestrator::coordinator::source_type_for_spec(&assignment.spec);
        tracing::Span::current().record("spec_kind", spec_kind.as_str());
        let _ = transport
            .send(WorkerMessage::Event {
                task_id: task_id.clone(),
                event_type: "worker_task_claimed".to_string(),
                payload: serde_json::json!({
                    "task_id": &task_id,
                    "spec_kind": spec_kind,
                    "parent_task_id": assignment.parent_task_id,
                }),
            })
            .await;

        let cancel_token = transport.cancellation_token(&task_id);

        let executing = match executor.execute(assignment).await {
            Ok(e) => e,
            Err(msg) => {
                tracing::error!(target: "worker", task_id = %task_id, error = %msg, "executor failed to start task");
                let _ = transport
                    .send(WorkerMessage::Outcome {
                        task_id,
                        outcome: TaskOutcome::Failed(msg),
                    })
                    .await;
                return;
            }
        };

        // Spawn heartbeat loop — DurableTransport updates DB, LocalTransport no-ops.
        let heartbeat_cancel = transport.spawn_heartbeat(&task_id, HEARTBEAT_INTERVAL);

        // Forward cancellation from transport to the executing task.
        let task_cancel = executing.cancel.clone();
        let cancel_fwd = tokio::spawn({
            let cancel_token = cancel_token.clone();
            let task_id = task_id.clone();
            async move {
                cancel_token.cancelled().await;
                tracing::info!(target: "worker", task_id = %task_id, "cancellation forwarded to executing task");
                task_cancel.cancel();
            }
        });

        // Forward events to coordinator.
        let event_fwd = {
            let transport = Arc::clone(&transport);
            let task_id = task_id.clone();
            tokio::spawn(async move {
                let mut events = executing.events;
                while let Some((event_type, payload)) = events.recv().await {
                    if transport
                        .send(WorkerMessage::Event {
                            task_id: task_id.clone(),
                            event_type,
                            payload,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        };

        // Forward all outcomes (pipeline may produce Suspended then Done).
        let mut outcomes = executing.outcomes;
        // Track whether the driver produced any resolving outcome (a terminal
        // Done/Failed/Cancelled, or a Suspended). A `Suspended` is a legitimate
        // non-hang stopping point after which the driver normally drops the
        // sender, so we must NOT treat that close as a failure. Only a channel
        // close with *no* outcome at all means the driver task died (panic or
        // early drop) before reporting anything.
        let mut saw_any_outcome = false;
        // Did the driver stop by *suspending*, i.e. still holding the claim?
        // See the `heartbeat_cancel` decision at the end of this function.
        let mut parked_suspended = false;
        while let Some(outcome) = outcomes.recv().await {
            saw_any_outcome = true;
            let is_terminal = matches!(
                outcome,
                TaskOutcome::Done { .. } | TaskOutcome::Failed(_) | TaskOutcome::Cancelled
            );
            // Recomputed every iteration, so a `Suspended` followed by `Done`
            // (the pipeline's normal resume-in-place shape) lands on `false`.
            //
            // Assigned BEFORE the `Deferred` early-`break` below, and that
            // ordering is load-bearing: a deferral hands the claim back, so a
            // `Suspended` → `Deferred` sequence has to land on `false` too.
            // Moving this past the break would keep the ticker alive for a row
            // this worker no longer holds.
            parked_suspended = matches!(outcome, TaskOutcome::Suspended { .. });
            // A deferral is not an outcome. Translate it into `Defer` so the
            // task returns to the queue unrun, and stop consuming: sending it
            // as an `Outcome` would have the coordinator record a result for a
            // task that never executed.
            if let TaskOutcome::Deferred {
                delay_secs,
                max_wait_secs,
                reason,
            } = outcome
            {
                tracing::info!(
                    target: "worker",
                    task_id = %task_id, delay_secs, %reason,
                    "task deferred; handing it back to the queue"
                );
                let _ = transport
                    .send(WorkerMessage::Defer {
                        task_id: task_id.clone(),
                        delay_secs,
                        max_wait_secs,
                        reason,
                    })
                    .await;
                break;
            }
            let outcome_type = match &outcome {
                TaskOutcome::Done { .. } => "Done",
                TaskOutcome::Suspended { .. } => "Suspended",
                TaskOutcome::Failed(_) => "Failed",
                TaskOutcome::Cancelled => "Cancelled",
                TaskOutcome::Deferred { .. } => unreachable!("handled above"),
            };
            tracing::info!(
                target: "worker",
                task_id = %task_id,
                outcome_type,
                is_terminal,
                "forwarding outcome"
            );
            // A dropped `Suspended` is the one send failure that must not stay
            // silent: `parked_suspended` is already true, so the ticker would be
            // kept alive for a suspension no coordinator ever received, and the
            // row would sit `claimed` on a fresh heartbeat with every backstop
            // disarmed — the same state `handle_suspended`'s map-miss arm exists
            // to prevent, reached one hop earlier. Letting the claim go stale
            // instead hands it to the ordinary reaper → `find_stuck_runs` chain.
            //
            // Unreachable today (`DurableTransport` owns both ends of this
            // channel, so it cannot close while the worker holds the transport),
            // which is exactly why it is worth pinning rather than discarding.
            if let Err(e) = transport
                .send(WorkerMessage::Outcome {
                    task_id: task_id.clone(),
                    outcome,
                })
                .await
            {
                tracing::error!(
                    target: "worker",
                    // Shared across all three drivers so the rule is filterable
                    // as one thing; their tracing targets differ by design.
                    rule = "dropped-outcome",
                    task_id = %task_id,
                    outcome_type,
                    error = %e,
                    "failed to deliver outcome to the coordinator"
                );
                parked_suspended = false;
            }
            if is_terminal {
                break;
            }
        }

        // The outcome channel closed without the driver ever reporting an
        // outcome — it died (panic or early drop). Synthesize a Failed so the
        // run row leaves "running" and the SSE emits a terminal event instead of
        // hanging forever.
        if !saw_any_outcome {
            tracing::error!(
                target: "worker",
                task_id = %task_id,
                "driver terminated without an outcome; synthesizing Failed"
            );
            let _ = transport
                .send(WorkerMessage::Outcome {
                    task_id: task_id.clone(),
                    outcome: TaskOutcome::Failed(
                        "driver terminated without an outcome (panic or early drop)".to_string(),
                    ),
                })
                .await;
        }

        // Clean up.
        //
        // The heartbeat proves the CLAIM is still owned by a live process — not
        // that a task future is still running. Those two came apart for
        // suspended tasks and that gap was a bug: `DurableTransport` writes
        // nothing for `TaskOutcome::Suspended` on purpose (the row must stay
        // `claimed` so it can resume), but suspending also completes the driver
        // future, so cancelling the ticker here froze `last_heartbeat` on a row
        // the reaper reads as "worker died". It re-queued the task at the
        // visibility timeout, a worker re-claimed it, the decider re-ran and
        // delegated the SAME step again — up to `max_claims` copies of any
        // delegated step that outlives 60s (airway pipelines, routinely).
        //
        // So: keep beating while parked.
        //
        // What retires the ticker is `DurableTransport::retire_heartbeat`, at
        // the points where this process stops owning the claim (terminal
        // outcome, deferral, or a re-claim of the same `task_id`) — NOT the
        // heartbeat predicate going unsatisfied. Do not weaken that to "it
        // stops itself once the row is no longer ours": `worker_id` is
        // process-stable and `update_queue_heartbeat` carries no fencing
        // token, so after the resume path re-enqueues this `task_id` and this
        // same process re-claims it — typically within milliseconds, far
        // inside one tick — a stale ticker's predicate matches all over again
        // and it never stops.
        //
        // Process death still stops every ticker, so a genuinely abandoned
        // suspension goes stale, gets reaped, and reaches `find_stuck_runs`
        // recovery as before — that path *depends* on the reaper freeing the
        // claim first.
        if !parked_suspended {
            heartbeat_cancel.cancel();
        }
        event_fwd.await.ok();
        cancel_fwd.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::transport::LocalTransport;
    use agentic_core::delegation::TaskSpec;
    use agentic_core::transport::{CoordinatorTransport, WorkerTransport};
    use serde_json::json;

    /// Helper: coordinator-side trait reference.
    fn coord(t: &LocalTransport) -> &dyn CoordinatorTransport {
        t
    }

    /// Mock executor that emits a few events and returns Done.
    struct MockExecutor;

    #[async_trait]
    impl TaskExecutor for MockExecutor {
        async fn execute(&self, assignment: TaskAssignment) -> Result<ExecutingTask, String> {
            let (event_tx, event_rx) = mpsc::channel(16);
            let (outcome_tx, outcome_rx) = mpsc::channel(4);
            let cancel = CancellationToken::new();

            let task_id = assignment.task_id.clone();
            tokio::spawn(async move {
                for i in 0..3 {
                    let _ = event_tx
                        .send(("test_event".into(), json!({"index": i, "task": &task_id})))
                        .await;
                }
                drop(event_tx);
                let _ = outcome_tx
                    .send(TaskOutcome::Done {
                        answer: format!("done:{task_id}"),
                        metadata: None,
                    })
                    .await;
            });

            Ok(ExecutingTask {
                events: event_rx,
                outcomes: outcome_rx,
                cancel,
                answers: None,
            })
        }
    }

    /// Mock executor that always fails.
    struct FailingExecutor;

    #[async_trait]
    impl TaskExecutor for FailingExecutor {
        async fn execute(&self, _assignment: TaskAssignment) -> Result<ExecutingTask, String> {
            Err("executor error".into())
        }
    }

    #[tokio::test]
    async fn test_worker_pulls_and_executes() {
        let transport = LocalTransport::with_defaults();
        let _worker = Worker::new(
            transport.clone() as Arc<dyn WorkerTransport>,
            Arc::new(MockExecutor),
        );

        let worker_handle = tokio::spawn({
            let worker = Worker::new(
                transport.clone() as Arc<dyn WorkerTransport>,
                Arc::new(MockExecutor),
            );
            async move { worker.run().await }
        });

        coord(&transport)
            .assign(TaskAssignment {
                task_id: "t1".into(),
                parent_task_id: None,
                run_id: "r1".into(),
                spec: TaskSpec::Agent {
                    agent_id: "a".into(),
                    question: "q".into(),
                    extra: None,
                },
                policy: None,
            })
            .await
            .unwrap();

        // Collect events and outcome.
        let mut events = vec![];
        loop {
            match coord(&transport).recv().await {
                Some(WorkerMessage::Event { event_type, .. }) => events.push(event_type),
                Some(WorkerMessage::Outcome { outcome, .. }) => {
                    assert!(matches!(outcome, TaskOutcome::Done { .. }));
                    break;
                }
                Some(WorkerMessage::Defer { .. }) => {
                    panic!("unexpected defer in this test")
                }
                None => panic!("transport closed unexpectedly"),
            }
        }
        // 3 events from MockExecutor plus the lifecycle
        // `worker_task_claimed` the worker emits on claim.
        assert_eq!(events.len(), 4);
        assert_eq!(events[0], "worker_task_claimed");

        // Drop transport sender to shut down worker.
        drop(transport);
        let _ = worker_handle;
    }

    /// Executor that logs from inside `execute` — the stand-in for every
    /// `worker` / `coordinator` line that used to reach HyperDX with no span.
    struct LoggingExecutor;

    #[async_trait]
    impl TaskExecutor for LoggingExecutor {
        async fn execute(&self, _assignment: TaskAssignment) -> Result<ExecutingTask, String> {
            tracing::info!(target: "worker", "executing inside the task");
            Err("stop here; the span is what is under test".into())
        }
    }

    /// Records, for every event, the chain of spans it was emitted inside and
    /// the `task_id` / `run_id` those spans carry.
    mod capture {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id};
        use tracing::{Event, Subscriber};
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::Context;
        use tracing_subscriber::registry::LookupSpan;

        /// A span's name and its recorded fields.
        pub type SpanRecord = (String, HashMap<String, String>);
        /// An event's message and the spans it was emitted inside, innermost first.
        pub type EventRecord = (String, Vec<SpanRecord>);

        #[derive(Default, Clone)]
        pub struct Seen {
            pub events: Arc<Mutex<Vec<EventRecord>>>,
        }

        struct Fields(HashMap<String, String>);
        impl Visit for Fields {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
        }

        impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Seen {
            fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
                let mut fields = Fields(HashMap::new());
                attrs.record(&mut fields);
                if let Some(span) = ctx.span(id) {
                    span.extensions_mut().insert(fields.0);
                }
            }

            fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
                let mut msg = Fields(HashMap::new());
                event.record(&mut msg);
                let chain = ctx
                    .event_scope(event)
                    .map(|scope| {
                        scope
                            .map(|span| {
                                let fields = span
                                    .extensions()
                                    .get::<HashMap<String, String>>()
                                    .cloned()
                                    .unwrap_or_default();
                                (span.name().to_string(), fields)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let message = msg.0.remove("message").unwrap_or_default();
                self.events.lock().unwrap().push((message, chain));
            }
        }
    }

    /// The durable task is the root of its own trace, keyed by the ids the
    /// queue and the product console use. Before `handle_task` was
    /// instrumented, every line below it had no span at all — which is what
    /// `oxy-worker`'s 0% trace-id coverage in HyperDX was measuring.
    ///
    /// Current-thread runtime on purpose: `set_default` is thread-local, and
    /// `Worker::run` spawns `handle_task`, so a multi-thread runtime could run
    /// the task on a thread that never saw this subscriber and pass vacuously.
    #[tokio::test(flavor = "current_thread")]
    async fn a_task_runs_inside_an_agentic_task_span_carrying_its_ids() {
        use tracing_subscriber::layer::SubscriberExt;

        let seen = capture::Seen::default();
        let subscriber = tracing_subscriber::registry().with(seen.clone());
        let _guard = tracing::subscriber::set_default(subscriber);

        let transport = LocalTransport::with_defaults();
        tokio::spawn({
            let transport = transport.clone();
            async move {
                Worker::new(
                    transport as Arc<dyn WorkerTransport>,
                    Arc::new(LoggingExecutor),
                )
                .run()
                .await;
            }
        });
        coord(&transport)
            .assign(TaskAssignment {
                task_id: "task-7".into(),
                parent_task_id: Some("task-6".into()),
                run_id: "run-42".into(),
                spec: TaskSpec::Agent {
                    agent_id: "a".into(),
                    question: "q".into(),
                    extra: None,
                },
                policy: None,
            })
            .await
            .unwrap();
        // The executor fails, so the worker reports an outcome and returns.
        loop {
            match coord(&transport).recv().await {
                Some(WorkerMessage::Outcome { .. }) => break,
                Some(_) => continue,
                None => panic!("transport closed before the task reported"),
            }
        }

        let events = seen.events.lock().unwrap().clone();
        let (_, chain) = events
            .iter()
            .find(|(msg, _)| msg == "executing inside the task")
            .expect("the executor's line was captured");
        let (name, fields) = chain
            .first()
            .expect("the executor's line was emitted inside a span — it had none before");
        assert_eq!(name, "agentic_task");
        assert_eq!(fields.get("task_id").map(String::as_str), Some("task-7"));
        assert_eq!(fields.get("run_id").map(String::as_str), Some("run-42"));
        assert_eq!(
            fields.get("parent_task_id").map(String::as_str),
            Some("task-6")
        );
        assert_eq!(
            chain.len(),
            1,
            "a claimed task is the root of its own trace, not a child of the claim loop: {chain:?}"
        );
    }

    #[tokio::test]
    async fn test_worker_executor_error() {
        let transport = LocalTransport::with_defaults();

        tokio::spawn({
            let transport = transport.clone();
            async move {
                let worker = Worker::new(
                    transport as Arc<dyn WorkerTransport>,
                    Arc::new(FailingExecutor),
                );
                worker.run().await;
            }
        });

        coord(&transport)
            .assign(TaskAssignment {
                task_id: "t1".into(),
                parent_task_id: None,
                run_id: "r1".into(),
                spec: TaskSpec::Agent {
                    agent_id: "a".into(),
                    question: "q".into(),
                    extra: None,
                },
                policy: None,
            })
            .await
            .unwrap();

        // Should get a Failed outcome. Drain lifecycle events first.
        loop {
            match coord(&transport).recv().await {
                Some(WorkerMessage::Event { .. }) => continue,
                Some(WorkerMessage::Outcome {
                    outcome: TaskOutcome::Failed(msg),
                    ..
                }) => {
                    assert_eq!(msg, "executor error");
                    break;
                }
                other => panic!("expected Failed outcome, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn test_worker_suspended_outcome() {
        use agentic_core::delegation::SuspendReason;
        use agentic_core::human_input::SuspendedRunData;

        /// Executor that emits Suspended for Agent specs, Done for Resume specs.
        struct SuspendingExecutor;

        #[async_trait]
        impl TaskExecutor for SuspendingExecutor {
            async fn execute(&self, assignment: TaskAssignment) -> Result<ExecutingTask, String> {
                let (event_tx, event_rx) = mpsc::channel(16);
                let (outcome_tx, outcome_rx) = mpsc::channel(4);
                let cancel = CancellationToken::new();

                let task_id = assignment.task_id.clone();
                let spec = assignment.spec.clone();
                tokio::spawn(async move {
                    let _ = event_tx
                        .send(("test_event".into(), json!({"task": &task_id})))
                        .await;
                    drop(event_tx);
                    match spec {
                        TaskSpec::Agent { .. } => {
                            let _ = outcome_tx
                                .send(TaskOutcome::Suspended {
                                    reason: SuspendReason::HumanInput { questions: vec![] },
                                    resume_data: SuspendedRunData {
                                        from_state: "clarifying".into(),
                                        original_input: "test".into(),
                                        trace_id: "t1".into(),
                                        stage_data: json!({}),
                                        question: "what?".into(),
                                        suggestions: vec![],
                                    },
                                    trace_id: "t1".into(),
                                })
                                .await;
                        }
                        TaskSpec::Resume { .. } => {
                            let _ = outcome_tx
                                .send(TaskOutcome::Done {
                                    answer: "resumed-done".into(),
                                    metadata: None,
                                })
                                .await;
                        }
                        _ => {
                            let _ = outcome_tx
                                .send(TaskOutcome::Failed("unsupported spec".into()))
                                .await;
                        }
                    }
                });

                Ok(ExecutingTask {
                    events: event_rx,
                    outcomes: outcome_rx,
                    cancel,
                    answers: None,
                })
            }
        }

        let transport = LocalTransport::with_defaults();
        tokio::spawn({
            let transport = transport.clone();
            async move {
                let worker = Worker::new(
                    transport as Arc<dyn WorkerTransport>,
                    Arc::new(SuspendingExecutor),
                );
                worker.run().await;
            }
        });

        // Assign an Agent task → should get Suspended outcome.
        coord(&transport)
            .assign(TaskAssignment {
                task_id: "t1".into(),
                parent_task_id: None,
                run_id: "r1".into(),
                spec: TaskSpec::Agent {
                    agent_id: "a".into(),
                    question: "q".into(),
                    extra: None,
                },
                policy: None,
            })
            .await
            .unwrap();

        let mut got_event = false;
        let got_suspended;
        loop {
            match coord(&transport).recv().await {
                Some(WorkerMessage::Event { .. }) => {
                    got_event = true;
                }
                Some(WorkerMessage::Outcome {
                    outcome: TaskOutcome::Suspended { .. },
                    ..
                }) => {
                    got_suspended = true;
                    break;
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert!(got_event, "should have received at least one event");
        assert!(got_suspended, "should have received Suspended outcome");

        // Now assign a Resume task → should get Done outcome.
        coord(&transport)
            .assign(TaskAssignment {
                task_id: "t1".into(),
                parent_task_id: None,
                run_id: "r1".into(),
                spec: TaskSpec::Resume {
                    run_id: "r1".into(),
                    resume_data: agentic_core::human_input::SuspendedRunData {
                        from_state: "clarifying".into(),
                        original_input: "test".into(),
                        trace_id: "t1".into(),
                        stage_data: json!({}),
                        question: "what?".into(),
                        suggestions: vec![],
                    },
                    answer: "the answer".into(),
                },
                policy: None,
            })
            .await
            .unwrap();

        let mut got_resume_event = false;
        loop {
            match coord(&transport).recv().await {
                Some(WorkerMessage::Event { .. }) => {
                    got_resume_event = true;
                }
                Some(WorkerMessage::Outcome {
                    outcome: TaskOutcome::Done { answer, .. },
                    ..
                }) => {
                    assert_eq!(answer, "resumed-done");
                    break;
                }
                other => panic!("unexpected on resume: {other:?}"),
            }
        }
        assert!(
            got_resume_event,
            "should have received event from resumed task"
        );
    }

    #[tokio::test]
    async fn test_worker_cancellation() {
        let transport = LocalTransport::with_defaults();

        struct WaitingExecutor;

        #[async_trait]
        impl TaskExecutor for WaitingExecutor {
            async fn execute(&self, _assignment: TaskAssignment) -> Result<ExecutingTask, String> {
                let (_event_tx, event_rx) = mpsc::channel(1);
                let (outcome_tx, outcome_rx) = mpsc::channel(4);
                let cancel = CancellationToken::new();

                let cancel_clone = cancel.clone();
                tokio::spawn(async move {
                    cancel_clone.cancelled().await;
                    let _ = outcome_tx.send(TaskOutcome::Cancelled).await;
                });

                Ok(ExecutingTask {
                    events: event_rx,
                    outcomes: outcome_rx,
                    cancel,
                    answers: None,
                })
            }
        }

        tokio::spawn({
            let transport = transport.clone();
            async move {
                let worker = Worker::new(
                    transport as Arc<dyn WorkerTransport>,
                    Arc::new(WaitingExecutor),
                );
                worker.run().await;
            }
        });

        coord(&transport)
            .assign(TaskAssignment {
                task_id: "t1".into(),
                parent_task_id: None,
                run_id: "r1".into(),
                spec: TaskSpec::Agent {
                    agent_id: "a".into(),
                    question: "q".into(),
                    extra: None,
                },
                policy: None,
            })
            .await
            .unwrap();

        // Give the worker a moment to start the task.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        coord(&transport).cancel("t1").await.unwrap();

        // Should get Cancelled outcome. Drain any lifecycle events
        // (worker_task_claimed) that arrive before the outcome.
        loop {
            match coord(&transport).recv().await {
                Some(WorkerMessage::Event { .. }) => continue,
                Some(WorkerMessage::Outcome {
                    outcome: TaskOutcome::Cancelled,
                    ..
                }) => break,
                other => panic!("expected Cancelled outcome, got {other:?}"),
            }
        }
    }

    /// Asserts the per-worker `inflight` semaphore caps concurrent
    /// execution: with `max_inflight = 2` and 5 assigned tasks, only 2
    /// should begin executing until earlier ones finish. Without the
    /// semaphore, all 5 would run concurrently (the pre-fix behaviour
    /// that lets a wide fan-out stampede the executor pool).
    #[tokio::test]
    async fn test_worker_inflight_cap_backpressures_claims() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Executor that bumps a counter on execute and parks until
        /// `release` hands it a permit. `Semaphore` (not `Notify`) is
        /// used because Notify only ever stores 1 permit — sending 5
        /// `notify_one()` calls back-to-back would lose four of them
        /// and the test would deadlock on the third task.
        struct CountingExecutor {
            inflight: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
            release: Arc<Semaphore>,
        }

        #[async_trait]
        impl TaskExecutor for CountingExecutor {
            async fn execute(&self, _assignment: TaskAssignment) -> Result<ExecutingTask, String> {
                let (_event_tx, event_rx) = mpsc::channel(1);
                let (outcome_tx, outcome_rx) = mpsc::channel(4);
                let cancel = CancellationToken::new();
                let current = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(current, Ordering::SeqCst);
                let release = Arc::clone(&self.release);
                let inflight = Arc::clone(&self.inflight);
                tokio::spawn(async move {
                    let permit = release.acquire().await.unwrap();
                    permit.forget();
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    let _ = outcome_tx
                        .send(TaskOutcome::Done {
                            answer: "ok".into(),
                            metadata: None,
                        })
                        .await;
                });
                Ok(ExecutingTask {
                    events: event_rx,
                    outcomes: outcome_rx,
                    cancel,
                    answers: None,
                })
            }
        }

        let transport = LocalTransport::with_defaults();
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        // Start at 0 permits; the test releases tasks one-by-one below.
        let release = Arc::new(Semaphore::new(0));

        tokio::spawn({
            let transport = transport.clone();
            let inflight = Arc::clone(&inflight);
            let peak = Arc::clone(&peak);
            let release = Arc::clone(&release);
            async move {
                let worker = Worker::with_max_inflight(
                    transport as Arc<dyn WorkerTransport>,
                    Arc::new(CountingExecutor {
                        inflight,
                        peak,
                        release,
                    }),
                    2,
                );
                worker.run().await;
            }
        });

        // Queue 5 tasks; the per-worker cap is 2.
        for i in 0..5 {
            coord(&transport)
                .assign(TaskAssignment {
                    task_id: format!("t{i}"),
                    parent_task_id: None,
                    run_id: "r1".into(),
                    spec: TaskSpec::Agent {
                        agent_id: "a".into(),
                        question: "q".into(),
                        extra: None,
                    },
                    policy: None,
                })
                .await
                .unwrap();
        }

        // Give the worker time to claim + try to spawn all 5. With
        // backpressure, only 2 should make it into `execute` before
        // they have to wait for permits to free up.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            inflight.load(Ordering::SeqCst),
            2,
            "with cap=2, only 2 tasks should be in flight"
        );

        // Drip-release one task at a time and drain its outcome before
        // releasing the next. This proves a new task actually claimed
        // the freed slot rather than the worker having queued all 5
        // up-front.
        for _ in 0..5 {
            release.add_permits(1);
            loop {
                match coord(&transport).recv().await {
                    Some(WorkerMessage::Event { .. }) => continue,
                    Some(WorkerMessage::Outcome {
                        outcome: TaskOutcome::Done { .. },
                        ..
                    }) => break,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        }
        assert_eq!(inflight.load(Ordering::SeqCst), 0);
        // The peak ever reached must respect the cap. >2 here would mean
        // the worker dispatched a new task without first acquiring a
        // permit — i.e. the backpressure was bypassed.
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak in-flight was {}, must not exceed cap=2",
            peak.load(Ordering::SeqCst)
        );
    }
}

/// The heartbeat ticker tracks CLAIM ownership, not driver-future lifetime.
///
/// Regression cover for: a delegating step that outlives the 60s visibility
/// timeout ran up to `max_claims` times. `DurableTransport` writes nothing for
/// `TaskOutcome::Suspended` so the row stays `claimed` and can resume — but the
/// driver future *completes* when it suspends, and cancelling the ticker there
/// froze `last_heartbeat` on a live row. The reaper read that as a dead worker,
/// re-queued it, and the re-run decider delegated the same step again.
///
/// These tests use a transport that hands back the tokens it minted, so the
/// assertion is on the exact decision that changed rather than on a 60s wait
/// (the reason the bug survived: it is invisible to any test whose step
/// returns inside the visibility timeout).
#[cfg(test)]
mod heartbeat_lifetime_tests {
    use super::*;
    use crate::orchestrator::transport::LocalTransport;
    use agentic_core::delegation::{SuspendReason, TaskSpec};
    use agentic_core::human_input::SuspendedRunData;
    use agentic_core::transport::{CoordinatorTransport, TransportError, WorkerTransport};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Delegates everything to a `LocalTransport`, but records the heartbeat
    /// token it minted for each task so a test can ask whether the worker
    /// cancelled it.
    struct HeartbeatSpy {
        inner: Arc<LocalTransport>,
        tokens: Arc<Mutex<HashMap<String, CancellationToken>>>,
    }

    #[async_trait]
    impl WorkerTransport for HeartbeatSpy {
        async fn recv_assignment(&self) -> Option<TaskAssignment> {
            self.inner.recv_assignment().await
        }
        async fn send(&self, msg: WorkerMessage) -> Result<(), TransportError> {
            self.inner.send(msg).await
        }
        fn cancellation_token(&self, task_id: &str) -> CancellationToken {
            self.inner.cancellation_token(task_id)
        }
        fn spawn_heartbeat(&self, task_id: &str, _interval: Duration) -> CancellationToken {
            let token = CancellationToken::new();
            self.tokens
                .lock()
                .unwrap()
                .insert(task_id.to_string(), token.clone());
            token
        }
    }

    /// Reports `Done` and stops — the claim is given up.
    struct DoneExecutor;

    #[async_trait]
    impl TaskExecutor for DoneExecutor {
        async fn execute(&self, _assignment: TaskAssignment) -> Result<ExecutingTask, String> {
            let (event_tx, event_rx) = mpsc::channel(4);
            let (outcome_tx, outcome_rx) = mpsc::channel(4);
            tokio::spawn(async move {
                let _ = outcome_tx
                    .send(TaskOutcome::Done {
                        answer: "done".into(),
                        metadata: None,
                    })
                    .await;
                drop(event_tx);
            });
            Ok(ExecutingTask {
                events: event_rx,
                outcomes: outcome_rx,
                cancel: CancellationToken::new(),
                answers: None,
            })
        }
    }

    /// The shape of every delegating step: report `Suspended`, then drop both
    /// senders. The claim stays ours; the child task is what runs next.
    struct SuspendingExecutor;

    #[async_trait]
    impl TaskExecutor for SuspendingExecutor {
        async fn execute(&self, _assignment: TaskAssignment) -> Result<ExecutingTask, String> {
            let (event_tx, event_rx) = mpsc::channel(4);
            let (outcome_tx, outcome_rx) = mpsc::channel(4);
            tokio::spawn(async move {
                let _ = outcome_tx
                    .send(TaskOutcome::Suspended {
                        reason: SuspendReason::HumanInput { questions: vec![] },
                        resume_data: SuspendedRunData {
                            from_state: "workflow_decision".into(),
                            original_input: "q".into(),
                            trace_id: "trace".into(),
                            stage_data: serde_json::json!({}),
                            question: "waiting on a child".into(),
                            suggestions: vec![],
                        },
                        trace_id: "trace".into(),
                    })
                    .await;
                drop(event_tx);
            });
            Ok(ExecutingTask {
                events: event_rx,
                outcomes: outcome_rx,
                cancel: CancellationToken::new(),
                answers: None,
            })
        }
    }

    /// Run one task to its first outcome and return the heartbeat token the
    /// worker was handed for it.
    async fn drive_one(executor: Arc<dyn TaskExecutor>, task_id: &str) -> CancellationToken {
        let inner = LocalTransport::with_defaults();
        let tokens: Arc<Mutex<HashMap<String, CancellationToken>>> = Default::default();
        let spy = Arc::new(HeartbeatSpy {
            inner: inner.clone(),
            tokens: tokens.clone(),
        });
        tokio::spawn(async move {
            let worker = Worker::new(spy as Arc<dyn WorkerTransport>, executor);
            worker.run().await;
        });

        (&*inner as &dyn CoordinatorTransport)
            .assign(TaskAssignment {
                task_id: task_id.into(),
                parent_task_id: None,
                run_id: task_id.into(),
                spec: TaskSpec::Agent {
                    agent_id: "a".into(),
                    question: "q".into(),
                    extra: None,
                },
                policy: None,
            })
            .await
            .unwrap();

        // Wait for the first outcome — by then `handle_task` has made its
        // cancel-or-keep decision (the executor drops its outcome sender
        // immediately after, and the decision precedes `event_fwd.await`).
        loop {
            match (&*inner as &dyn CoordinatorTransport).recv().await {
                Some(WorkerMessage::Outcome { .. }) => break,
                Some(_) => continue,
                None => panic!("transport closed before any outcome"),
            }
        }
        tokens
            .lock()
            .unwrap()
            .get(task_id)
            .cloned()
            .expect("worker must spawn a heartbeat for an assigned task")
    }

    /// The control. A terminal outcome gives the claim up, so the ticker must
    /// stop — if this ever stops holding, the test below proves nothing.
    #[tokio::test]
    async fn terminal_outcome_cancels_the_heartbeat() {
        let token = drive_one(Arc::new(DoneExecutor), "t-done").await;
        tokio::time::timeout(Duration::from_secs(5), token.cancelled())
            .await
            .expect("a Done outcome must cancel the heartbeat ticker");
    }

    /// The regression. Suspending keeps the claim, so the ticker must survive:
    /// it is the only thing telling the reaper this row is parked rather than
    /// abandoned.
    #[tokio::test]
    async fn suspension_keeps_the_heartbeat_alive() {
        let token = drive_one(Arc::new(SuspendingExecutor), "t-suspend").await;
        assert!(
            tokio::time::timeout(Duration::from_millis(500), token.cancelled())
                .await
                .is_err(),
            "a suspended task still holds its claim; cancelling the heartbeat here \
             freezes last_heartbeat and the reaper re-queues a task that is merely \
             waiting on a child"
        );
    }
}
