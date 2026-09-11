//! Prometheus exposition for the `oxy worker` process.
//!
//! Single endpoint, `GET /metrics`, mounted on the same tiny health
//! server that already exposes `/healthz` and `/readyz`. The format
//! is the documented Prometheus text exposition; we hand-roll it to
//! avoid pulling in a heavyweight client crate for a set this small.
//!
//! What gets exposed:
//!
//! | Metric | Type | Labels | Source |
//! |---|---|---|---|
//! | `oxy_queue_depth_queued` | gauge | `task_kind` | `agentic_task_queue` GROUP BY |
//! | `oxy_queue_depth_claimed` | gauge | `task_kind` | same |
//! | `oxy_queue_depth_dead` | gauge | `task_kind` | same |
//! | `oxy_worker_capacity` | gauge | `task_kind` | one global env cap, per label |
//! | `oxy_worker_info` | gauge=1 | `worker_id`, `version` | identity |
//! | `oxy_compile_stuck_compiling` | gauge | (none) | `revisions` compile-health query |
//! | `oxy_compile_promotion_lag` | gauge | (none) | `revisions` ⋈ `workspaces` |
//! | `oxy_tasks_requeued_total` | counter | (none) | `agentic_runtime::crud::TASKS_REQUEUED` |
//! | `oxy_tasks_dead_lettered_total` | counter | (none) | `agentic_runtime::crud::TASKS_DEAD_LETTERED` |
//! | `oxy_router_probes_received_total` | counter | (none) | `agentic_runtime::router::PROBES_RECEIVED` |
//! | `oxy_router_last_probe_received_timestamp_seconds` | gauge | (none) | `agentic_runtime::router::LAST_PROBE_RECEIVED_MILLIS` |
//! | `oxy_metrics_scrape_db_ok` | gauge=0/1 | (none) | this replica's DB read status this scrape |
//!
//! The queue-depth rows group by the task's kind, which `agentic_task_queue`
//! stores only inside its `spec` JSONB — that table has no `source_type`
//! column (that one is on `agentic_runs`). See [`read_queue_depth`].
//!
//! Per-process inflight counters (current concurrent tasks per
//! worker) aren't surfaced yet because the orchestrator doesn't
//! expose them out of the box; they'll land alongside `claimed_by`
//! observability (refinement E of the scaling design). Scrapers
//! should derive in-flight from `oxy_queue_depth_claimed` meanwhile
//! (`max` across replicas, then summed across `task_kind`) — with the
//! same absent-vs-zero caveat the queue-depth gauges carry below: it
//! comes off the same folded map, so an idle fleet emits no series at
//! all and a panel reads "no data" rather than 0.
//!
//! Both uses aggregate this gauge identically — `max` across replicas,
//! then `sum` across `task_kind`. There is no second rule for the HPA
//! numerator. The only per-kind constraint in that query is on the
//! DENOMINATOR: `oxy_worker_capacity` must carry exactly one `task_kind`
//! label, because summing its three is the 3x trap below.
//!
//! Aggregation splits on the table's `Source` column, not on metric
//! type, and the replica and `task_kind` axes don't always agree.
//! DB-sourced gauges are identical on every replica — take `max`;
//! summing multiplies the backlog by replica count. Process-local ones
//! (`oxy_worker_capacity`, `oxy_worker_info`, both `*_total` counters)
//! take `sum` across replicas — but capacity does *not* sum across
//! `task_kind`: `ConcurrencyCaps::from_env` emits one global pool under
//! all three labels, so adding them reports 3× the real headroom. Pick a
//! single label.
//!
//! `oxy_metrics_scrape_db_ok` belongs to NEITHER bucket, despite reading
//! the DB: `db_error` is computed from this process's own two queries,
//! so the gauge is per-replica. Aggregate with `min`. Alerting takes
//! THREE matchers for three disjoint failures — they are not
//! alternatives:
//!
//! ```text
//! up{job="oxy-worker"} == 0            one replica's scrape hung or
//!                                      failed. PRIMARY — the only
//!                                      per-target matcher. Give it a
//!                                      for: 2m, so one slow or missed
//!                                      scrape does not page; real
//!                                      starvation persists across
//!                                      several.
//! <gauge>{job="oxy-worker"} == 0       scrape returned, DB read failed.
//! absent(<gauge>{job="oxy-worker"})    TOTAL loss — every target gone
//!                                      or discovery broke, which
//!                                      up == 0 cannot catch because
//!                                      there are no up series either.
//! ```
//!
//! `job` comes from your scrape config — substitute whatever labels it.
//!
//! `absent()` alone is NOT enough: it fires only when the selector
//! matches nothing, so one starved replica among ten leaves nine series
//! and it stays quiet — the same partial-blindness defect `max` is
//! rejected for below. Always scope the selector: unscoped, a staging
//! fleet in the same Prometheus keeps the vector non-empty forever.
//! `max` is the one aggregation that must not be used — it returns 1
//! while any single replica can still reach Postgres, so the alert goes
//! quiet exactly when part of the fleet has gone blind, and this is the
//! alert called load-bearing below.
//!
//! The HPA reads outstanding work — `oxy_queue_depth_queued` PLUS
//! `oxy_queue_depth_claimed`, summed across kinds — against
//! `oxy_worker_capacity`, so each of those mistakes mis-sizes the
//! fleet: `sum` across replicas inflates the backlog N×, `max` across
//! replicas hides all but one replica's headroom, and summing capacity's
//! labels reports 3× the pool — which under-scales precisely under load.
//! NOT a per-kind ratio: capacity is ONE shared semaphore per process
//! (see `from_env`), so dividing one kind's work by it double-books the
//! pool. Sum the numerator across kinds and keep a single capacity label
//! as the pool size; the recipe is in `internal-docs/worker-fleet.md`.
//! Alert recipes for the DB-sourced gauges: "What to watch" in
//! `internal-docs/compile-boundary.md`, the operator runbook.
//!
//! Failure mode: if the DB read errors, we emit the in-process
//! metrics anyway and surface scrape health via the separate
//! `oxy_metrics_scrape_db_ok` gauge (1 = ok, 0 = the DB read failed
//! this scrape). We never fail the scrape, so the in-process counters
//! still export. Be precise about what the HPA then sees, though: the
//! queue-depth series go *absent*, which is not the same as reading
//! zero. An absent series usually leaves the HPA unable to compute the
//! metric, so it holds replicas rather than scaling down — safe, but
//! not "correct" in any stronger sense. Idle and DB-broken are both
//! absence and cannot be told apart from these series alone, which is
//! what makes alerting on `oxy_metrics_scrape_db_ok` load-bearing
//! rather than nice-to-have.

use std::sync::Arc;

use agentic_runtime::entity::task_queue;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, DatabaseBackend, DatabaseConnection, EntityTrait, FromQueryResult, QueryFilter,
    QuerySelect, Statement,
};

#[derive(Clone)]
pub struct MetricsState {
    pub worker_id: Arc<String>,
    pub version: &'static str,
    pub db: Arc<DatabaseConnection>,
    /// The in-flight cap, surfaced for HPA target sizing. ONE shared pool
    /// emitted under three `task_kind` labels — not a partition
    /// (HPA compares outstanding work — `queue_depth_queued` +
    /// `queue_depth_claimed`, all kinds — against `worker_capacity` to
    /// pick the right number of replicas).
    pub capacity: ConcurrencyCaps,
}

#[derive(Clone, Copy, Debug)]
pub struct ConcurrencyCaps {
    pub compile: u32,
    pub agent: u32,
    pub other: u32,
}

impl ConcurrencyCaps {
    /// All three caps share the single global concurrency knob
    /// (`OXY_WORKER_MAX_INFLIGHT`, default 32) — the process backs every
    /// kind with ONE semaphore, so these labels are NOT a partition; the
    /// same number is emitted under each.
    ///
    /// Which means a per-`task_kind` ratio of queue depth to capacity is
    /// not well-defined: it divides one kind's work by the whole pool and
    /// double-books it. Sum the numerator across kinds and keep a single
    /// capacity label as the pool size — see the recipe in
    /// `internal-docs/worker-fleet.md`. The labels are retained only so
    /// the gauge's shape matches its siblings.
    pub fn from_env() -> Self {
        let global = std::env::var("OXY_WORKER_MAX_INFLIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(32);
        Self {
            compile: global,
            agent: global,
            other: global,
        }
    }
}

/// Hand-rolled Prometheus exposition. Quick path that doesn't need a
/// metrics client crate; the module header table is the list of what
/// this emits.
///
/// UNBOUNDED, and that is the thing to know before changing it. Nothing
/// caps how long this runs: `worker_health::spawn` merges plain `Router`s
/// with no `TimeoutLayer`, and there is no `tokio::time::timeout` around
/// the two DB reads below — each can park on the pool waiter queue for
/// `ACQUIRE_TIMEOUT` (30s). Against a starved pool the client abandons the
/// scrape at `scrape_timeout` and the WHOLE series set vanishes rather
/// than reporting `oxy_metrics_scrape_db_ok 0`, which is what turns one
/// starved replica into a silently under-counted HPA denominator. If you
/// add the timeout, bound it against `scrape_timeout` (not the interval)
/// with headroom — the reads are only part of the request — and use ONE
/// timeout around the pair, not one each: the reads are sequential, so
/// two at `scrape_timeout / 2` costs the whole budget and loses the same
/// race. The floor discussion in `internal-docs/worker-fleet.md` has the
/// full argument.
///
/// UNBOUNDED does NOT mean shutdown can hang on it — but it is not free
/// either. `axum::serve(...).with_graceful_shutdown(...)` waits for
/// in-flight connections, so a parked scrape keeps the health task alive
/// until `drain_background` gives up on it; that join is bounded at 5s and
/// then abandoned (`cli/commands/worker.rs`). Since `release_queue_claims`
/// runs AFTER `drain_background` rather than inside it, a parked scrape
/// therefore DELAYS the release by up to that 5s — it cannot hang it. At the
/// default recovery interval this 5s used to be the ENTIRE shutdown budget,
/// because both DB-touching terms were gated on `interval < 30`. **One of
/// them no longer is.** The 30s recovery drain still is (`tick_once`
/// short-circuits at `interval >= REAPER_INTERVAL`), but the 10s
/// `release_queue_claims` is now reachable at any interval: the worker drives
/// runs, so its driver loops build a `DurableTransport` — and mint
/// `PROCESS_WORKER_ID` — the first time they pick one up, rather than only
/// inside a sub-default reaper tick. So: ~10s with no `--health-port`, ~15s
/// with it, 45s only below the default interval — budget for 45s, see the k8s
/// recipe in `internal-docs/worker-fleet.md`. The cost of a parked scrape is
/// a lost series set and 5s added to that budget, not a stuck pod.
pub async fn metrics(State(state): State<MetricsState>) -> Response {
    let mut body = String::new();

    push_identity_and_capacity(&mut body, &state);

    // Two queries, two round trips per scrape. A failure in either is
    // reported through `oxy_metrics_scrape_db_ok` rather than an error
    // status, so the in-process counters below still get exported.
    let queue_rows = read_queue_depth(&state.db).await;
    let compile_health = read_compile_health(&state.db).await;
    let db_error = queue_rows.is_err() || compile_health.is_err();
    if let Err(err) = &queue_rows {
        tracing::warn!(?err, "metrics: queue-depth read failed");
    }
    if let Err(err) = &compile_health {
        tracing::warn!(?err, "metrics: compile-health read failed");
    }

    push_queue_depth(
        &mut body,
        &fold_queue_depth(&queue_rows.unwrap_or_default()),
    );
    push_compile_health(&mut body, &compile_health.unwrap_or_default());
    push_reap_counters(&mut body);
    push_router_probe(
        &mut body,
        agentic_runtime::router::PROBES_RECEIVED.load(std::sync::atomic::Ordering::Relaxed),
        agentic_runtime::router::LAST_PROBE_RECEIVED_MILLIS
            .load(std::sync::atomic::Ordering::Relaxed),
    );

    body.push_str(
        // Names both reads: the gauge drops to 0 for a compile-health
        // failure too, and `compile-boundary.md` points operators here as
        // the watchdog for the DB-sourced gauges. No count in that phrase
        // on purpose — the gauge covers queued/claimed/dead plus
        // stuck_compiling and promotion_lag, and a number here would
        // just be a second thing to keep in sync. An operator reading
        // this string in Grafana has to be able to trust its scope.
        "# HELP oxy_metrics_scrape_db_ok Whether both metrics DB reads (queue depth, compile health) succeeded on this scrape. PER-REPLICA, not fleet-wide: aggregate with min. Alerting needs three scoped matchers, not one: up{job=...}==0 with a for: 2m for a hung scrape (primary, per-target; the for keeps one missed scrape from paging), this gauge ==0 for a live scrape whose DB read failed, and absent(oxy_metrics_scrape_db_ok{job=...}) for total loss. max stays at 1 while any one replica can still reach the DB.\n",
    );
    body.push_str("# TYPE oxy_metrics_scrape_db_ok gauge\n");
    body.push_str(&format!(
        "oxy_metrics_scrape_db_ok {}\n",
        if db_error { 0 } else { 1 }
    ));

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// Process-local gauges: who this worker is and what it is sized for.
/// Neither touches the DB, so both are exported even on a failed scrape.
fn push_identity_and_capacity(body: &mut String, state: &MetricsState) {
    body.push_str(
        "# HELP oxy_worker_info Worker identity (worker_id + version), value is always 1.\n",
    );
    body.push_str("# TYPE oxy_worker_info gauge\n");
    body.push_str(&format!(
        "oxy_worker_info{{worker_id=\"{}\",version=\"{}\"}} 1\n",
        escape_label(&state.worker_id),
        escape_label(state.version)
    ));

    body.push_str(
        "# HELP oxy_worker_capacity In-flight cap for this worker process. ONE shared pool: the same value is emitted under every task_kind, the labels are not a partition.\n",
    );
    body.push_str("# TYPE oxy_worker_capacity gauge\n");
    for (label, cap) in [
        ("compile", state.capacity.compile),
        ("agent", state.capacity.agent),
        ("other", state.capacity.other),
    ] {
        body.push_str(&format!(
            "oxy_worker_capacity{{task_kind=\"{label}\"}} {cap}\n"
        ));
    }
}

/// Emit the three queue-depth gauges from the already-folded counts.
///
/// Scrape health is reported separately via `oxy_metrics_scrape_db_ok`
/// so the data labels here stay consistent with sibling gauges and don't
/// churn series identity between scrapes — which would double the metric
/// cardinality and complicate HPA queries.
fn push_queue_depth(body: &mut String, folded: &QueueDepthByKind) {
    for (metric, status, help) in [
        (
            "oxy_queue_depth_queued",
            "queued",
            "Tasks in 'queued' status, by task kind. Scale on this PLUS claimed: queued alone goes absent when the fleet keeps up.",
        ),
        (
            "oxy_queue_depth_claimed",
            "claimed",
            "Tasks currently claimed (in flight) by some worker. The HPA numerator is this PLUS queued: queued alone goes absent when the fleet keeps up.",
        ),
        (
            "oxy_queue_depth_dead",
            "dead",
            // Deliberately says "retained": dead rows sit in the table for
            // `dead_ttl` (30d default), so this is a rolling backlog, not a
            // point-in-time state like the other two. One dead-lettered task
            // holds it above zero for a month, which makes the obvious
            // `> 0` alert permanently lit. Alert on the counter instead.
            "Dead-lettered tasks (hit max_claims) still retained in the queue table \
             — a rolling backlog over dead_ttl (30d default), not current state; \
             alert on increase(oxy_tasks_dead_lettered_total) for the event.",
        ),
    ] {
        body.push_str(&format!("# HELP {metric} {help}\n"));
        body.push_str(&format!("# TYPE {metric} gauge\n"));
        // No `escape_label` on `task_kind` — deliberate, not an oversight of
        // the split. `task_kind_label` returns `&'static str` from a closed
        // three-value set, so there is no DB-derived text left to escape;
        // the type is the guarantee that escaping used to provide at run
        // time. Keep it that way: if this label ever becomes a `String`
        // sourced from a row, escaping has to come back with it.
        for ((_, task_kind), count) in folded.iter().filter(|((s, _), _)| s.as_str() == status) {
            body.push_str(&format!("{metric}{{task_kind=\"{task_kind}\"}} {count}\n"));
        }
    }
}

/// Compile-boundary health.
///
/// Sourced from the shared `revisions` table so a single worker scrape
/// surfaces them fleet-wide. `stuck_compiling` catches crashed compiles
/// (and, since it counts long-running `compiling` rows, also flags a
/// missing/un-draining worker); `promotion_lag` catches "compiles succeed
/// but the workspace pointer didn't move" — the silent regression class.
fn push_compile_health(body: &mut String, compile: &CompileHealthRow) {
    body.push_str(
        "# HELP oxy_compile_stuck_compiling Revisions stuck in 'compiling' past the reaper threshold.\n",
    );
    body.push_str("# TYPE oxy_compile_stuck_compiling gauge\n");
    body.push_str(&format!(
        "oxy_compile_stuck_compiling {}\n",
        compile.stuck_compiling
    ));
    body.push_str(
        "# HELP oxy_compile_promotion_lag Recently-ready main revisions not promoted to current_revision_id.\n",
    );
    body.push_str("# TYPE oxy_compile_promotion_lag gauge\n");
    body.push_str(&format!(
        "oxy_compile_promotion_lag {}\n",
        compile.promotion_lag
    ));
}

/// Reap-event counters.
///
/// Monotonic, process-local counters incremented inside
/// `agentic-runtime::orchestrator::crud::queue::reap_stale_tasks` itself
/// (re-exported as `agentic_runtime::crud::TASKS_REQUEUED` /
/// `TASKS_DEAD_LETTERED`), not by the caller — that function is the
/// single choke point every reap path funnels through (the periodic
/// `background::run_reaper_cycle` loop, this worker's startup pre-pass,
/// the admin `/run-reaper` handler, and pipeline recovery), so counting
/// there is what makes every reap in this process observable regardless
/// of which path triggered it. They live in `agentic-runtime` rather
/// than here because that's where `reap_stale_tasks` lives, and
/// `agentic-runtime` must never depend on `oxy-app`. Read directly from
/// the statics rather than mirroring them into `MetricsState`; the
/// `oxy_queue_depth_dead` gauge answers "how much is still sitting
/// there" (a rolling backlog over `dead_ttl`, not current state — see
/// its HELP string), these answer "how often are we dead-lettering".
///
/// Being process-local makes these the one pair here that a single
/// scrape does *not* answer fleet-wide: only the replica that ran the
/// reap cycle increments, and a restart resets it to 0. Operator
/// queries need `sum(increase(...))` across replicas, which is why the
/// runbook recipe is written that way.
///
/// Summing still doesn't make them complete, and the gap is worth
/// knowing before trusting them as *the* dead-letter signal:
/// `background::start` runs its reaper in every `oxy serve` process
/// (`router::entry::new_agentic_state`) as well as in the worker, but
/// this endpoint is mounted only on the worker health server
/// (`worker_health`). A reap on a serve replica therefore increments a
/// counter nothing scrapes. `oxy_queue_depth_dead` reads the rows
/// themselves and so has no such blind spot — the gauge is the
/// complete-coverage signal, these are the timely ones.
fn push_reap_counters(body: &mut String) {
    body.push_str(
        "# HELP oxy_tasks_requeued_total Stale claims returned to the queue by the reaper.\n\
         # TYPE oxy_tasks_requeued_total counter\n",
    );
    body.push_str(&format!(
        "oxy_tasks_requeued_total {}\n",
        agentic_runtime::crud::TASKS_REQUEUED.load(std::sync::atomic::Ordering::Relaxed)
    ));
    body.push_str(
        "# HELP oxy_tasks_dead_lettered_total Claims moved to dead by the reaper.\n\
         # TYPE oxy_tasks_dead_lettered_total counter\n",
    );
    body.push_str(&format!(
        "oxy_tasks_dead_lettered_total {}\n",
        agentic_runtime::crud::TASKS_DEAD_LETTERED.load(std::sync::atomic::Ordering::Relaxed)
    ));
}

/// Health of the LISTEN/NOTIFY wake pipeline, as seen by this process.
///
/// The task router fires a probe on `oxy_health_probe` every 60s and
/// every listener — on this instance and on every peer — records the
/// receipt. A healthy pipeline therefore ticks continuously; a pipeline
/// where Postgres has silently stopped delivering notifications goes
/// quiet while the connection still answers `SELECT 1`. That silent
/// stall is the failure this pair exists to catch, and it is the reason
/// the signal has to be a *timestamp* rather than a rate: the question
/// is "how long since the last one", not "how many".
///
/// Both are process-local, so they carry the same replica caveat as the
/// reap counters above — and one more besides. This endpoint is mounted
/// only on the worker health server (`worker_health`), but
/// `background::start` runs a router in every `oxy serve` and `oxy ide`
/// process too. Those replicas receive probes that nothing scrapes.
/// That is acceptable rather than merely tolerated: the worker's claim
/// loop is the thing whose latency actually depends on NOTIFY delivery,
/// so the fleet that matters is the fleet that is covered. A serve-only
/// stall shows up as claim latency, not as a missing wake.
///
/// **The gauge is absent, never zero, before the first probe.**
/// `background::start` deliberately lets one full interval elapse
/// before the first emission, so a freshly started pod has legitimately
/// seen nothing. Emitting `0` there would make `time() - gauge` read as
/// ~57 years and page every rollout. Alerting therefore takes THREE
/// matchers for three disjoint failures, exactly as
/// `oxy_metrics_scrape_db_ok` above documents for its own:
///
/// ```text
/// (time() - oxy_router_last_probe_received_timestamp_seconds{job="oxy-worker"}) > 300
///     delivery has STOPPED. Three missed 60s probes, with a for: 5m.
///     Per-target, and the primary matcher.
/// oxy_router_probes_received_total{job="oxy-worker"} == 0
///     this replica has never received one, while its peers are fine.
///     Per-target, for: 15m.
/// absent(oxy_router_last_probe_received_timestamp_seconds{job="oxy-worker"})
///     TOTAL loss — every target gone, or discovery broke, which the
///     other two cannot catch because there are no series to evaluate.
/// ```
///
/// **The counter is what makes the per-replica check possible, and it is
/// not optional corroboration.** `absent()` is quiet whenever *any*
/// replica reports, so on its own it catches a never-delivering pod only
/// if the whole fleet is dead — one bad pod among healthy peers emits no
/// gauge series at all, leaving staleness with nothing to evaluate and
/// `absent()` masked by its neighbours. That is the same partial-blindness
/// defect `internal-docs/worker-fleet.md` names for
/// `oxy_metrics_scrape_db_ok`, and the reason `oxy_router_probes_received_total`
/// is exported unconditionally at `0`: a per-target `== 0` is a real
/// series to match on, where a missing gauge is not.
///
/// For capacity rather than alerting, `rate()` over the counter should sit
/// at roughly (peer count / 60) per second, and a step change means
/// instances joined or left rather than that delivery broke. Aggregate
/// with `sum` across replicas, like the other process-local counters.
///
/// This replaced a per-receipt `info` log line. See
/// `agentic_runtime::router::PROBES_RECEIVED` for that history — the
/// short version is that the line cost 67% of prod's entire oxy log
/// volume and landed in a store that carries no alerts.
///
/// Takes its two readings as arguments rather than loading the statics
/// itself, unlike [`push_reap_counters`]: the absent-vs-zero branch
/// below is the whole point of this function, and a test that has to
/// mutate a process-wide static to reach it would race every other test
/// in the same binary.
fn push_router_probe(body: &mut String, probes_received: u64, last_probe_millis: i64) {
    body.push_str(
        "# HELP oxy_router_probes_received_total Health probes received on the LISTEN/NOTIFY wake channel by this process, from any instance including itself. PROCESS-LOCAL: aggregate with sum across replicas; a restart resets it to 0.\n\
         # TYPE oxy_router_probes_received_total counter\n",
    );
    body.push_str(&format!(
        "oxy_router_probes_received_total {probes_received}\n"
    ));

    // Absent, not zero, until the first probe lands — see the doc
    // comment. A zero timestamp is 1970 and would page on every deploy.
    if last_probe_millis > 0 {
        body.push_str(
            "# HELP oxy_router_last_probe_received_timestamp_seconds UNIX time of the most recent LISTEN/NOTIFY health probe seen by this process. ABSENT until the first probe, which is one full interval after start by design — never 0, since time() - 0 is ~57 years. Alerting needs three scoped matchers, not one: (time() - this) > 300 with a for: 5m for delivery that has STOPPED (three missed 60s probes, per-target, the primary matcher); oxy_router_probes_received_total{job=...} == 0 for a replica that has never received one, which this gauge cannot express because it emits no series there and a job-scoped absent() is masked by healthy peers; and absent(oxy_router_probes_received_total{job=...}) for total loss, which neither of the others can see.\n",
        );
        body.push_str("# TYPE oxy_router_last_probe_received_timestamp_seconds gauge\n");
        body.push_str(&format!(
            "oxy_router_last_probe_received_timestamp_seconds {:.3}\n",
            last_probe_millis as f64 / 1000.0
        ));
    }
}

#[derive(FromQueryResult, Debug, Clone)]
struct QueueDepthRow {
    queue_status: String,
    /// The `type` tag of the row's serialized `TaskSpec` — `agent`,
    /// `workflow`, `workflow_step`, `workflow_decision`, `resume`,
    /// `airway`, `compile`, `custom`.
    ///
    /// `None` for any spec without a `type` key: a malformed row, or a
    /// legacy externally-tagged shape (`{"AnalyticsTurn": {…}}`) of the
    /// kind `internal_jobs::extract_task_type` still keeps a first-key
    /// fallback for. This query deliberately doesn't reproduce that
    /// fallback: recovering the key would cost a `jsonb_object_keys`
    /// lookup and change no count, because no legacy key matches an arm
    /// in `task_kind_label` — those rows fold to `other` either way.
    spec_type: Option<String>,
    count: i64,
}

impl QueueDepthRow {
    fn task_kind_label(&self) -> &'static str {
        // Two separate constraints here, with separate strengths:
        //
        //   CLOSED set (strong) — unrecognised kinds fold into `other`.
        //     Series-cardinality control (see below), plus the per-kind
        //     ALERTING queries in internal-docs/compile-boundary.md,
        //     which select `compile` by name.
        //   Closed to CAPACITY's set specifically (weak) — it only buys
        //     depth and capacity sharing one dashboard axis. The HPA does
        //     NOT join on `task_kind`: it sums the numerator across kinds
        //     and pins one label on the denominator, so an unpaired
        //     queue-depth label would add to the sum, not break anything.
        //
        // So a new label is a cardinality and alerting question first; the
        // capacity coupling alone is not grounds to refuse one. Airway and
        // automation tasks share the generic worker capacity pool and
        // therefore collapse into `other` rather than getting their own
        // label. Unrecognised spec types also fold into `other` so
        // `Custom` tasks with arbitrary `kind` strings don't explode the
        // metric series count.
        match self.spec_type.as_deref() {
            Some("compile") => "compile",
            // A `resume` task re-drives a suspended agent run; the
            // coordinator's `source_type_for_spec` stamps it `analytics`,
            // so it belongs in the same bucket as a fresh agent task.
            Some("agent") | Some("resume") => "agent",
            _ => "other",
        }
    }
}

/// Queue depth folded to the metric label space: `(queue_status, task_kind) -> count`.
type QueueDepthByKind = std::collections::BTreeMap<(String, &'static str), i64>;

/// Sum the SQL rows into the emitted label space.
///
/// Several spec types collapse into one `task_kind` (`agent` and `resume`
/// both fold to `agent`; every unrecognised type folds to `other`), so the
/// rows MUST be summed before emission. Writing them out un-summed would
/// put two samples with an identical label set in one scrape, which
/// Prometheus rejects outright ("duplicate sample for timestamp") — it
/// drops the whole scrape, not just the offending line, which would leave
/// the queue just as unmonitored as the failing query did.
///
/// `BTreeMap` also fixes the emission order, so the exposition is stable
/// scrape to scrape.
fn fold_queue_depth(rows: &[QueueDepthRow]) -> QueueDepthByKind {
    let mut folded = QueueDepthByKind::new();
    for row in rows {
        *folded
            .entry((row.queue_status.clone(), row.task_kind_label()))
            .or_insert(0) += row.count;
    }
    folded
}

async fn read_queue_depth(db: &DatabaseConnection) -> Result<Vec<QueueDepthRow>, sea_orm::DbErr> {
    // The task's kind lives ONLY inside the `spec` JSONB — there is no
    // `source_type` column on `agentic_task_queue` (that column is on
    // `agentic_runs`; selecting it here errored on every scrape and took
    // the whole queue-depth signal down with it). `TaskSpec` is an
    // internally-tagged enum (`#[serde(tag = "type")]`), so the variant
    // lands under `type` — the same key `internal_jobs::extract_task_type`
    // reads. Joining `agentic_runs` for the real `source_type` would work
    // too, but the spec is the queue row's own data and `source_type` is
    // derived from it anyway (`coordinator::source_type_for_spec`), so the
    // join buys nothing and costs a second table per scrape.
    //
    // Built through the query builder rather than a SQL string so every
    // column reference is checked against `task_queue::Column` at compile
    // time. A hand-written string is precisely what let the nonexistent
    // column ship: nothing failed until the query ran in production. Only
    // the JSON extraction stays `Expr::cust` — it names no column, so it
    // can't carry that failure mode.
    //
    // COST: the *output* is bounded (≤3 statuses × 3 folded kinds), but the
    // scan is not, and this is a new cost because the query never once ran
    // successfully. Neither partial index covers the predicate —
    // `idx_task_queue_poll` is `WHERE queue_status = 'queued'`,
    // `idx_task_queue_reap` is `WHERE queue_status = 'claimed'`, and nothing
    // covers `'dead'` — so the planner falls back to a sequential scan over
    // every retained row, including the `completed`/`failed` ones
    // `purge_old_terminal_tasks` holds for 7 and 30 days respectively. That
    // is once per scrape per worker replica. Fine at current queue volume
    // *assuming retention is enabled* — both TTLs accept `0`/`off`/`never`
    // (`RetentionConfig::from_env`), and with either disabled the scanned
    // set is unbounded and grows monotonically for the life of the
    // deployment, which is a different risk profile than "fine at current
    // volume". Either way, if the table grows this wants an index covering
    // the three statuses before it wants any other optimisation.
    let spec_type = Expr::cust("spec->>'type'");
    task_queue::Entity::find()
        .select_only()
        .column(task_queue::Column::QueueStatus)
        .expr_as(spec_type.clone(), "spec_type")
        .expr_as(Expr::cust("COUNT(*)::bigint"), "count")
        .filter(task_queue::Column::QueueStatus.is_in(["queued", "claimed", "dead"]))
        .group_by(task_queue::Column::QueueStatus)
        .group_by(spec_type)
        .into_model::<QueueDepthRow>()
        .all(db)
        .await
}

#[derive(FromQueryResult, Debug, Clone, Default)]
struct CompileHealthRow {
    stuck_compiling: i64,
    promotion_lag: i64,
}

/// One round trip for both compile-health gauges. `stuck_compiling` uses the
/// `idx_revisions_status_started` partial index; `promotion_lag` is bounded to
/// a 1-hour window so it stays a cheap point-in-time signal rather than a full
/// table scan.
///
/// Still a raw SQL string, unlike [`read_queue_depth`] — the correlated
/// subqueries don't express well in the query builder, so this one keeps five
/// hand-typed column references across `revisions` and `workspaces` that no
/// compiler checks. They are correct today; the exposure is a rename, exactly
/// the position the queue-depth query was in before it shipped a column that
/// didn't exist. Until it's rewritten, the DB-backed regression test this PR
/// leaves as follow-up must cover *both* reads, not just queue depth — this is
/// the one with no compile-time backstop, so it needs the runtime one more.
async fn read_compile_health(db: &DatabaseConnection) -> Result<CompileHealthRow, sea_orm::DbErr> {
    let sql = "\
        SELECT \
          (SELECT COUNT(*) FROM revisions \
             WHERE status = 'compiling' \
               AND started_at < now() - interval '15 minutes')::bigint AS stuck_compiling, \
          (SELECT COUNT(*) FROM revisions r \
             JOIN workspaces w ON w.id = r.workspace_id \
             WHERE r.status = 'ready' AND r.kind = 'main' \
               AND r.finished_at > now() - interval '1 hour' \
               AND w.current_revision_id IS DISTINCT FROM r.revision_id)::bigint AS promotion_lag";
    let stmt = Statement::from_string(DatabaseBackend::Postgres, sql.to_string());
    Ok(CompileHealthRow::find_by_statement(stmt)
        .one(db)
        .await?
        .unwrap_or_default())
}

/// Escape a label value per the Prometheus text format spec: `\\`, `\n`, `"`.
fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentic_core::delegation::TaskSpec;
    use agentic_core::human_input::SuspendedRunData;

    #[test]
    fn caps_from_env_share_the_global_pool() {
        // Every task kind reads the same global cap. Per-kind env vars
        // were removed in the compile-boundary simplification — workers
        // share one pool, and the per-kind labels are NOT a partition:
        // the same number is emitted under each, so a per-kind ratio of
        // queue depth to capacity double-books it. See `from_env`.
        unsafe {
            std::env::set_var("OXY_WORKER_MAX_INFLIGHT", "16");
        }
        let caps = ConcurrencyCaps::from_env();
        assert_eq!(caps.compile, 16);
        assert_eq!(caps.agent, 16);
        assert_eq!(caps.other, 16);
        unsafe {
            std::env::remove_var("OXY_WORKER_MAX_INFLIGHT");
        }
    }

    fn row(queue_status: &str, spec_type: Option<&str>, count: i64) -> QueueDepthRow {
        QueueDepthRow {
            queue_status: queue_status.into(),
            spec_type: spec_type.map(str::to_string),
            count,
        }
    }

    #[test]
    fn task_kind_label_folds_unknown_to_other() {
        assert_eq!(
            row("queued", Some("never_seen"), 1).task_kind_label(),
            "other"
        );
    }

    /// The tag Postgres will actually see in `spec->>'type'` for a real
    /// spec — serialized the same way the enqueue path serializes it.
    ///
    /// Deriving the tag instead of hardcoding the string is the point: two
    /// variants carry an explicit `#[serde(rename)]` (`Automation` →
    /// `workflow`, `AutomationStep` → `workflow_step`) and the enum is
    /// `rename_all = "snake_case"`, so the wire tag is not recoverable from
    /// the variant name by eye. A literal table would keep passing through
    /// a rename while the query quietly started folding that kind into
    /// `other` — the same untyped drift that produced the original bug.
    fn tag_of(spec: &TaskSpec) -> String {
        serde_json::to_value(spec).expect("TaskSpec serializes")["type"]
            .as_str()
            .expect("TaskSpec is internally tagged under `type`")
            .to_string()
    }

    /// Compile-time guard for the *other* half of the drift: a new
    /// `TaskSpec` variant. Adding one breaks this match, which forces a
    /// decision about its `task_kind` bucket instead of letting it fold
    /// silently into `other`.
    #[allow(dead_code)]
    fn every_variant_is_accounted_for(spec: &TaskSpec) {
        match spec {
            TaskSpec::Agent { .. }
            | TaskSpec::Automation { .. }
            | TaskSpec::Resume { .. }
            | TaskSpec::AutomationStep { .. }
            | TaskSpec::AutomationDecision { .. }
            | TaskSpec::Custom { .. }
            | TaskSpec::Airway { .. }
            | TaskSpec::Compile { .. } => {}
        }
    }

    #[test]
    fn task_kind_label_matches_capacity_label_space() {
        // This pins the MAPPING, spec variant by spec variant — not just
        // that the set is closed. So `task_kind_label`'s note that a new
        // label is "a cardinality and alerting question first" is true of
        // the DESIGN and does not make it free: adding one fails here.
        //
        // Adding a label therefore costs, in order: update this test;
        // decide whether `ConcurrencyCaps` should emit it too (only if you
        // want depth and capacity on a shared dashboard axis — the HPA does
        // not care, it sums across kinds); and check the per-kind alerting
        // queries in internal-docs/compile-boundary.md. None of that is a
        // prohibition, it is the checklist.
        //
        // `Custom { kind }` folding to `other` is the cardinality guard
        // itself and should stay. Airway and automation tasks share the
        // generic worker pool and fold into `other`.
        //
        // Every input below is a tag derived from a real `TaskSpec` value, so
        // this asserts against the enum rather than against a copy of its
        // serde tags. The inputs are NOT `agentic_runs.source_type` values —
        // `agentic_task_queue` has no `source_type` column, which is what
        // broke this query in the first place.
        let cases = [
            (
                TaskSpec::Compile {
                    workspace_id: uuid::Uuid::nil(),
                    git_sha: None,
                    branch: None,
                    promote: false,
                    kind: None,
                    owner_user_id: None,
                },
                "compile",
            ),
            (
                TaskSpec::Agent {
                    agent_id: "analytics".into(),
                    question: "hi".into(),
                    extra: None,
                },
                "agent",
            ),
            (
                TaskSpec::Resume {
                    run_id: "r1".into(),
                    resume_data: SuspendedRunData {
                        from_state: "clarifying".into(),
                        original_input: "hi".into(),
                        trace_id: "t1".into(),
                        stage_data: serde_json::json!({}),
                        question: "which one?".into(),
                        suggestions: vec![],
                    },
                    answer: "that one".into(),
                },
                "agent",
            ),
            (
                TaskSpec::Automation {
                    workflow_ref: "a.automation.yml".into(),
                    variables: None,
                    retry_from_run_id: None,
                    cache_enabled: false,
                    body: None,
                    initial_render_context: None,
                },
                "other",
            ),
            (
                TaskSpec::AutomationStep {
                    step_config: serde_json::json!({}),
                    render_context: serde_json::json!({}),
                    workflow_context: serde_json::json!({}),
                },
                "other",
            ),
            (
                TaskSpec::AutomationDecision {
                    run_id: "r1".into(),
                    pending_child_answer: None,
                },
                "other",
            ),
            (
                TaskSpec::Airway {
                    pipeline_ref: "p.airway.yml".into(),
                    variables: None,
                    resources: vec![],
                    backfill_from: None,
                    backfill_to: None,
                    // `None` = airway's own defaults; this test asserts the
                    // queue-depth label, not admission.
                    contract_policy: None,
                    environment: None,
                },
                "other",
            ),
            (
                TaskSpec::Custom {
                    kind: "preagg_cycle".into(),
                    payload: serde_json::json!({}),
                },
                "other",
            ),
        ];

        for (spec, expected) in &cases {
            let tag = tag_of(spec);
            assert_eq!(
                row("queued", Some(&tag), 1).task_kind_label(),
                *expected,
                "tag={tag}"
            );
        }

        // A spec with no `type` key at all — malformed, or the legacy
        // externally-tagged shape `extract_task_type` still falls back for.
        assert_eq!(row("queued", None, 1).task_kind_label(), "other");
    }

    #[test]
    fn fold_sums_spec_types_that_share_a_label() {
        // `agent` and `resume` are separate SQL groups but one metric series.
        // Emitting both would write a duplicate sample and cost the entire
        // scrape, so they must be summed into a single entry.
        let folded = fold_queue_depth(&[
            row("queued", Some("agent"), 3),
            row("queued", Some("resume"), 2),
            row("queued", Some("workflow"), 4),
            row("queued", Some("airway"), 1),
            row("claimed", Some("agent"), 7),
        ]);

        assert_eq!(folded.get(&("queued".to_string(), "agent")), Some(&5));
        assert_eq!(folded.get(&("queued".to_string(), "other")), Some(&5));
        assert_eq!(folded.get(&("claimed".to_string(), "agent")), Some(&7));
        assert_eq!(folded.len(), 3);
    }

    #[test]
    fn queue_depth_exposition_has_no_duplicate_series() {
        let mut body = String::new();
        push_queue_depth(
            &mut body,
            &fold_queue_depth(&[
                row("queued", Some("agent"), 3),
                row("queued", Some("resume"), 2),
                row("queued", Some("compile"), 1),
                row("claimed", Some("workflow"), 6),
                row("dead", Some("custom"), 9),
            ]),
        );

        let samples: Vec<&str> = body
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split_whitespace().next().unwrap_or_default())
            .collect();
        let mut unique = samples.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(samples.len(), unique.len(), "duplicate series in:\n{body}");

        assert!(body.contains("oxy_queue_depth_queued{task_kind=\"agent\"} 5"));
        assert!(body.contains("oxy_queue_depth_queued{task_kind=\"compile\"} 1"));
        assert!(body.contains("oxy_queue_depth_claimed{task_kind=\"other\"} 6"));
        assert!(body.contains("oxy_queue_depth_dead{task_kind=\"other\"} 9"));
        // A status with no rows still gets its HELP/TYPE header.
        assert!(body.contains("# TYPE oxy_queue_depth_dead gauge"));
    }

    #[test]
    fn empty_queue_emits_headers_but_no_samples() {
        let mut body = String::new();
        push_queue_depth(&mut body, &fold_queue_depth(&[]));
        assert!(body.lines().all(|l| l.starts_with('#')), "{body}");
        assert!(body.contains("# TYPE oxy_queue_depth_queued gauge"));
    }

    /// The alert is `time() - gauge > 300`, so a pre-first-probe pod
    /// emitting `0` would compute ~57 years of staleness and page on
    /// every rollout. Absence is the only safe encoding, and the
    /// scoped `absent()` matcher in the VMRule is what covers it.
    #[test]
    fn probe_gauge_is_absent_until_the_first_probe() {
        let mut body = String::new();
        push_router_probe(&mut body, 0, 0);

        assert!(
            !body.contains("oxy_router_last_probe_received_timestamp_seconds"),
            "a never-probed process must emit no timestamp series at all, \
             not even a HELP header a scraper could turn into a 0:\n{body}"
        );
        // The counter is still exported: 0 is a meaningful reading for a
        // counter, and its absence would be indistinguishable from a
        // scrape that never reached this function.
        assert!(
            body.contains("oxy_router_probes_received_total 0"),
            "{body}"
        );
    }

    #[test]
    fn probe_gauge_renders_millis_as_fractional_seconds() {
        let mut body = String::new();
        push_router_probe(&mut body, 42, 1_788_955_660_699);

        assert!(
            body.contains("oxy_router_probes_received_total 42"),
            "{body}"
        );
        // Prometheus timestamps are seconds. Truncating to whole seconds
        // would be fine for a 300s alert but loses the sub-second detail
        // that makes two adjacent probes distinguishable in a graph.
        assert!(
            body.contains("oxy_router_last_probe_received_timestamp_seconds 1788955660.699"),
            "{body}"
        );
        assert!(
            body.contains("# TYPE oxy_router_last_probe_received_timestamp_seconds gauge"),
            "{body}"
        );
    }

    /// A negative reading cannot come from the router — it stores
    /// `SystemTime::now()` millis or 0 — but the exporter must not turn
    /// one into a timestamp before 1970 if that ever changes.
    #[test]
    fn probe_gauge_treats_a_negative_reading_as_never() {
        let mut body = String::new();
        push_router_probe(&mut body, 1, -5);
        assert!(
            !body.contains("oxy_router_last_probe_received_timestamp_seconds"),
            "{body}"
        );
    }
}
