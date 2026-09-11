//! Background startup recovery for in-flight agentic runs.
//!
//! [`cleanup_stale_runs`] (called from [`super::entry::new_agentic_state`])
//! only transitions interrupted runs to `task_status = "needs_resume"`. The
//! runs themselves are not re-driven until [`recover_active_runs`] rebuilds a
//! `PlatformContext` + `BuilderBridges` per workspace, and hands them to the
//! pipeline's recovery entry point.
//!
//! Spawned in the background so the HTTP listener can start binding while
//! recovery is still in progress — a stuck workspace should not keep the
//! server from serving healthchecks.

use std::sync::Arc;

use agentic_http::AgenticState;
use agentic_pipeline::platform::{BuilderBridges, PlatformContext};
use agentic_pipeline::recovery::{recover_active_runs, recover_stranded_runs};
use agentic_pipeline::{BuilderAppRunnerTrait, BuilderTestRunnerTrait};
use agentic_runtime::state::RuntimeState;
use oxy::adapters::secrets::SecretsManager;
use oxy::adapters::workspace::builder::WorkspaceBuilder;
use sea_orm::{DatabaseConnection, EntityTrait};

use crate::agentic_wiring::{OxyProjectContext, build_builder_bridges};
use crate::server::api::middlewares::workspace_context::PreaggCacheCtx;
use crate::server::service::secret_manager::SecretManagerService;
use oxy::config::WorkingCopy;
use oxy_app_core::serve_mode::{LOCAL_WORKSPACE_ID, ServeMode};

/// Handles for the hooks spawned by [`spawn_shutdown_hook`], so the serve
/// command can wait for them before the process exits.
///
/// A process-lifetime registry rather than a value threaded through the
/// router return type, deliberately: `api_router` already returns a tuple
/// consumed at nine call sites (most of them tests), and
/// [`serve_application`](crate::cli::commands::serve) — the only place that
/// owns the shutdown wait — never sees the `AgenticState` the hook is built
/// from. Threading a third element through would be six signature changes to
/// move a value whose lifetime is the process anyway, which is exactly what
/// the shutdown token itself already is.
static SHUTDOWN_HOOKS: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> =
    std::sync::Mutex::new(Vec::new());

/// How long [`await_shutdown_hooks`] waits before giving up on the hooks.
///
/// Well inside a default Kubernetes 30s termination grace period, so a slow
/// or wedged database can never be the reason a pod gets SIGKILLed.
const SHUTDOWN_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Spawn the graceful-shutdown hook. Returns immediately.
///
/// When `agentic_state.shutdown_token` is cancelled (the serve command
/// forwards SIGINT/SIGTERM into it), mark every active run as `"shutdown"`
/// and signal their cancel channels, then release this process's durable
/// queue claims. Runs with `task_status = "shutdown"` are picked up by
/// [`recover_active_runs`] on the next startup, so this is the resumable
/// counterpart to the `"cancelled"` state set by user-initiated cancel.
///
/// The claim release happens here, in `oxy-app`, rather than inside
/// `RuntimeState::shutdown_all` — `agentic-runtime`'s `lifecycle/` sub-layer
/// (which owns `shutdown_all`) must have zero deps on its own `orchestrator/`
/// sub-layer (see `crates/agentic/runtime/CLAUDE.md`), but `oxy-app` may
/// legitimately import both, so this is where the two halves of "shut this
/// process down" are stitched back together.
///
/// The handle is registered in [`SHUTDOWN_HOOKS`]; call
/// [`await_shutdown_hooks`] on the way out of `serve` so the release
/// actually lands instead of racing process exit.
pub(super) fn spawn_shutdown_hook(agentic_state: Arc<AgenticState>) {
    let token = agentic_state.shutdown_token.clone();
    let runtime = agentic_state.runtime.clone();
    let db = agentic_state.db.clone();
    let handle = tokio::spawn(async move {
        token.cancelled().await;

        // Close the claim path FIRST, before anything else touches the queue.
        // The release below flips rows `claimed -> queued`, which fires the
        // NOTIFY trigger and wakes this process's own workers; without this
        // they would re-claim what we just released and die holding it. This
        // is the only ordering guarantee here, and it is one we establish
        // ourselves rather than inherit from `shutdown_all`.
        agentic_runtime::transport::begin_shutdown();

        let count = runtime.shutdown_all(&db).await;
        if count > 0 {
            tracing::info!(
                target: "recovery",
                count,
                "graceful shutdown: marked active runs resumable"
            );
        }

        // Give back every durable-queue claim this process holds so a
        // successor can pick the work up immediately, budget-neutral.
        //
        // Note what this is NOT: `shutdown_all` fires cancel channels
        // fire-and-forget (`tx.send(true).ok()`, no join), so a task that was
        // mid-execution may still be running when its row goes back to
        // `queued`. There is no completion barrier here, and the correctness
        // of that is owned elsewhere — the `try_acquire_driver` CAS prevents
        // double-drive, and the heartbeat's `worker_id`/`claimed` predicate
        // stops a still-ticking heartbeat from re-stamping a row we no longer
        // hold. What the `begin_shutdown` above guarantees is narrower and
        // sufficient: nothing in this process claims *new* work from here on.
        //
        // `process_worker_id_if_initialized` rather than `process_worker_id`:
        // the id is lazily minted, so asking for it here in a process that
        // never built a `DurableTransport` would forge a fresh id and issue a
        // guaranteed-zero-row UPDATE. `None` means we never claimed anything.
        let Some(worker_id) = agentic_runtime::transport::process_worker_id_if_initialized() else {
            return;
        };

        // Order matters: `mark_released_roots_global` matches on this
        // worker's `worker_id` + `queue_status = 'claimed'`, and the drain
        // below clears both (`worker_id -> NULL`, status -> `queued`). Run it
        // first so an orphaned workflow/airway root becomes visible to the
        // global claim path instead of waiting for a process restart — see
        // `mark_released_roots_global`'s doc comment for why this is gated to
        // `workflow`/`airway` and roots only.
        match agentic_runtime::crud::mark_released_roots_global(&db, worker_id).await {
            Ok(0) => {}
            Ok(marked) => tracing::info!(
                target: "recovery",
                marked,
                worker_id,
                "graceful shutdown: made orphaned roots globally recoverable"
            ),
            Err(e) => tracing::warn!(
                target: "recovery",
                error = %e,
                worker_id,
                "graceful shutdown: failed to mark roots global"
            ),
        }

        // `drain_claims_for_worker` rather than a single release pass: a worker
        // that cleared the `is_shutting_down` gate before it was set can still
        // land its claim *after* the release runs, and the `shutdown_all` above
        // does one DB write per active run in between. The drain is bounded (3
        // passes, stopping on the first empty one) so it stays inside
        // `SHUTDOWN_HOOK_TIMEOUT`, which still wraps this whole task.
        match agentic_runtime::crud::drain_claims_for_worker(&db, worker_id).await {
            Ok(0) => {}
            Ok(released) => tracing::info!(
                target: "recovery",
                released,
                worker_id,
                "graceful shutdown: released claims back to the queue"
            ),
            Err(e) => tracing::warn!(
                target: "recovery",
                error = %e,
                worker_id,
                "graceful shutdown: failed to release claims; the reaper will \
                 reclaim them after the visibility timeout"
            ),
        }
    });
    // A poisoned lock only means some other thread panicked while holding a
    // `Vec<JoinHandle>`; the Vec is still structurally sound, so recover
    // rather than turn a shutdown-path detail into a panic.
    SHUTDOWN_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(handle);
}

/// Wait for every registered shutdown hook to finish, bounded by
/// [`SHUTDOWN_HOOK_TIMEOUT`].
///
/// The hooks are detached tasks racing process exit; without this the claim
/// release in `shutdown_all` is a coin flip. Bounded because a slow database
/// must not hold the pod past its termination grace period — on timeout we
/// degrade to the pre-existing behaviour, where the reaper reclaims the
/// claims once the visibility timeout expires.
///
/// `shutdown_token` is the one the hooks are parked on. If it was never
/// cancelled the server is unwinding from an error rather than a signal, so
/// the hooks will never fire and waiting on them would just burn the full
/// timeout and log a shutdown warning for a failure that isn't one.
pub(crate) async fn await_shutdown_hooks(shutdown_token: &tokio_util::sync::CancellationToken) {
    if !shutdown_token.is_cancelled() {
        return;
    }

    let handles: Vec<_> =
        std::mem::take(&mut *SHUTDOWN_HOOKS.lock().unwrap_or_else(|e| e.into_inner()));
    if handles.is_empty() {
        return;
    }

    let drain = async {
        for handle in handles {
            // A hook that panicked has already logged; the remaining hooks
            // still deserve their chance to release.
            let _ = handle.await;
        }
    };

    if tokio::time::timeout(SHUTDOWN_HOOK_TIMEOUT, drain)
        .await
        .is_err()
    {
        tracing::warn!(
            target: "recovery",
            timeout_secs = SHUTDOWN_HOOK_TIMEOUT.as_secs(),
            "graceful shutdown: claim release timed out; the reaper will reclaim"
        );
    }
}

/// The in-process global driver loop: after one-shot startup recovery, re-run
/// recovery on an interval so scheduler-seeded (Phase 2) and crash-orphaned
/// `scope_owned = false` runs are driven to completion without a per-request
/// coordinator. It also fires the periodic schedule + monitor-scan ticks
/// (`tick_schedules` / `tick_monitor_schedules`).
///
/// Enabled by ROLE, not a standalone flag: every role except the stateless
/// `serve` replica runs it (`all` / `ide` / `worker`). `OXY_INPROC_GLOBAL_WORKER`
/// remains an override in both directions (`=0` forces it off on a non-serve
/// node). Firing is exactly-once across replicas via the scheduler `next_run_at`
/// CAS, so several eligible nodes running it concurrently is safe — no leader
/// election needed.
pub(crate) const INPROC_GLOBAL_WORKER_ENV: &str = "OXY_INPROC_GLOBAL_WORKER";
const INPROC_GLOBAL_WORKER_INTERVAL_ENV: &str = "OXY_INPROC_GLOBAL_WORKER_INTERVAL_SECS";
const DEFAULT_INPROC_GLOBAL_WORKER_INTERVAL_SECS: u64 = 30;

pub(crate) fn inproc_global_worker_enabled() -> bool {
    // Explicit env override wins, in both directions.
    if let Ok(v) = std::env::var(INPROC_GLOBAL_WORKER_ENV) {
        return matches!(v.as_str(), "1" | "true" | "yes" | "on");
    }
    // Otherwise derive from the process role: only the stateless serve replica
    // skips the periodic driver (it offloads to the worker fleet). Single source
    // of truth so a single `OXY_ROLE=all` instance always drains its own queue.
    crate::server::role_manifest::role_runs_inprocess_workers(
        crate::server::role_manifest::current_process_role(),
    )
}

fn inproc_global_worker_interval() -> std::time::Duration {
    let secs = std::env::var(INPROC_GLOBAL_WORKER_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_INPROC_GLOBAL_WORKER_INTERVAL_SECS);
    std::time::Duration::from_secs(secs)
}

/// Whether this process may run the **one-shot startup pass** of
/// [`spawn_recovery`] — the `recover_active_runs` sweep that resumes every
/// interrupted run, as opposed to the periodic stranded-run tick.
///
/// The two passes select differently, and only one of them is safe to run from
/// a process that did not host the coordinators:
///
/// - The periodic tick (`find_stuck_runs`) and the latency worker
///   (`find_pending_global_runs`) both carry a **queue predicate**. A run a
///   live coordinator is driving always has a `claimed` — or `queued` +
///   `scope_owned = true` — entry in `agentic_task_queue`, and both queries
///   exclude exactly that. They cannot poach a live run, from any process.
/// - The one-shot pass (`get_resumable_root_runs`) has **no queue predicate**.
///   Its only guard is the driver lease, and no direct-drive path calls
///   `try_acquire_driver` — the lease is taken only inside
///   `recover_single_run`. So a run being direct-driven right now has
///   `driver_id IS NULL` and the one-shot pass selects it.
///
///   Interactive **airway** submits are no longer among those: they enqueue
///   `TaskScope::Global` and are driven through `recover_single_run` like any
///   other queued work. The remaining direct-drivers are analytics, builder,
///   the chunked backfill (`agentic-pipeline::backfill`) and the
///   `oxy airway run` CLI — enough that the premise below is unchanged, but
///   `spawn_airway_run_drive` is no longer the example to reach for.
///
/// That is sound in `oxy serve`, where the pass runs once at boot and the
/// coordinators it would collide with died with the previous process. It is
/// **not** sound in `oxy worker`: a worker restart (rolling deploy, OOM, HPA
/// scale-up) says nothing about whether the IDE node is mid-run, so a
/// worker-side one-shot pass would re-drive live work. Hence the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupPass {
    /// Run it — this process is the one whose death orphaned the runs
    /// (`oxy serve`, whose own direct-drives went down with it).
    Run,
    /// Skip it and go straight to the queue-gated loops (`oxy worker`, which
    /// hosts no coordinators of its own and cannot tell a live peer's run from
    /// an orphan). Costs at most `STRANDED_GRACE_SECS` + one tick of latency:
    /// `find_stuck_runs` selects `needs_resume` / `shutdown` too, so nothing
    /// is lost, only delayed.
    Skip,
}

/// Spawn startup recovery in the background. Returns immediately.
///
/// The spawned task rebuilds per-workspace `PlatformContext` + `BuilderBridges`
/// and hands them to [`recover_active_runs`] so runs interrupted by a server
/// restart actually resume instead of sitting in `needs_resume` forever — see
/// [`StartupPass`] for which callers may do that.
///
/// When [`INPROC_GLOBAL_WORKER_ENV`] is set, the task does not stop after the
/// one-shot pass: it re-runs recovery on an interval (tied to the shutdown
/// token) so the queue has a durable consumer for `scope_owned = false` work.
/// Safe to run from several processes at once: every selection funnels through
/// `recover_single_run`'s `try_acquire_driver` CAS, so the loser of a race
/// skips.
///
/// NOTE: an earlier version of this comment justified repeat calls with
/// "`get_resumable_root_runs` is driver-lease-gated (F1), so an in-flight run a
/// live driver owns is excluded". The function is lease-gated, but that premise
/// is false — a direct-drive never takes the lease, so a direct-driven run has
/// `driver_id IS NULL` and IS selected. What actually keeps callers apart is
/// the queue predicate, which is why the one-shot pass is gated behind
/// [`StartupPass`] rather than run unconditionally. (That note originally named
/// `spawn_airway_run_drive`; airway submits go through the queue now, but
/// analytics, builder, the chunked backfill and the CLI still direct-drive, so
/// the reasoning stands on them.)
pub(crate) fn spawn_recovery(
    agentic_state: Arc<AgenticState>,
    mode: ServeMode,
    // The node's Layer-1 preagg cache, from the same `AppState` the HTTP
    // middleware reads. Threaded down to the two background context builders so
    // queued work — scheduled monitor scans, automations, agentic runs — resolves
    // rollups on the tier its request-driven twin does. Default (no cache) simply
    // compiles to warehouse SQL.
    preagg: PreaggCacheCtx,
    startup: StartupPass,
) {
    let db = agentic_state.db.clone();
    let runtime = agentic_state.runtime.clone();
    let schema_cache = Some(agentic_state.schema_cache.clone());
    let builder_test_runner: Option<Arc<dyn BuilderTestRunnerTrait>> =
        agentic_state.builder_test_runner.clone();
    let builder_app_runner: Option<Arc<dyn BuilderAppRunnerTrait>> =
        agentic_state.builder_app_runner.clone();
    let router = agentic_state.router.clone();
    let shutdown = agentic_state.shutdown_token.clone();
    // §12 FU4c: long-lived cache so repeated periodic ticks (and the
    // latency worker, below) reuse the per-workspace OxyProjectContext
    // instead of rebuilding WorkspaceManager every cycle.
    let ws_cache = super::workspace_cache::new_workspace_context_cache();

    // §12 FU4c latency worker — shared across local and cloud modes.
    // Drains `queued scope_owned=false` rows at sub-second latency
    // instead of waiting for the periodic loop's grace window.
    //
    // The cloud-mode branch (added when `agentic_runs.workspace_id`
    // landed) groups pending rows by workspace and routes each to the
    // right cached `PlatformContext` via `ws_cache`; no need for one
    // worker per workspace.
    if inproc_global_worker_enabled() {
        spawn_latency_worker(
            db.clone(),
            runtime.clone(),
            schema_cache.clone(),
            builder_test_runner.clone(),
            builder_app_runner.clone(),
            router.clone(),
            mode,
            shutdown.clone(),
            ws_cache.clone(),
            preagg.clone(),
        );
    } else if matches!(mode, ServeMode::Cloud) {
        // Reached only when the global driver was explicitly disabled
        // (`OXY_INPROC_GLOBAL_WORKER=0`) on a non-serve node — the derived
        // default already turns it OFF for `serve` (which offloads to the worker
        // fleet) and ON for every other role. Loud signal because in cloud mode
        // with no node draining the queue, `TaskSpec::Compile` tasks (which the
        // compile boundary depends on) never run, so workspaces stay uncompiled
        // and unservable by the stateless fleet. Compile execution needs the
        // workspace working copy on disk, so the node running the driver must
        // have it. See internal-docs/compile-boundary.md.
        tracing::warn!(
            target: "recovery",
            "in-process global driver is explicitly OFF on a cloud node — queued \
             Global tasks (compiles in particular) will NOT be drained here. Ensure \
             a node with the workspace working copy runs the global driver (it is on \
             by default for every role except serve), or compiles never run. See \
             internal-docs/compile-boundary.md."
        );
    }

    tokio::spawn(async move {
        match startup {
            StartupPass::Run => {
                let recovered = run_recovery(
                    &db,
                    runtime.clone(),
                    schema_cache.clone(),
                    builder_test_runner.clone(),
                    builder_app_runner.clone(),
                    router.clone(),
                    mode,
                    false, // one-shot startup recovery: every coordinator is dead
                    ws_cache.clone(),
                    preagg.clone(),
                )
                .await;
                if recovered > 0 {
                    tracing::info!(
                        target: "recovery",
                        recovered,
                        mode = mode.label(),
                        "startup recovery resumed interrupted runs"
                    );
                }
            }
            StartupPass::Skip => tracing::info!(
                target: "recovery",
                mode = mode.label(),
                "skipping the one-shot startup pass: this process hosts no \
                 coordinators, so a resumable-looking run may belong to a live \
                 peer. Stranded work is picked up by the queue-gated loops \
                 below instead"
            ),
        }

        if !inproc_global_worker_enabled() {
            return;
        }
        let interval = inproc_global_worker_interval();
        tracing::info!(
            target: "recovery",
            interval_secs = interval.as_secs(),
            mode = mode.label(),
            "in-process global driver loop enabled (OXY_INPROC_GLOBAL_WORKER)"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!(target: "recovery", "global driver loop: shutdown");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    // Every eligible node drives this tick — no leader election
                    // needed. The work it fans out is already exactly-once across
                    // replicas: `tick_schedules` / `tick_monitor_schedules`
                    // CAS-advance `next_run_at` (only the replica whose UPDATE
                    // hits one row fires — see agentic-pipeline::scheduler), and
                    // stranded-run recovery is driver-lease-gated (F1). So N
                    // concurrent drivers self-dedupe, which is strictly better
                    // HA than a single leader: instant failover, no lease, no
                    // SPOF. The only cost is a little redundant polling, which is
                    // a cheap indexed query.
                    let n = run_recovery(
                        &db,
                        runtime.clone(),
                        schema_cache.clone(),
                        builder_test_runner.clone(),
                        builder_app_runner.clone(),
                        router.clone(),
                        mode,
                        true, // periodic tick: stranded runs only (never poach a live interactive run)
                        ws_cache.clone(),
                        preagg.clone(),
                    )
                    .await;
                    if n > 0 {
                        tracing::info!(
                            target: "recovery",
                            picked_up = n,
                            "global driver loop: drove scope_owned=false runs"
                        );
                    }
                }
            }
        }
    });
}

/// Pure async recovery entry point — separated from [`spawn_recovery`] so
/// tests can drive it without a background task.
pub(super) async fn run_recovery(
    db: &DatabaseConnection,
    runtime: Arc<RuntimeState>,
    schema_cache: Option<
        Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<Arc<dyn BuilderAppRunnerTrait>>,
    router: Arc<dyn agentic_runtime::router::TaskRouter>,
    mode: ServeMode,
    // `true` = periodic global-driver tick (drive stranded runs only);
    // `false` = one-shot startup recovery (drive all resumable runs).
    periodic: bool,
    ws_cache: Arc<super::workspace_cache::WorkspaceContextCache>,
    preagg: PreaggCacheCtx,
) -> usize {
    match mode {
        ServeMode::Local => {
            recover_local(
                db,
                runtime,
                schema_cache,
                builder_test_runner,
                builder_app_runner,
                router,
                periodic,
                ws_cache,
                preagg,
            )
            .await
        }
        ServeMode::Cloud => {
            recover_all_workspaces(
                db,
                runtime,
                schema_cache,
                builder_test_runner,
                builder_app_runner,
                router,
                periodic,
                ws_cache,
                preagg,
            )
            .await
        }
    }
}

async fn recover_local(
    db: &DatabaseConnection,
    runtime: Arc<RuntimeState>,
    schema_cache: Option<
        Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<Arc<dyn BuilderAppRunnerTrait>>,
    router: Arc<dyn agentic_runtime::router::TaskRouter>,
    periodic: bool,
    ws_cache: Arc<super::workspace_cache::WorkspaceContextCache>,
    preagg: PreaggCacheCtx,
) -> usize {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "recovery", error = %e, "local recovery: no cwd, skipping");
            return 0;
        }
    };

    let Some(project_ctx) = ws_cache
        .get_or_build(LOCAL_WORKSPACE_ID, || async {
            build_local_project_ctx(&cwd, db, &preagg).await
        })
        .await
    else {
        return 0;
    };
    let platform: Arc<dyn PlatformContext> = project_ctx.clone();
    let platform_for_monitor: Arc<dyn PlatformContext> = project_ctx.clone();
    // Reused by the Phase 2 scheduler tick (airway seeds need a workspace
    // surface; OxyProjectContext is also a WorkflowWorkspaceContext).
    let workspace: Arc<dyn agentic_pipeline::WorkflowWorkspaceContext> = project_ctx.clone();
    let bridges: Option<BuilderBridges> = Some(build_builder_bridges(project_ctx));

    if periodic {
        // Local-mode workspace_id == LOCAL_WORKSPACE_ID (Uuid::nil()).
        // Stamped on every local run by the `start_*_run` paths; we
        // filter the selection by it so the SELECT can't drift across
        // workspaces if a cloud DB is ever pointed at a local server.
        let recovered = recover_stranded_runs(
            db.clone(),
            runtime,
            platform,
            bridges,
            schema_cache,
            builder_test_runner,
            builder_app_runner,
            router,
            Some(LOCAL_WORKSPACE_ID),
            Some(build_custom_task_registry(db, &preagg)),
        )
        .await;
        // Periodic tick drives the cron scheduler, scoped to this
        // workspace (§12 FU4b). Local mode = the single LOCAL_WORKSPACE_ID.
        let fired =
            agentic_pipeline::scheduler::tick_schedules(db, LOCAL_WORKSPACE_ID, workspace.as_ref())
                .await;
        if fired > 0 {
            tracing::info!(target: "scheduler", fired, workspace_id = %LOCAL_WORKSPACE_ID, "periodic tick fired schedules");
        }
        bootstrap_monitor_schedules(db, LOCAL_WORKSPACE_ID, &cwd).await;
        let monitor_fired = agentic_pipeline::scheduler::tick_monitor_schedules(
            db,
            LOCAL_WORKSPACE_ID,
            platform_for_monitor,
        )
        .await;
        if monitor_fired > 0 {
            tracing::info!(
                target: "metric_monitoring",
                fired = monitor_fired,
                workspace_id = %LOCAL_WORKSPACE_ID,
                "monitor tick enqueued scans"
            );
        }
        // Fire due per-workspace health rows. Cheap indexed SELECT of due rows
        // only; reconciliation of the rows themselves is NOT done here (see the
        // startup branch below).
        let health_fired = agentic_pipeline::scheduler::tick_health_schedules(db).await;
        if health_fired > 0 {
            tracing::info!(target: "health_eval", "health schedule fired");
        }
        let preagg_fired = agentic_pipeline::scheduler::tick_preagg_schedules(db).await;
        if preagg_fired > 0 {
            tracing::info!(target: "preagg", "preagg schedule fired");
        }
        recovered + fired
    } else {
        // One-time startup backfill of per-workspace health schedule rows from
        // compiled config. Steady-state sync is event-driven at compile time
        // (compile_worker::reconcile_health_from_compiled) — never per tick, so
        // the periodic driver never re-resolves config for every workspace.
        // Local mode: the single workspace has no `workspaces` row, so skip the
        // orphan prune (an empty table there is not evidence of orphans).
        reconcile_all_health_schedules(db, false).await;
        reconcile_all_preagg_schedules(db).await;
        recover_active_runs(
            db.clone(),
            runtime,
            platform,
            bridges,
            schema_cache,
            builder_test_runner,
            builder_app_runner,
            router,
            Some(LOCAL_WORKSPACE_ID),
            Some(build_custom_task_registry(db, &preagg)),
        )
        .await
    }
}

async fn recover_all_workspaces(
    db: &DatabaseConnection,
    runtime: Arc<RuntimeState>,
    schema_cache: Option<
        Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<Arc<dyn BuilderAppRunnerTrait>>,
    router: Arc<dyn agentic_runtime::router::TaskRouter>,
    periodic: bool,
    ws_cache: Arc<super::workspace_cache::WorkspaceContextCache>,
    preagg: PreaggCacheCtx,
) -> usize {
    let workspaces = match entity::workspaces::Entity::find().all(db).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::error!(target: "recovery", error = %e, "cloud recovery: failed to list workspaces");
            return 0;
        }
    };

    if workspaces.is_empty() {
        tracing::info!(target: "recovery", "cloud recovery: no workspaces registered");
        return 0;
    }

    let mut total = 0usize;
    // The health tick enqueues per-workspace eval tasks and needs no platform
    // handle; this flag is just a "did we build any workspace context this pass"
    // proxy so the tick fires once per pass after the loop (not per workspace).
    let mut health_platform: Option<Arc<dyn PlatformContext>> = None;
    for ws in &workspaces {
        let Some(ref path) = ws.path else {
            continue;
        };

        let Some(project_ctx) = ws_cache
            .get_or_build(ws.id, || async {
                build_cloud_project_ctx(ws.id, path, db, &preagg).await
            })
            .await
        else {
            // build_cloud_project_ctx already logged the failure cause.
            continue;
        };
        let platform: Arc<dyn PlatformContext> = project_ctx.clone();
        let platform_for_monitor: Arc<dyn PlatformContext> = project_ctx.clone();
        if periodic && health_platform.is_none() {
            health_platform = Some(project_ctx.clone());
        }
        // §12 FU4b: keep a workspace handle for the per-workspace
        // scheduler tick below; airway targets need it to resolve the
        // pipeline file on THIS workspace's filesystem.
        let workspace: Arc<dyn agentic_pipeline::WorkflowWorkspaceContext> = project_ctx.clone();
        let bridges: Option<BuilderBridges> = Some(build_builder_bridges(project_ctx));

        let n = if periodic {
            // Filter to THIS workspace's runs only — otherwise the
            // loop would drive every workspace's stranded rows with
            // the wrong PlatformContext on each iteration (silently
            // pre-FU4c, now an enforced invariant).
            let recovered = recover_stranded_runs(
                db.clone(),
                runtime.clone(),
                platform,
                bridges,
                schema_cache.clone(),
                builder_test_runner.clone(),
                builder_app_runner.clone(),
                router.clone(),
                Some(ws.id),
                Some(build_custom_task_registry(db, &preagg)),
            )
            .await;
            // Per-workspace scheduler tick (§12 FU4b). Each workspace
            // only ticks its own schedules.
            let fired =
                agentic_pipeline::scheduler::tick_schedules(db, ws.id, workspace.as_ref()).await;
            if fired > 0 {
                tracing::info!(
                    target: "scheduler",
                    workspace_id = %ws.id,
                    fired,
                    "cloud tick fired schedules"
                );
            }
            let ws_root = std::path::Path::new(path.as_str());
            bootstrap_monitor_schedules(db, ws.id, ws_root).await;
            let monitor_fired = agentic_pipeline::scheduler::tick_monitor_schedules(
                db,
                ws.id,
                platform_for_monitor,
            )
            .await;
            if monitor_fired > 0 {
                tracing::info!(
                    target: "metric_monitoring",
                    workspace_id = %ws.id,
                    fired = monitor_fired,
                    "monitor tick spawned scans"
                );
            }
            recovered + fired
        } else {
            recover_active_runs(
                db.clone(),
                runtime.clone(),
                platform,
                bridges,
                schema_cache.clone(),
                builder_test_runner.clone(),
                builder_app_runner.clone(),
                router.clone(),
                Some(ws.id),
                Some(build_custom_task_registry(db, &preagg)),
            )
            .await
        };

        if n > 0 {
            tracing::info!(
                target: "recovery",
                workspace_id = %ws.id,
                recovered = n,
                "cloud recovery: resumed runs for workspace"
            );
        }
        total += n;
    }
    if !periodic {
        // One-time startup backfill of per-workspace health schedule rows from
        // compiled config. Steady-state sync is event-driven at compile time
        // (compile_worker::reconcile_health_from_compiled) — never per tick, so
        // the periodic driver never re-resolves config for every workspace.
        reconcile_all_health_schedules(db, true).await;
        reconcile_all_preagg_schedules(db).await;
    }
    // Per-workspace health tick: fire due eval rows once per pass, but only
    // after at least one workspace context was built this pass (proxy for "this
    // node has work to do"). Cheap indexed SELECT of due rows; the tick enqueues
    // per-workspace eval tasks and needs no platform handle.
    if health_platform.is_some() {
        let health_fired = agentic_pipeline::scheduler::tick_health_schedules(db).await;
        if health_fired > 0 {
            tracing::info!(target: "health_eval", "cloud health schedule fired");
        }
        let preagg_fired = agentic_pipeline::scheduler::tick_preagg_schedules(db).await;
        if preagg_fired > 0 {
            tracing::info!(target: "preagg", "cloud preagg schedule fired");
        }
    }
    total
}

// ── Per-workspace context builders (used by the cache, §12 FU4c) ───────────

/// §12 FU4c latency-worker spawn (local + cloud).
///
/// Polls `find_pending_global_runs` every ~1s and drives any
/// freshly-seeded `queued scope_owned = false` runs immediately, instead
/// of waiting for the periodic loop's grace window (≈ grace + interval).
/// Concurrency-safe vs. the periodic loop: both call `recover_single_run`
/// which CAS-acquires the driver lease, so the loser of any race skips.
///
/// **Local mode:** rebuild/reuse the single `LOCAL_WORKSPACE_ID` context
/// every tick and drive any pending row.
///
/// **Cloud mode:** discover which workspaces actually have pending rows
/// via `find_pending_global_runs(None)`, then drive each workspace's
/// rows with its own cached context. This is a single shared worker, not
/// one-per-workspace — the routing is per-row via `agentic_runs.workspace_id`.
#[allow(clippy::too_many_arguments)]
fn spawn_latency_worker(
    db: sea_orm::DatabaseConnection,
    runtime: Arc<RuntimeState>,
    schema_cache: Option<
        Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<Arc<dyn BuilderAppRunnerTrait>>,
    router: Arc<dyn agentic_runtime::router::TaskRouter>,
    mode: ServeMode,
    shutdown: tokio_util::sync::CancellationToken,
    ws_cache: Arc<super::workspace_cache::WorkspaceContextCache>,
    preagg: PreaggCacheCtx,
) {
    // Configurable for soak tuning; default 1s.
    let poll = match std::env::var("OXY_LATENCY_WORKER_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
    {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => std::time::Duration::from_millis(1000),
    };
    // Say out loud what this node will refuse to drive. Both exclusions are
    // silent-by-construction failures otherwise: work simply stops being
    // picked up here, and the only other trace is a per-tick DEBUG line. The
    // airway one in particular has a deployment prerequisite (a worker fleet
    // must exist), so an operator who set the flag on a fleetless install
    // needs to be able to find this in the boot log.
    let excluded = excluded_source_types();
    if !excluded.is_empty() {
        tracing::info!(
            target: "recovery",
            role = crate::server::role_manifest::current_process_role().as_str(),
            excluded = ?excluded,
            "this node declines these run kinds at selection; another node must \
             drive them or they stay queued"
        );
    }
    tracing::info!(
        target: "recovery",
        poll_ms = poll.as_millis() as u64,
        mode = mode.label(),
        "latency worker enabled (OXY_INPROC_GLOBAL_WORKER)"
    );
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!(target: "recovery", "latency worker: shutdown");
                    break;
                }
                _ = tokio::time::sleep(poll) => {
                    // Race the whole tick against shutdown. A poll (or a run
                    // drive) already in flight when a co-located DB dies on
                    // shutdown would otherwise block the full pool
                    // `ACQUIRE_TIMEOUT` (~30s) before erroring, delaying the
                    // bounded shutdown-hook wait that follows serve. On
                    // shutdown we abandon the tick and break; any half-driven
                    // Global run is picked back up by recovery on restart (or
                    // by a peer), gated by the driver lease.
                    let tick = async {
                        match mode {
                            ServeMode::Local => {
                                tick_local(
                                    &db,
                                    &runtime,
                                    schema_cache.as_ref(),
                                    builder_test_runner.as_ref(),
                                    builder_app_runner.as_ref(),
                                    &router,
                                    &ws_cache,
                                    &preagg,
                                )
                                .await
                            }
                            ServeMode::Cloud => {
                                tick_cloud(
                                    &db,
                                    &runtime,
                                    schema_cache.as_ref(),
                                    builder_test_runner.as_ref(),
                                    builder_app_runner.as_ref(),
                                    &router,
                                    &ws_cache,
                                    &preagg,
                                )
                                .await
                            }
                        }
                    };
                    let n = tokio::select! {
                        _ = shutdown.cancelled() => {
                            tracing::info!(target: "recovery", "latency worker: shutdown during poll");
                            break;
                        }
                        n = tick => n,
                    };
                    if n > 0 {
                        tracing::debug!(
                            target: "recovery",
                            picked_up = n,
                            mode = mode.label(),
                            "latency worker: drove pending Global runs"
                        );
                    }
                }
            }
        }
    });
}

/// Local-mode tick: single cached context, no per-row routing needed.
///
/// One span per driver pass, which is what makes the `recovery`, `scheduler`,
/// `metric_monitoring`, `health_eval` and `preagg` lines emitted underneath it
/// correlate: they are the loudest untraced targets on `oxy-worker`, which
/// carried a trace id on **0** of 4,179 log lines
/// (`internal-docs/2026-09-09-hyperdx-observability-findings.md` §3). The span
/// sits on the outer pass rather than on each `tick_*` so a workspace fleet
/// does not multiply one heartbeat into hundreds of spans — one pass, one
/// trace, every line in it.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    target = "recovery",
    name = "driver_tick",
    skip_all,
    fields(mode = "local")
)]
async fn tick_local(
    db: &sea_orm::DatabaseConnection,
    runtime: &Arc<RuntimeState>,
    schema_cache: Option<
        &Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<&Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<&Arc<dyn BuilderAppRunnerTrait>>,
    router: &Arc<dyn agentic_runtime::router::TaskRouter>,
    ws_cache: &Arc<super::workspace_cache::WorkspaceContextCache>,
    preagg: &PreaggCacheCtx,
) -> usize {
    let Ok(cwd) = std::env::current_dir() else {
        return 0;
    };
    let Some(ctx) = ws_cache
        .get_or_build(LOCAL_WORKSPACE_ID, || async {
            build_local_project_ctx(&cwd, db, preagg).await
        })
        .await
    else {
        return 0;
    };
    drive_pending(
        db,
        runtime,
        ctx,
        schema_cache,
        builder_test_runner,
        builder_app_runner,
        router,
        Some(LOCAL_WORKSPACE_ID),
        preagg,
    )
    .await
}

/// Cloud-mode tick: discover which workspaces have pending rows, then
/// drive each one with its own cached context. We probe the unfiltered
/// SELECT once per tick to avoid scanning every workspace in the DB when
/// most have no work; the per-workspace re-select inside `drive_pending`
/// CAS-protects against double-drive across replicas.
///
/// Spanned for the same reason as [`tick_local`], and it matters more here:
/// cloud is the mode the fleet actually runs, so this is the root every
/// scheduler / recovery / monitor line on `oxy-worker` hangs off.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    target = "recovery",
    name = "driver_tick",
    skip_all,
    fields(mode = "cloud")
)]
async fn tick_cloud(
    db: &sea_orm::DatabaseConnection,
    runtime: &Arc<RuntimeState>,
    schema_cache: Option<
        &Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<&Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<&Arc<dyn BuilderAppRunnerTrait>>,
    router: &Arc<dyn agentic_runtime::router::TaskRouter>,
    ws_cache: &Arc<super::workspace_cache::WorkspaceContextCache>,
    preagg: &PreaggCacheCtx,
) -> usize {
    use std::collections::HashSet;
    let pending = match agentic_runtime::crud::find_pending_global_runs(db, None).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "recovery", error = %e, "latency worker: cloud probe failed");
            return 0;
        }
    };
    if pending.is_empty() {
        return 0;
    }

    // A cheap probe-level skip, NOT the gate.
    //
    // This filters only which workspaces are visited: `drive_pending` re-selects
    // per workspace, and that second selection is what feeds the driver lease.
    // The real gate is the `exclude_source_types` argument threaded into
    // `recover_pending_global_runs`. An earlier revision had only this
    // partition and believed it sufficient — it is not, because a workspace
    // with a compile AND any other pending Global run still reached the drive.
    //
    // Decline undrivable runs BEFORE the lease, not after the claim.
    //
    // `claim_task` has no `task_kind` predicate, so any eligible driver wins
    // any row — but a node may be unable (compile without a working copy) or
    // unwanted (airway on the ide, see `excluded_source_types`) as its driver.
    // Failing after the claim makes success a coin flip on poll phase;
    // deferring after the claim is worse still, because only the lease-holder
    // may claim the row, so the deferring process re-selects its own work
    // every few seconds while its heartbeat excludes every other node from
    // `find_pending_global_runs` entirely — ending in a dead-lettered queue
    // row and a run stuck non-terminal.
    //
    // Skipping at selection leaves the run untouched, `driver_id IS NULL`, and
    // therefore selectable by the next eligible tick. That is the handoff the
    // other approaches only claimed.
    //
    // Same set as the gate, from one function, for the reason spelled out on
    // `excluded_source_types`: a probe that skips MORE than the gate silently
    // hides drivable work.
    let exclude = excluded_source_types();
    // Kept whole for `retire_orphaned_runs` below. Retiring a run whose
    // workspace row is gone is a plain DB write that needs no workspace
    // context, so it is NOT a capability this node can lack — filtering it by
    // the driving exclusion would let an orphan of a declined kind sit
    // un-retired, which is the shape that pinned the pool on 2026-09-01.
    // Cheap: a clone of a handful of small structs, only on ticks that found
    // pending work at all.
    //
    // **Scope: the workspaces this node visits.** `workspaces` below is built
    // from the POST-partition list, so a deleted workspace whose only pending
    // runs are of a declined kind is never visited and `all_pending` is never
    // consulted for it. On a split fleet the other role covers that case — a
    // worker retires orphaned airway, an ide retires orphaned compile — so it
    // is not a stranding. Building `workspaces` from `all_pending` instead
    // would close it, at the cost of a per-tick workspace lookup (every 300 ms)
    // for workspaces this node has already decided it will not drive; that
    // trade is not obviously worth it, and this comment exists so the next
    // reader can make it deliberately rather than discover the gap.
    let all_pending = pending.clone();
    let (pending, declined): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .partition(|r| agentic_pipeline::recovery::may_drive(r.source_type.as_deref(), exclude));
    if !declined.is_empty() {
        tracing::debug!(
            target: "recovery",
            count = declined.len(),
            role = crate::server::role_manifest::current_process_role().as_str(),
            excluded = ?exclude,
            "latency worker: leaving runs this node declines for one that takes them"
        );
    }
    if pending.is_empty() {
        return 0;
    }
    let workspaces: HashSet<uuid::Uuid> = pending.iter().map(|r| r.workspace_id).collect();

    let mut total = 0usize;
    for ws_id in workspaces {
        // Resolve workspace path from the workspaces table — cached
        // contexts are keyed by workspace_id, but the FIRST build per id
        // needs the path. Cheap one-row lookup; skipped on cache hits.
        let path = match entity::workspaces::Entity::find_by_id(ws_id).one(db).await {
            Ok(found) => match classify_workspace(found.map(|ws| ws.path)) {
                WorkspaceOutcome::Drive(p) => p,
                WorkspaceOutcome::Retire(reason) => {
                    // `all_pending`, not `pending`. Once this workspace is known
                    // unrunnable, every run pointing at it is unrunnable
                    // regardless of whether THIS node would have driven it.
                    // Passing the filtered list leaves a declined kind — a
                    // compile on a worker, an airway run on a deferring ide —
                    // orphaned and re-selected forever.
                    //
                    // Merge note: this argument was written for the
                    // workspace-row-missing case and applies MORE broadly since
                    // #3062, which retires path-less workspaces through this same
                    // arm. Neither branch had both halves.
                    retire_orphaned_runs(db, ws_id, &all_pending, reason).await;
                    continue;
                }
            },
            // A lookup FAILURE is the one case that must still skip. It says
            // the database was briefly unreachable, not that the workspace is
            // gone — retiring on it would destroy live runs during a blip.
            Err(e) => {
                tracing::warn!(target: "recovery", error = %e, "latency worker: workspace lookup failed");
                continue;
            }
        };

        let Some(ctx) = ws_cache
            .get_or_build(ws_id, || async {
                build_cloud_project_ctx(ws_id, &path, db, preagg).await
            })
            .await
        else {
            continue;
        };
        total += drive_pending(
            db,
            runtime,
            ctx,
            schema_cache,
            builder_test_runner,
            builder_app_runner,
            router,
            Some(ws_id),
            preagg,
        )
        .await;
    }
    total
}

/// What the latency worker should do with a workspace it has just looked up.
///
/// Extracted from [`tick_cloud`]'s match so the decision can be tested without
/// a database. The property worth protecting is not any single arm — it is that
/// **every** non-drivable outcome retires rather than skips, and that is exactly
/// what regressed: the deleted-row arm retired while the path-less arm next to
/// it went on skipping, which is the identical permanent hot loop one door over.
enum WorkspaceOutcome {
    /// The workspace resolves to a path; drive its pending runs.
    Drive(String),
    /// Unrunnable, and no future poll changes that. Retire with this reason.
    Retire(&'static str),
}

/// Classify a `workspaces` lookup that SUCCEEDED.
///
/// `None` — no such row. `Some(None)` — the row exists with no `path`.
/// `Some(Some(p))` — drivable. Deliberately takes the path rather than the
/// model: those two bits are the whole decision, and narrowing the input keeps
/// this callable from a test without constructing an entity.
///
/// A path-less workspace is not the transient state it looks like.
/// `register_project` writes `path` in the same `INSERT` that sets `status`, so
/// a path never "arrives later" — a row without one (local-mode sentinels,
/// demo/listing-only seeded rows) will not grow one, and a pending Global run
/// against it can never be driven.
fn classify_workspace(path: Option<Option<String>>) -> WorkspaceOutcome {
    match path {
        Some(Some(p)) => WorkspaceOutcome::Drive(p),
        Some(None) => WorkspaceOutcome::Retire("workspace has no path"),
        None => WorkspaceOutcome::Retire("workspace no longer exists"),
    }
}

/// Dead-letter every pending Global run on a workspace that cannot be resolved
/// to a path — whether the row is gone or is present without one. See
/// [`classify_workspace`], which decides between those and is the reason both
/// arrive here rather than only the first.
///
/// Such a run is unrunnable **by definition** — the tick needs the workspace
/// path to build a context and there is none, and no future poll will find one.
/// Skipping it therefore does not defer the work, it creates a permanent hot
/// loop: `find_pending_global_runs` re-selects the identical row on the very
/// next poll, on every node running a latency worker.
///
/// That loop took prod down on 2026-09-01. Two orphaned rows kept four
/// processes (`oxy-0` plus three workers) re-selecting at the 300 ms poll
/// interval around the clock; the connections that cost, on top of the other
/// resident loops, pinned `oxy_app` against Postgres' connection ceiling, and
/// the serve fleet — which shares that ceiling — started losing the race and
/// returning 500s after the full 30 s `ACQUIRE_TIMEOUT`. The user-visible
/// symptom was a homepage that intermittently hung for 30 s.
///
/// Terminal is the only correct outcome, so take it here.
async fn retire_orphaned_runs(
    db: &sea_orm::DatabaseConnection,
    ws_id: uuid::Uuid,
    pending: &[agentic_runtime::crud::StuckRun],
    reason: &'static str,
) {
    for run in pending.iter().filter(|r| r.workspace_id == ws_id) {
        // One transaction for both writes. Cancelling the queue rows is what
        // removes the run from `find_pending_global_runs`, so a terminal write
        // that then failed would strand the run non-terminal AND unselectable —
        // nothing left to retry it. `retire_run` rolls both back instead.
        match agentic_runtime::crud::retire_run(db, &run.run_id, reason).await {
            Ok(()) => tracing::warn!(
                target: "recovery",
                workspace_id = %ws_id,
                run_id = %run.run_id,
                reason,
                "latency worker: retired a pending run whose workspace cannot be resolved"
            ),
            Err(e) => tracing::warn!(
                target: "recovery",
                workspace_id = %ws_id,
                run_id = %run.run_id,
                error = %e,
                "latency worker: failed to retire an orphaned run; rolled back, \
                 retrying next tick"
            ),
        }
    }
}

/// Build the host-side registry of `TaskSpec::Custom` executors injected into
/// the global-run driver. Currently just per-workspace health eval; future
/// Custom kinds register here. See `internal-docs/agentic-runtime-integration.md`
/// ("One-shot queue work: `TaskSpec::Custom` + `CustomTaskRegistry`").
fn build_custom_task_registry(
    db: &sea_orm::DatabaseConnection,
    preagg: &PreaggCacheCtx,
) -> Arc<agentic_runtime::worker::CustomTaskRegistry> {
    use crate::server::app_function_executor::{APP_FUNCTION_KIND, AppFunctionTaskExecutor};
    use crate::server::health_eval_executor::{HEALTH_EVAL_KIND, HealthEvalTaskExecutor};
    use crate::server::preagg_executor::PreaggTaskExecutor;
    let mut reg = agentic_runtime::worker::CustomTaskRegistry::new();
    reg.register(
        HEALTH_EVAL_KIND,
        Arc::new(HealthEvalTaskExecutor { db: db.clone() }),
    );
    reg.register(
        APP_FUNCTION_KIND,
        // A scheduled Oxy Function's `ctx.semantic` resolves rollups on the node
        // draining the queue, the same way its HTTP-invoked twin does.
        Arc::new(AppFunctionTaskExecutor {
            db: db.clone(),
            preagg: preagg.clone(),
        }),
    );
    reg.register(
        crate::server::simulation::SIMULATION_RUN_KIND,
        Arc::new(crate::server::simulation::SimulationTaskExecutor { db: db.clone() }),
    );
    // Same shape as health eval: one executor instance, workspace context
    // rebuilt fresh per task from `workspace_id` in the payload. See
    // `preagg_executor`'s module doc for why this replaced a single
    // startup-bound worker.
    reg.register(
        "preagg_cycle",
        Arc::new(PreaggTaskExecutor { db: db.clone() }),
    );
    Arc::new(reg)
}

/// Opt-in: make the `ide` singleton hand airway runs to the worker fleet
/// instead of driving them itself. See [`excluded_source_types`].
///
/// Default **off**, and that default is the safety property, not laziness. An
/// `ide` + `serve` deployment with no worker replicas has no other driver for
/// a `Global` airway run, so switching this on there leaves pipelines sitting
/// `queued` forever. Off by default means such a deployment behaves exactly as
/// it does today; an operator who has a worker fleet turns it on and gets the
/// placement they deployed the fleet for.
pub(super) const IDE_DEFER_AIRWAY_ENV: &str = "OXY_IDE_DEFER_AIRWAY";

fn ide_defers_airway() -> bool {
    std::env::var(IDE_DEFER_AIRWAY_ENV)
        .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

/// The `source_type`s **this** process must decline at selection, for
/// `recover_pending_global_runs`' `exclude_source_types`.
///
/// One function so the cheap probe-level skip in [`tick_cloud`] and the real
/// gate inside `recover_pending_global_runs` cannot disagree. They filter at
/// different layers on purpose (see the comment at the probe), but they must
/// filter on the *same set*: a probe that skips less than the gate merely
/// wastes a workspace visit, while a probe that skips **more** hides work the
/// gate would have driven, and nothing fails — it just silently stops running.
///
/// The two exclusions are mirror images of each other:
///
/// - **`compile`** is declined by nodes that *cannot* run it. Compiles need a
///   workspace working copy, which since #2822 is a property of the ROLE, not
///   of what happens to be on disk.
/// - **`airway`** is declined by the one node that *can*. Not a capability
///   gate — a placement one. Airway submit routes are `IdeOnly`, so every
///   interactive pipeline is enqueued by the IDE singleton, and its own
///   latency worker polls the same queue on the same 300 ms tick as the
///   fleet's. Left to race, the pod that just accepted the submit often wins
///   and a memory-heavy pipeline executes in the pod least able to afford it —
///   which is the whole thing Phase 2 exists to stop. Declining is what makes
///   `TaskScope::Global` actually mean "somebody else runs this".
///
/// `Role::All` is deliberately excluded from the airway rule: that is the
/// single-process deployment, where the ide *is* the fleet and deferring would
/// strand the run with nobody to pick it up.
///
/// Returned as a `&'static [&'static str]` rather than a `Vec` so the callers
/// stay allocation-free on a path that runs every 300 ms per workspace.
///
/// **Only the latency-worker path is gated, and that is deliberate — it is
/// what keeps the airway rule from being able to strand a pipeline.**
///
/// `recover_stranded_runs` (the 30 s periodic tick) is NOT gated.
/// `find_stuck_runs` excludes a run only for a `claimed` queue row or a
/// `queued` one with `scope_owned = true`; a `queued` **Global** row is
/// neither, so an airway run that no worker has claimed within
/// `STRANDED_GRACE_SECS` becomes drivable by any eligible node — including the
/// ide that just declined it.
///
/// That is not the gate leaking. The two paths cannot fight over a normal
/// submit, because a worker's latency loop claims within ~300 ms and both the
/// claim and the driver lease then exclude the run. What is left is exactly
/// the case worth a fallback: *nobody took this for thirty seconds*. So
/// `OXY_IDE_DEFER_AIRWAY=1` on a deployment whose worker fleet is missing or
/// wedged degrades to slower placement, not a stall. Gating recovery too would
/// convert a self-healing misconfiguration into permanently queued pipelines.
///
/// Pinned by `an_unclaimed_global_airway_run_falls_back_to_the_periodic_tick`
/// in `agentic-runtime`'s integration suite, which exists because this reads
/// exactly backwards from how it behaves.
fn excluded_source_types() -> &'static [&'static str] {
    exclusions_for(
        crate::server::role_manifest::current_process_role(),
        ide_defers_airway(),
    )
}

/// The decision behind [`excluded_source_types`], as a pure function.
///
/// Split out because `PROCESS_ROLE` is a `OnceLock` — a process has exactly one
/// role for its whole life, so a test cannot exercise the other three through
/// the real reader. Same reason `agentic_pipeline::recovery::may_drive` is
/// public: the test drives the predicate production uses instead of a copy that
/// keeps passing after the call site stops applying it.
pub(super) fn exclusions_for(
    role: crate::server::role_manifest::Role,
    defer_airway: bool,
) -> &'static [&'static str] {
    use crate::server::role_manifest::Role;
    use agentic_runtime::coordinator::{AIRWAY_SOURCE_TYPE, COMPILE_SOURCE_TYPE};
    const NONE: &[&str] = &[];
    const COMPILE_ONLY: &[&str] = &[COMPILE_SOURCE_TYPE];
    const AIRWAY_ONLY: &[&str] = &[AIRWAY_SOURCE_TYPE];

    match role {
        // Matched on `Ide` specifically, NOT `process_can_compile()`, which is
        // also true for `All`. `All` is the single-process deployment where the
        // ide IS the fleet, so deferring there strands the run.
        Role::Ide if defer_airway => AIRWAY_ONLY,
        Role::Ide | Role::All => NONE,
        // `Serve` reaches here only if something turned its driver on
        // explicitly (`role_runs_inprocess_workers` is false for it); the
        // compile exclusion is right for it either way, since it owns no
        // working copy.
        Role::Worker | Role::Serve => COMPILE_ONLY,
    }
}

/// `recover_pending_global_runs` with the matching workspace filter.
#[allow(clippy::too_many_arguments)]
async fn drive_pending(
    db: &sea_orm::DatabaseConnection,
    runtime: &Arc<RuntimeState>,
    ctx: Arc<OxyProjectContext>,
    schema_cache: Option<
        &Arc<
            std::sync::Mutex<
                std::collections::HashMap<String, agentic_pipeline::AnalyticsSchemaCatalog>,
            >,
        >,
    >,
    builder_test_runner: Option<&Arc<dyn BuilderTestRunnerTrait>>,
    builder_app_runner: Option<&Arc<dyn BuilderAppRunnerTrait>>,
    router: &Arc<dyn agentic_runtime::router::TaskRouter>,
    workspace_id: Option<uuid::Uuid>,
    preagg: &PreaggCacheCtx,
) -> usize {
    let platform: Arc<dyn PlatformContext> = ctx.clone();
    let bridges: Option<BuilderBridges> = Some(build_builder_bridges(ctx));
    // The latency worker is where freshly-seeded Global runs (incl. per-workspace
    // `health_eval_workspace` Custom tasks) are drained, so inject the host's
    // Custom-kind executors here. Cheap to build per call (a few Arc clones).
    let custom_executors = Some(build_custom_task_registry(db, preagg));
    // The real gate: checked inside `recover_pending_global_runs`, immediately
    // before `try_acquire_driver`, so a declined run keeps `driver_id IS NULL`
    // and stays selectable by a node that can take it. See
    // `excluded_source_types` for what is excluded where, and why.
    let exclude = excluded_source_types();
    agentic_pipeline::recovery::recover_pending_global_runs(
        db.clone(),
        runtime.clone(),
        platform,
        bridges,
        schema_cache.cloned(),
        builder_test_runner.cloned(),
        builder_app_runner.cloned(),
        router.clone(),
        workspace_id,
        custom_executors,
        exclude,
    )
    .await
}

async fn build_local_project_ctx(
    cwd: &std::path::Path,
    db: &DatabaseConnection,
    preagg: &PreaggCacheCtx,
) -> Option<Arc<OxyProjectContext>> {
    let mut builder = match WorkspaceBuilder::new(LOCAL_WORKSPACE_ID)
        .with_working_copy(cwd, None, oxy::config::OnMissing::Empty)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "recovery",
                cwd = %cwd.display(),
                error = %e,
                "local recovery: failed to resolve workspace path"
            );
            return None;
        }
    };
    // Attach the same DB-first (env-fallback) secrets manager the request
    // middleware builds — otherwise a resumed run can't resolve workspace
    // secrets stored in the DB (e.g. a ClickHouse `password_var`, a Toast
    // `TOAST_CLIENT_SECRET`) and only sees process env vars. This is the
    // difference between the resume path and a direct run.
    builder = with_db_secrets_manager(builder, LOCAL_WORKSPACE_ID);
    let wm = match builder.build().await {
        Ok(wm) => wm,
        Err(e) => {
            tracing::warn!(
                target: "recovery",
                cwd = %cwd.display(),
                error = %e,
                "local recovery: failed to build WorkspaceManager"
            );
            return None;
        }
    };
    // Wire the DB handle exactly like the HTTP workspace middleware does.
    // The in-process global driver executes `TaskSpec::Compile` through this
    // context, and `compile_dispatcher()` (like the anomaly tools) is `None`
    // unless `db` is set — without it every compile fails with
    // "compile_dispatcher() returned None". See OxyProjectContext::with_db.
    Some(Arc::new(
        OxyProjectContext::new(wm)
            .with_db(Arc::new(db.clone()))
            .with_preagg(preagg),
    ))
}

/// Attach the DB-backed (env-fallback) secrets manager to a workspace
/// builder, mirroring the request middleware. Best-effort: a failure logs
/// and leaves the builder's default manager, matching middleware behavior.
fn with_db_secrets_manager(
    builder: WorkspaceBuilder<WorkingCopy>,
    workspace_id: uuid::Uuid,
) -> WorkspaceBuilder<WorkingCopy> {
    match SecretsManager::from_database_with_env_fallback(SecretManagerService::new(workspace_id)) {
        Ok(secrets_manager) => builder.with_secrets_manager(secrets_manager),
        Err(e) => {
            tracing::warn!(
                target: "recovery",
                %workspace_id,
                error = %e,
                "recovery: failed to create DB secrets manager; resumed run may not resolve DB secrets"
            );
            builder
        }
    }
}

/// Build a workspace-scoped `OxyProjectContext` for a background sweep given
/// only a `workspace_id` (resolving its path from the `workspaces` table).
/// Returns `None` when the workspace has no path (e.g. a local-mode sentinel) —
/// the caller degrades gracefully. Cloud path only; reuses
/// [`build_cloud_project_ctx`]. Not cached here — callers that touch many
/// checks should build once per workspace and reuse.
pub(crate) async fn build_workspace_ctx(
    workspace_id: uuid::Uuid,
    db: &DatabaseConnection,
) -> Option<Arc<OxyProjectContext>> {
    let path = match entity::workspaces::Entity::find_by_id(workspace_id)
        .one(db)
        .await
    {
        Ok(Some(ws)) => ws.path?,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(target: "recovery", %workspace_id, error = %e,
                "build_workspace_ctx: workspace lookup failed");
            return None;
        }
    };
    // No preagg context: this builder serves the workspace-health smoke probes
    // and the reconciliation check, both of which exist to test the WAREHOUSE.
    // Reconciliation compares an Oxy measure against a live external source, so
    // answering the Oxy side from a rollup would report drift that is really
    // rollup lag — the check would be measuring its own cache.
    let ctx =
        build_cloud_project_ctx_inner(workspace_id, &path, db, &PreaggCacheCtx::default()).await?;

    // Resolve the semantic scan root from the PROMOTED REVISION, and only from
    // it. Two failures, one line:
    //
    // 1. The eval runs as a queued task on the global-run fleet, which holds a
    //    workspace's ROW and not its DIRECTORY. Reading `working_copy().root()`
    //    there failed with `failed to load semantic model: failed to read
    //    <workspace path>: No such file or directory` — an alert about the
    //    node, worded as an alert about the workspace, on a workspace whose
    //    every other surface was querying that same model fine.
    // 2. On a node that DOES hold a working copy — a single instance, or the
    //    factory doubling as the global-run node — that copy holds whatever
    //    someone is editing in the IDE right now. Health judges the workspace,
    //    so it must judge what is promoted: an uncommitted `.view.yml` must
    //    never page the on-call about a revision that is fine.
    //
    // Hence `resolve_compiled_scan_source`, not `resolve_query_scan_source` —
    // the serving resolver falls back to the working copy by design, which is
    // right for every reader that is not this one.
    //
    // The scan is resolved ONCE and held for the life of the context: a
    // materialised scan is a tempdir, and every probe in the pass reads it.
    let ctx = ctx.requiring_compiled_semantic_scan();
    let ctx =
        match crate::server::api::semantic::resolve_compiled_scan_source(ctx.workspace_manager())
            .await
        {
            Ok(scan) => ctx.with_semantic_scan(scan),
            Err(unavailable) => {
                // Not fatal: the connection and app probes don't need a layer, and
                // the workspace is still evaluated on everything else. A compile is
                // enqueued, and the semantic checks report that the model is not
                // compiled rather than blaming the YAML — or, worse, judging
                // someone's draft.
                tracing::warn!(
                    target: "health_eval",
                    %workspace_id,
                    reason = %unavailable.message(),
                    "no compiled semantic model for the health eval; semantic checks will \
                     report unavailable until a compile lands"
                );
                ctx
            }
        };
    Some(Arc::new(ctx))
}

/// Deserialize a compiled `config.yml` and stamp the workspace path onto it.
///
/// `workspace_path` is `#[serde(skip)]` on `Config`, so a config that came from
/// Postgres carries none — every downstream file resolver (`resolve_file`, the
/// DuckDB dataset dir, a BigQuery `key_path`) would resolve against an empty
/// path. The request middleware stamps it for exactly this reason; so do we.
///
async fn build_cloud_project_ctx(
    workspace_id: uuid::Uuid,
    path: &str,
    db: &DatabaseConnection,
    preagg: &PreaggCacheCtx,
) -> Option<Arc<OxyProjectContext>> {
    build_cloud_project_ctx_inner(workspace_id, path, db, preagg)
        .await
        .map(Arc::new)
}

/// [`build_cloud_project_ctx`] before the `Arc`, so a caller that has more to
/// attach (`build_workspace_ctx` resolves a semantic scan root) can do it
/// without unwrapping one.
async fn build_cloud_project_ctx_inner(
    workspace_id: uuid::Uuid,
    path: &str,
    db: &DatabaseConnection,
    preagg: &PreaggCacheCtx,
) -> Option<OxyProjectContext> {
    // Resolve the config the same way the request middleware does: the promoted
    // compiled revision first, the working copy only on a miss.
    //
    // This is not just a cache — the two sources are not the same config. The
    // compile worker *injects* fields the on-disk `config.yml` never has, most
    // importantly the DuckDB `s3_mirror` block (`oxy-compile::duckdb_mirror`).
    // A context built from the FS therefore builds a `Local` DuckDB connector
    // pointed at a working copy that a stateless replica doesn't have, while
    // every request path — reading the compiled config — builds the S3-mirror
    // connector and succeeds. Background work (run recovery, reconciliation, the
    // health smoke probes) must see the same databases, with the same shape, as
    // the queries it is meant to be checking; otherwise the smoke test reports a
    // dead connection for a warehouse the product is happily querying.
    //
    // Branch is `None` (the promoted revision) to match the workspace root path
    // this context is built against — the same pairing `resolve_smoke_settings`
    // already uses.
    let compiled_config =
        crate::server::api::compiled_reader::resolve_request_revision(workspace_id, None).await;

    if compiled_config.is_none() && !crate::server::role_manifest::process_can_compile() {
        tracing::warn!(
            target: "recovery",
            %workspace_id,
            "no compiled config and no working copy on this node; enqueuing a compile"
        );
        crate::server::api::middlewares::workspace_context::enqueue_lazy_compile(db, workspace_id)
            .await;
        return None;
    }

    let builder_init = WorkspaceBuilder::new(workspace_id)
        .with_working_copy(path, compiled_config, oxy::config::OnMissing::Empty)
        .await;
    let mut builder = match builder_init {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "recovery",
                %workspace_id,
                error = %e,
                "cloud recovery: failed to resolve workspace path, skipping"
            );
            return None;
        }
    };
    // Same DB-first secrets manager as the request middleware, so a
    // resumed run resolves this workspace's DB-stored secrets (not just
    // process env). See `build_local_project_ctx`.
    builder = with_db_secrets_manager(builder, workspace_id);
    let wm = match builder.build().await {
        Ok(wm) => wm,
        Err(e) => {
            tracing::warn!(
                target: "recovery",
                %workspace_id,
                error = %e,
                "cloud recovery: failed to build WorkspaceManager, skipping"
            );
            return None;
        }
    };
    // Wire the DB handle exactly like the HTTP workspace middleware does.
    // The in-process global driver executes `TaskSpec::Compile` through this
    // context, and `compile_dispatcher()` (like the anomaly tools) is `None`
    // unless `db` is set — without it every compile fails with
    // "compile_dispatcher() returned None". See OxyProjectContext::with_db.
    Some(
        OxyProjectContext::new(wm)
            .with_db(Arc::new(db.clone()))
            .with_preagg(preagg),
    )
}

/// Reconcile per-workspace `health_eval` schedule rows on startup. Removes the
/// legacy cross-tenant singleton row (`target_ref = 'global'`, the old 10-minute
/// sweep) if present, then gives every workspace with a *readable promoted
/// config* a row whose cadence comes from that config's `health_check` (1h when
/// the block sets no interval; **disabled** when there is no block — health
/// checks are opt-in).
///
/// Workspaces without one are skipped entirely rather than written as disabled:
/// neither "no promoted revision yet" nor a failed read states intent, and this
/// loops over *every* workspace, so a bad read here can't switch off a tenant
/// that opted in. A skipped workspace may therefore have no row at all until its
/// first promoted compile, which reconciles through the same upsert
/// (`compile_worker::health_reconcile_target`) — and no row behaves exactly like
/// a disabled one. Idempotent and best-effort: a per-workspace failure is logged
/// and skipped. Replaces the old `bootstrap_health_schedule`.
///
/// `prune_orphans` gates the destructive orphan sweep to Cloud mode: there the
/// `workspaces` table is the authoritative set, so a row pointing outside it is
/// genuinely orphaned (and an empty table genuinely means zero workspaces). In
/// Local mode the single workspace has no `workspaces` row, so the table is empty
/// and pruning would wrongly delete the local schedules — hence `false` there.
async fn reconcile_all_health_schedules(db: &DatabaseConnection, prune_orphans: bool) {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    // Drop the legacy global singleton row — superseded by per-workspace rows.
    if let Err(e) = db
        .execute_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "DELETE FROM agentic_schedules \
             WHERE target_kind = 'health_eval' AND target_ref = 'global'",
        ))
        .await
    {
        tracing::warn!(
            target: "health_eval",
            error = %e,
            "startup reconcile: failed to delete legacy global health row"
        );
    }

    let workspaces = match entity::workspaces::Entity::find().all(db).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!(
                target: "health_eval",
                error = %e,
                "startup reconcile: failed to list workspaces; skipping health backfill"
            );
            return;
        }
    };

    // Prune orphaned schedules: workspaces carry no FK to `agentic_schedules`,
    // so a deleted workspace leaves its rows behind — a `health_eval` (or
    // `monitor_scan`) row keeps enqueuing tasks for a workspace that no longer
    // exists, which the worker fails and eventually dead-letters, piling up.
    // Delete every schedule whose workspace is gone. (New deletes clean up inline
    // via `cleanup_workspace_schedules`; this drains rows orphaned before that.)
    if prune_orphans {
        let live_ids: Vec<uuid::Uuid> = workspaces.iter().map(|w| w.id).collect();
        prune_orphaned_schedules(db, &live_ids).await;
    }

    for ws in workspaces {
        // Reads the workspace's compiled config (FS fallback on miss) and
        // reconciles its row to the configured cadence — never clobbers a
        // config-set cadence with a default.
        crate::server::compile_worker::reconcile_health_from_compiled(db, ws.id).await;
    }
}

/// One-time startup backfill of every workspace's `preagg_cycle` schedule row
/// from compiled config. Mirrors [`reconcile_all_health_schedules`] minus the
/// orphan sweep and legacy-row cleanup — both are already generic across every
/// `target_kind` and run once from the health call in the same startup pass, so
/// running them twice would be redundant, not incorrect, and this skips that.
/// Steady-state sync is event-driven at compile time
/// (`compile_worker::reconcile_preagg_from_compiled`) — never per tick.
async fn reconcile_all_preagg_schedules(db: &DatabaseConnection) {
    let workspaces = match entity::workspaces::Entity::find().all(db).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!(
                target: "preagg",
                error = %e,
                "startup reconcile: failed to list workspaces; skipping preagg backfill"
            );
            return;
        }
    };
    for ws in workspaces {
        crate::server::compile_worker::reconcile_preagg_from_compiled(db, ws.id).await;
    }
}

/// Delete every schedule row (any `target_kind`) whose workspace no longer
/// exists. Given the full set of live workspace ids, removes each row pointing
/// outside it — the orphans left behind by workspace deletion (schedules have no
/// FK, so the DB won't cascade). An empty `live_ids` means no workspaces exist,
/// so every row is an orphan. **Cloud-only** (see `prune_orphans` on the caller):
/// in Local mode the single workspace has no `workspaces` row, so an empty table
/// there is not evidence of orphans. Best-effort; a failure is logged and skipped.
async fn prune_orphaned_schedules(db: &DatabaseConnection, live_ids: &[uuid::Uuid]) {
    use agentic_runtime::entity::schedule;
    use sea_orm::{ColumnTrait, QueryFilter};

    let mut q = schedule::Entity::delete_many();
    if !live_ids.is_empty() {
        q = q.filter(schedule::Column::WorkspaceId.is_not_in(live_ids.iter().copied()));
    }
    match q.exec(db).await {
        Ok(res) if res.rows_affected > 0 => tracing::info!(
            target: "health_eval",
            removed = res.rows_affected,
            "startup reconcile: pruned orphaned schedules for deleted workspaces"
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            target: "health_eval",
            error = %e,
            "startup reconcile: failed to prune orphaned schedules"
        ),
    }
}

/// The timezone a workspace's `monitor_scan` schedule rows should fire in.
///
/// Only the FILE-level `.monitor.yml` timezone can apply: one schedule row
/// drives every monitor at a given granularity, so a per-monitor override has
/// nothing to attach to. Absent = UTC, preserving existing behavior.
fn desired_schedule_timezone(cfg: &oxy_metric_monitoring::MonitorConfig) -> String {
    cfg.timezone.clone().unwrap_or_else(|| "UTC".to_string())
}

/// Build the `ActiveModel` for a timezone reconcile: sets only `id` (primary
/// key — never emitted in the `SET` clause), `timezone` and `next_run_at`,
/// leaving every other column `NotSet`.
///
/// `agentic_schedules` has exactly one true writer today —
/// `tick_monitor_schedules`'s CAS `UPDATE ... WHERE id = $2 AND
/// next_run_at = $3` — and several replicas run the tick loop concurrently
/// with no leader election precisely because that CAS makes it safe
/// (`recovery.rs` docs above). Building this `ActiveModel` from
/// `row.clone().into()` would set every column from a snapshot taken
/// earlier in the boot/tick pass and emit a full-row, unguarded `UPDATE`,
/// making this the first non-CAS writer on the table: a concurrent CAS fire
/// on the same row between the snapshot read and this write would have its
/// bookkeeping (`last_fired_at`, `last_run_id`, `missed_runs`,
/// `last_missed_at`) silently reverted. A targeted two-column update can
/// never race the CAS fire on any column it doesn't touch.
fn timezone_reconcile_active_model(
    row: &agentic_runtime::entity::schedule::Model,
    timezone: &str,
    next: chrono::DateTime<chrono::FixedOffset>,
) -> agentic_runtime::entity::schedule::ActiveModel {
    use agentic_runtime::entity::schedule;

    schedule::ActiveModel {
        id: sea_orm::ActiveValue::Set(row.id.clone()),
        timezone: sea_orm::ActiveValue::Set(timezone.to_string()),
        next_run_at: sea_orm::ActiveValue::Set(next),
        ..Default::default()
    }
}

/// Update the `timezone` (and recompute `next_run_at`) on any existing
/// `monitor_scan` row whose timezone differs from `desired`.
///
/// Bootstrap is create-only, so without this reconcile every existing row
/// would be stranded on its original timezone and a `timezone:` edit in
/// `.monitor.yml` would silently never take effect. Cadence, the enabled
/// flag and variables are left exactly as they are — see
/// [`timezone_reconcile_active_model`] for why the update must stay
/// column-targeted rather than a full-row write. A row whose next fire
/// time can't be recomputed for the new timezone is logged and left alone,
/// never half-updated.
async fn reconcile_monitor_schedule_timezones(
    db: &DatabaseConnection,
    workspace_id: uuid::Uuid,
    existing: &[agentic_runtime::entity::schedule::Model],
    timezone: &str,
) {
    for row in existing.iter().filter(|r| r.timezone != timezone) {
        let next = match agentic_pipeline::scheduler::next_after(
            &row.cron_expr,
            timezone,
            chrono::Utc::now(),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    target: "metric_monitoring",
                    %workspace_id,
                    schedule_id = %row.id,
                    error = %e,
                    "reconcile: could not recompute next_run_at for the new timezone; leaving the row alone"
                );
                continue;
            }
        };
        let active = timezone_reconcile_active_model(row, timezone, next);
        match sea_orm::ActiveModelTrait::update(active, db).await {
            Ok(_) => tracing::info!(
                target: "metric_monitoring",
                %workspace_id,
                schedule_id = %row.id,
                from = %row.timezone,
                to = %timezone,
                "reconciled monitor_scan schedule timezone"
            ),
            Err(e) => tracing::warn!(
                target: "metric_monitoring",
                %workspace_id,
                schedule_id = %row.id,
                error = %e,
                "reconcile: failed to update monitor_scan schedule timezone"
            ),
        }
    }
}

/// Read `.monitor.yml`'s `schedule:` block and create `monitor_scan` schedule
/// rows for any granularity not yet present in `agentic_schedules`. Also
/// reconciles the `timezone` column on rows already present — see
/// [`reconcile_monitor_schedule_timezones`]. Otherwise create-only: never
/// touches cadence, enabled flag, or variables on existing rows, and never
/// deletes rows.
async fn bootstrap_monitor_schedules(
    db: &DatabaseConnection,
    workspace_id: uuid::Uuid,
    workspace_root: &std::path::Path,
) {
    use agentic_pipeline::scheduler::{ScheduleInput, create_schedule};
    use agentic_runtime::entity::schedule;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let config_path = oxy_metric_monitoring::default_config_path(workspace_root);
    let cfg = match oxy_metric_monitoring::load_from_file(&config_path) {
        Ok(c) => c,
        // "No monitors here" and "this node holds no working copy" are the two
        // ORDINARY answers, not failures. Every periodic pass visits every
        // workspace on every node, so warning on them emitted ~70 lines/min per
        // worker in prod (2026-09-01) — noise that buried the real signal while
        // the fleet was actually failing. A malformed or unreadable file is a
        // genuine problem and stays loud.
        // `WorkspaceAbsent` only — `load_from_file` returns `Ok(default)` for a
        // merely missing file, so `NotFound` is unconstructible there and an arm
        // for it would read as coverage it does not provide.
        Err(e @ oxy_metric_monitoring::ConfigLoadError::WorkspaceAbsent(_)) => {
            tracing::debug!(
                target: "metric_monitoring",
                %workspace_id,
                error = %e,
                "bootstrap: no readable .monitor.yml on this node; skipping"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                target: "metric_monitoring",
                %workspace_id,
                error = %e,
                "bootstrap: failed to read .monitor.yml; skipping"
            );
            return;
        }
    };
    let timezone = desired_schedule_timezone(&cfg);
    let Some(sched) = cfg.schedule.clone() else {
        return;
    };

    // Fetch existing monitor_scan rows once to avoid N+1 queries.
    let existing = match schedule::Entity::find()
        .filter(schedule::Column::WorkspaceId.eq(workspace_id))
        .filter(schedule::Column::TargetKind.eq("monitor_scan"))
        .all(db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "metric_monitoring",
                %workspace_id,
                error = %e,
                "bootstrap: failed to query existing monitor schedules"
            );
            return;
        }
    };

    let has_granularity = |gran: &str| {
        existing.iter().any(|s| {
            s.variables
                .as_ref()
                .and_then(|v| v.get("granularity"))
                .and_then(|g| g.as_str())
                == Some(gran)
        })
    };

    reconcile_monitor_schedule_timezones(db, workspace_id, &existing, &timezone).await;

    let entries = [
        (sched.daily.as_deref(), "day", "Metric monitoring (daily)"),
        (
            sched.weekly.as_deref(),
            "week",
            "Metric monitoring (weekly)",
        ),
        (
            sched.monthly.as_deref(),
            "month",
            "Metric monitoring (monthly)",
        ),
    ];

    for (maybe_cron, gran, name) in entries {
        let Some(cron_expr) = maybe_cron else {
            continue;
        };
        if has_granularity(gran) {
            continue;
        }
        let input = ScheduleInput {
            name: name.to_string(),
            target_kind: "monitor_scan".to_string(),
            target_ref: ".monitor.yml".to_string(),
            question: None,
            variables: Some(serde_json::json!({ "granularity": gran })),
            cron_expr: cron_expr.to_string(),
            timezone: timezone.clone(),
            enabled: true,
        };
        match create_schedule(db, workspace_id, input).await {
            Ok(_) => tracing::info!(
                target: "metric_monitoring",
                %workspace_id,
                granularity = %gran,
                "bootstrapped monitor_scan schedule"
            ),
            Err(e) => tracing::warn!(
                target: "metric_monitoring",
                %workspace_id,
                granularity = %gran,
                error = %e,
                "bootstrap: failed to create monitor_scan schedule"
            ),
        }
    }
}

#[cfg(test)]
mod exclusion_tests {
    use super::exclusions_for;
    use crate::server::role_manifest::Role;
    use agentic_runtime::coordinator::{AIRWAY_SOURCE_TYPE, COMPILE_SOURCE_TYPE};

    /// The placement rule Phase 2 exists for: with the flag on, the ide hands
    /// airway to the fleet. Without this the `TaskScope::Global` flip is a
    /// coin flip on poll phase — the ide's own latency worker polls the same
    /// queue on the same tick as the fleet's and frequently wins, so the
    /// pipeline keeps executing in the pod that accepted the submit.
    #[test]
    fn the_ide_defers_airway_only_when_asked() {
        assert_eq!(exclusions_for(Role::Ide, true), &[AIRWAY_SOURCE_TYPE]);
        assert!(exclusions_for(Role::Ide, false).is_empty());
    }

    /// The default that keeps a fleetless deployment working. An `ide` +
    /// `serve` install with no worker replicas has no other driver for a
    /// `Global` airway run, so an ide that deferred by default would leave
    /// every pipeline `queued` forever.
    #[test]
    fn deferral_is_off_unless_the_env_var_is_set() {
        // The reader, not the pure function — pins that the default is OFF.
        // Uses whatever the ambient env is; the var is not set in CI.
        assert!(
            !super::ide_defers_airway(),
            "OXY_IDE_DEFER_AIRWAY must default off"
        );
    }

    /// `All` is the single-process deployment: the ide IS the fleet, so
    /// deferring would strand the run with nobody to pick it up. It must never
    /// defer, even with the flag on — which is why the match arm keys on
    /// `Role::Ide` and not on `process_can_compile()`, a predicate that is
    /// true for both.
    #[test]
    fn a_single_process_deployment_never_defers_airway() {
        assert!(exclusions_for(Role::All, true).is_empty());
        assert!(exclusions_for(Role::All, false).is_empty());
    }

    /// The pre-existing compile gate must survive the airway one. A worker
    /// owns no working copy, so it declines compiles whatever the airway flag
    /// says — and it must NEVER decline airway, since it is the node the
    /// pipeline is being handed to.
    #[test]
    fn a_worker_declines_compiles_and_always_accepts_airway() {
        for defer in [true, false] {
            let ex = exclusions_for(Role::Worker, defer);
            assert_eq!(ex, &[COMPILE_SOURCE_TYPE]);
            assert!(
                !ex.contains(&AIRWAY_SOURCE_TYPE),
                "the worker is the airway destination; excluding it there \
                 would leave the run queued with no eligible driver at all"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this file exists to prevent a second time.
    ///
    /// On 2026-09-01 the deleted-workspace arm was taught to retire while the
    /// path-less arm beside it kept skipping. Both are "the tick cannot get a
    /// path, and no later poll will produce one", so both are permanent hot
    /// loops — `find_pending_global_runs` re-selects an untouched row on the
    /// very next tick, forever. Asserting the arms individually would not have
    /// caught it; asserting that NO successful lookup can produce a skip does.
    #[test]
    fn every_unresolvable_workspace_retires_rather_than_skips() {
        for (label, input) in [("row deleted", None), ("row present, no path", Some(None))] {
            assert!(
                matches!(classify_workspace(input), WorkspaceOutcome::Retire(_)),
                "{label}: an unresolvable workspace must retire its runs, not skip                  them — skipping leaves the row selectable and re-polls it forever"
            );
        }
    }

    #[test]
    fn a_workspace_with_a_path_still_drives() {
        match classify_workspace(Some(Some("/workspace/oxy_data/ws".into()))) {
            WorkspaceOutcome::Drive(p) => assert_eq!(p, "/workspace/oxy_data/ws"),
            WorkspaceOutcome::Retire(r) => panic!("a drivable workspace was retired: {r}"),
        }
    }

    /// The two causes stay distinguishable in the retirement reason, which is
    /// what a future reader of `agentic_runs.error_message` has to work from.
    #[test]
    fn retirement_reasons_name_the_actual_cause() {
        let reason = |i| match classify_workspace(i) {
            WorkspaceOutcome::Retire(r) => r,
            WorkspaceOutcome::Drive(_) => panic!("expected a retirement"),
        };
        assert_eq!(reason(None), "workspace no longer exists");
        assert_eq!(reason(Some(None)), "workspace has no path");
    }

    #[test]
    fn schedule_timezone_comes_from_the_monitor_config() {
        let cfg: oxy_metric_monitoring::MonitorConfig = serde_yaml::from_str(
            "timezone: America/Los_Angeles\nmonitors:\n  - measure: a.b\n    time_dimension: a.t\n",
        )
        .unwrap();
        assert_eq!(desired_schedule_timezone(&cfg), "America/Los_Angeles");
    }

    #[test]
    fn schedule_timezone_defaults_to_utc() {
        let cfg: oxy_metric_monitoring::MonitorConfig =
            serde_yaml::from_str("monitors:\n  - measure: a.b\n    time_dimension: a.t\n").unwrap();
        assert_eq!(
            desired_schedule_timezone(&cfg),
            "UTC",
            "an existing config with no timezone must keep firing on the UTC cron"
        );
    }

    #[test]
    fn per_monitor_overrides_do_not_change_the_schedule_timezone() {
        // One schedule row drives every monitor at a granularity, so only the
        // file-level timezone can determine when it fires.
        let cfg: oxy_metric_monitoring::MonitorConfig = serde_yaml::from_str(
            "timezone: America/Los_Angeles\nmonitors:\n  - measure: a.b\n    time_dimension: a.t\n    timezone: Europe/Berlin\n",
        )
        .unwrap();
        assert_eq!(desired_schedule_timezone(&cfg), "America/Los_Angeles");
    }

    /// `agentic_schedules` has exactly one true writer today — the tick's CAS
    /// `UPDATE ... WHERE id = $2 AND next_run_at = $3` — and several replicas
    /// run the tick loop concurrently with no leader election *because* that
    /// CAS makes it safe. A reconcile built from `row.clone().into()` would
    /// set every column and emit a full-row, unguarded `UPDATE`, becoming a
    /// second, non-CAS writer that can silently revert a concurrent tick's
    /// fire bookkeeping. Pin the reconcile to a true two-column update: every
    /// field except `id` (the primary key, never emitted in `SET`),
    /// `timezone` and `next_run_at` must stay `NotSet`.
    #[test]
    fn timezone_reconcile_active_model_only_touches_timezone_and_next_run_at() {
        use agentic_runtime::entity::schedule;

        let now: chrono::DateTime<chrono::FixedOffset> = chrono::Utc::now().fixed_offset();
        let row = schedule::Model {
            id: "sched-1".to_string(),
            workspace_id: uuid::Uuid::new_v4(),
            project_id: None,
            branch_id: None,
            name: "Metric monitoring (daily)".to_string(),
            target_kind: "monitor_scan".to_string(),
            target_ref: ".monitor.yml".to_string(),
            question: None,
            variables: Some(serde_json::json!({ "granularity": "day" })),
            cron_expr: "0 6 * * *".to_string(),
            timezone: "UTC".to_string(),
            enabled: true,
            next_run_at: now,
            // Bookkeeping a concurrent CAS fire on another replica could have
            // just written — must survive an unrelated timezone reconcile.
            last_fired_at: Some(now),
            last_run_id: Some("run-42".to_string()),
            last_error: None,
            missed_runs: 3,
            last_missed_at: Some(now),
            created_at: now,
            updated_at: now,
        };
        let next = now + chrono::Duration::hours(1);

        let active = timezone_reconcile_active_model(&row, "America/Los_Angeles", next);

        assert!(
            active.id.is_set(),
            "primary key must be set to target the row"
        );
        assert!(active.timezone.is_set());
        assert!(active.next_run_at.is_set());

        assert!(active.workspace_id.is_not_set());
        assert!(active.project_id.is_not_set());
        assert!(active.branch_id.is_not_set());
        assert!(active.name.is_not_set());
        assert!(active.target_kind.is_not_set());
        assert!(active.target_ref.is_not_set());
        assert!(active.question.is_not_set());
        assert!(active.variables.is_not_set());
        assert!(active.cron_expr.is_not_set());
        assert!(active.enabled.is_not_set());
        assert!(
            active.last_fired_at.is_not_set(),
            "must not revert a concurrent tick's fire bookkeeping"
        );
        assert!(active.last_run_id.is_not_set());
        assert!(active.last_error.is_not_set());
        assert!(active.missed_runs.is_not_set());
        assert!(active.last_missed_at.is_not_set());
        assert!(active.created_at.is_not_set());
        assert!(active.updated_at.is_not_set());
    }
}

#[cfg(test)]
mod health_scan_wiring_tests {
    /// Source up to this module's own test attribute — the same shape guard
    /// `api::metric_tree::handlers_never_scan_the_working_copy` uses.
    fn src() -> &'static str {
        include_str!("recovery.rs")
            .split("\n#[cfg(test)]")
            .next()
            .expect("source splits on its own test-module attribute")
    }

    /// `build_workspace_ctx` is the ONLY context builder the workspace-health
    /// eval uses. That eval judges a workspace, so it must read the promoted
    /// revision and nothing else: the fleet node it runs on may not hold the
    /// workspace directory at all, and a node that does holds whatever is being
    /// edited in the IDE.
    ///
    /// Guarded as source shape because the regression is an ABSENCE: dropping
    /// either call compiles perfectly and silently restores the working-copy
    /// fallback, which is only wrong in cloud. Poke House Staging paged for 58
    /// days on exactly that. `resolve_query_scan_source` is named here as the
    /// wrong door — it falls back to the working copy by design.
    #[test]
    fn the_health_context_reads_the_promoted_revision_and_nothing_else() {
        let s = src();
        let body = s
            .split("pub(crate) async fn build_workspace_ctx")
            .nth(1)
            .expect("build_workspace_ctx is defined here");
        assert!(
            body.contains("resolve_compiled_scan_source") && body.contains("with_semantic_scan"),
            "build_workspace_ctx must hand its context a compile-boundary scan root; \
             without one the health eval parses the semantic model out of a working \
             copy the fleet does not have"
        );
        assert!(
            body.contains("requiring_compiled_semantic_scan"),
            "and it must REFUSE the working-copy fallback: on a node that does hold the \
             files, falling back judges someone's uncommitted IDE edit and pages the \
             on-call about a promoted revision that is fine"
        );
    }
}
