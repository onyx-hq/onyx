use std::sync::Arc;
use std::time::Instant;

use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::flush_queue::SpanFlushQueue;
use crate::layer::SpanCollectorLayer;
use crate::store::ObservabilityStore;
use crate::types::SpanRecord;

const DEFAULT_LOG_LEVEL: &str = "warn";
const DEFAULT_OBSERVABILITY_LOG_LEVEL: &str = "debug";

/// Build an EnvFilter for console output, falling back to DEFAULT_LOG_LEVEL if OXY_LOG_LEVEL is invalid
fn build_env_filter() -> EnvFilter {
    // First try RUST_LOG (standard env var)
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        return filter.add_directive("deser_incomplete=off".parse().unwrap());
    }

    // Then try OXY_LOG_LEVEL with validation
    let level = std::env::var("OXY_LOG_LEVEL").unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());

    match EnvFilter::try_new(&level) {
        Ok(filter) => filter.add_directive("deser_incomplete=off".parse().unwrap()),
        Err(_) => {
            eprintln!(
                "Warning: Invalid OXY_LOG_LEVEL='{}', falling back to '{}'",
                level, DEFAULT_LOG_LEVEL
            );
            EnvFilter::try_new(DEFAULT_LOG_LEVEL)
                .unwrap()
                .add_directive("deser_incomplete=off".parse().unwrap())
        }
    }
}

/// Build an EnvFilter for observability spans. Uses `OXY_OBSERVABILITY_LOG_LEVEL`
/// env var, defaults to "debug" to capture all traces.
fn build_observability_filter() -> EnvFilter {
    let level = std::env::var("OXY_OBSERVABILITY_LOG_LEVEL")
        .unwrap_or_else(|_| DEFAULT_OBSERVABILITY_LOG_LEVEL.to_string());
    observability_filter_for(&level)
}

/// The filter for a level (or a full directive string) — the env-free half,
/// so it can be tested without touching `OXY_OBSERVABILITY_LOG_LEVEL`.
fn observability_filter_for(level: &str) -> EnvFilter {
    // Whether the operator wrote their own filter is decided on the string
    // that *parsed*: a typo falls back to the default level with the
    // suppressions, not to an unfiltered `debug`.
    let (filter, operator_filter) = match EnvFilter::try_new(level) {
        Ok(filter) => (filter, is_full_directive_string(level)),
        Err(_) => {
            eprintln!(
                "Warning: Invalid observability log level '{}', falling back to '{}'",
                level, DEFAULT_OBSERVABILITY_LOG_LEVEL
            );
            (
                EnvFilter::try_new(DEFAULT_OBSERVABILITY_LOG_LEVEL).unwrap(),
                false,
            )
        }
    };
    let filter = filter
        .add_directive("deser_incomplete=off".parse().unwrap())
        // Custom-app host-op spans (one per `ctx.query` / `ctx.fetch` / …) are
        // platform telemetry: they go to the OTLP trace, not to this store,
        // which keeps one row per invocation rather than one per query. See
        // `custom_apps_functions::host_call_attrs::HOST_CALL_TARGET`.
        .add_directive("oxy::host_call=off".parse().unwrap())
        // The platform's line for a failed custom-app invocation
        // (`custom_apps_functions::failure_signal`): on-call's, in the stderr
        // log. The tenant's store already holds the invocation and its error.
        .add_directive("oxy::app_function=off".parse().unwrap())
        // HTTP request spans are platform telemetry too. Nothing tenant-facing
        // reads them (the console roots on `agent.run_agent` / `analytics.run`),
        // and on oxy-dev they were ~380k rows a day of this store, mostly the
        // kubelet's probes before those stopped being traced at all.
        .add_directive("oxy_telemetry::http_trace=off".parse().unwrap());
    // A full directive string is the operator's own filter and is left alone,
    // as the telemetry side treats `RUST_LOG` / `OXY_OTEL_FILTER`; only a bare
    // level gets the framework suppressions appended.
    if operator_filter {
        return filter;
    }
    // The framework half of what the stderr and OTLP filters carry — never
    // `custom_app_function`, whose `info` lines are a tenant's `ctx.log()` and
    // are exactly what this store must keep. With the store defaulting to
    // `debug`, the AWS SDK's credential chain and the OTel exporter's own
    // narration were landing here as tenant-visible spans (~280k a day on
    // oxy-dev: `try_op`, `finally_attempt`, `clickhouse.insert`).
    oxy_shared::log_noise::FRAMEWORK_NOISE_DIRECTIVES
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .fold(filter, |f, d| match d.parse() {
            Ok(directive) => f.add_directive(directive),
            Err(e) => {
                // Never take the process down over a filter string; the
                // framework line just stays at the operator's level.
                eprintln!("Warning: skipping observability filter directive '{d}': {e}");
                f
            }
        })
}

/// `info,opentelemetry=debug` is a filter the operator wrote; `info` is a level.
fn is_full_directive_string(level: &str) -> bool {
    level.contains(',') || level.contains('=')
}

/// Build just the `SpanCollectorLayer` and its receiver. No store, no bridge.
///
/// Use this when you need to install the tracing layer early (e.g. before the
/// database is ready) and defer the store wiring. Spans emitted before the
/// bridge is spawned accumulate in the unbounded channel — call
/// [`spawn_bridge`] later with the matching receiver to drain them.
pub fn build_layer_and_receiver() -> (
    SpanCollectorLayer,
    tokio::sync::mpsc::UnboundedReceiver<SpanRecord>,
) {
    let (span_tx, span_rx) = tokio::sync::mpsc::unbounded_channel::<SpanRecord>();
    let service_name = std::env::var("OXY_SERVICE_NAME").unwrap_or_else(|_| "oxy".to_string());
    let layer = SpanCollectorLayer::new(span_tx, service_name);
    (layer, span_rx)
}

/// Records queued when the store starts flowing again after an outage; beyond
/// this, oldest records are dropped (with a warning) to bound memory.
const MAX_BUFFERED_SPANS: usize = 5_000;

/// Time-based flush cadence. Every flush is one write, and on a columnar store
/// a write is a physical unit the reader later pays for: on ClickHouse each
/// `INSERT` creates a MergeTree part, so a 1s cadence at low span rates
/// produces ~86k near-empty parts/day per writer for background merges to chase
/// — the road to "too many parts". The cost model was first learned the hard
/// way on DuckLake, where one commit was one parquet file: the 2026-07-06 dev
/// outage buried reads in file enumeration until every observability endpoint
/// blew past the 30s server / 60s HTTP timeout. 30s bounds writes at ≤2,880/day
/// per writer while keeping the panel near-real-time; bursts still flush early
/// on [`FLUSH_BATCH_SIZE`].
const FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Size-based flush trigger — bounds memory and batch width under load.
const FLUSH_BATCH_SIZE: usize = 100;

/// Spawn the batching bridge task that drains `receiver` into `store`.
///
/// Batches are committed on whichever fires first: [`FLUSH_BATCH_SIZE`]
/// records, or the [`FLUSH_INTERVAL`] tick.
///
/// Failed batches are requeued and retried with exponential backoff (see
/// [`SpanFlushQueue`]) so a store outage causes bounded, logged loss instead
/// of silently discarding every batch for its duration.
pub fn spawn_bridge(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<SpanRecord>,
    store: Arc<dyn ObservabilityStore>,
) {
    async fn flush(
        queue: &mut SpanFlushQueue,
        store: &Arc<dyn ObservabilityStore>,
        min_len: usize,
    ) {
        let Some(batch) = queue.take_ready(Instant::now(), min_len) else {
            return;
        };
        // Clone so the batch can be requeued on failure — `insert_spans`
        // consumes its argument.
        match store.insert_spans(batch.clone()).await {
            Ok(()) => queue.on_success(),
            Err(e) => {
                let batch_len = batch.len();
                let dropped = queue.on_failure(batch, Instant::now());
                if dropped > 0 {
                    tracing::error!(
                        "Failed to insert spans; requeued {} but dropped {} oldest (buffer full): {}",
                        batch_len - dropped.min(batch_len),
                        dropped,
                        e
                    );
                } else {
                    tracing::error!(
                        "Failed to insert spans; requeued {} for retry: {}",
                        batch_len,
                        e
                    );
                }
            }
        }
    }

    tokio::spawn(async move {
        let mut queue = SpanFlushQueue::new(
            MAX_BUFFERED_SPANS,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(60),
        );
        let mut interval = tokio::time::interval(FLUSH_INTERVAL);
        interval.tick().await; // consume first immediate tick
        // Drops accumulated since the last report — see the push arm below.
        let mut dropped_since_report: u64 = 0;

        loop {
            tokio::select! {
                msg = receiver.recv() => {
                    match msg {
                        Some(record) => {
                            // Accumulate rather than log per record. `push`
                            // runs once per incoming span, so warning inline
                            // means one line per DROPPED SPAN — the logging
                            // gets loudest exactly when the store is already
                            // failing, and it competes for the same resources.
                            // Measured in oxy-dev on 2026-09-03: 11,209 lines
                            // in 30 minutes from this one statement, 49% of
                            // all oxy log volume. Reported on the flush tick
                            // below instead: one line per FLUSH_INTERVAL with
                            // the window total.
                            dropped_since_report =
                                dropped_since_report.saturating_add(queue.push(record) as u64);
                            if queue.len() >= FLUSH_BATCH_SIZE {
                                flush(&mut queue, &store, FLUSH_BATCH_SIZE).await;
                            }
                        }
                        None => {
                            // Channel closed — final best-effort send, bypassing
                            // the retry backoff (there is no later attempt for
                            // the gate to defer to).
                            if dropped_since_report > 0 {
                                tracing::warn!(
                                    dropped = dropped_since_report,
                                    buffer_capacity = MAX_BUFFERED_SPANS,
                                    "Span buffer full during store outage; dropped oldest records (final)"
                                );
                            }
                            let batch = queue.take_all();
                            if !batch.is_empty() {
                                let _ = store.insert_spans(batch).await;
                            }
                            break;
                        }
                    }
                }
                _ = interval.tick() => {
                    if dropped_since_report > 0 {
                        // Structured fields, not interpolation: `dropped` and
                        // `window_secs` land under `fields.*` in the JSON log
                        // and stay queryable downstream, where a formatted
                        // string would have to be re-parsed.
                        tracing::warn!(
                            dropped = dropped_since_report,
                            window_secs = FLUSH_INTERVAL.as_secs(),
                            buffer_capacity = MAX_BUFFERED_SPANS,
                            "Span buffer full during store outage; dropped oldest records"
                        );
                        dropped_since_report = 0;
                    }
                    flush(&mut queue, &store, 1).await;
                }
            }
        }
    });
}

/// Build a `SpanCollectorLayer` wired to the observability store and spawn the
/// batching bridge task. Convenience combiner over [`build_layer_and_receiver`]
/// + [`spawn_bridge`] for callers that have a store ready at subscriber-install
/// time.
pub fn build_observability_layer(store: Arc<dyn ObservabilityStore>) -> SpanCollectorLayer {
    let (layer, receiver) = build_layer_and_receiver();
    spawn_bridge(receiver, store);
    layer
}

/// Build the EnvFilter used for the observability layer. Exported so callers
/// composing their own subscriber can attach it alongside the layer returned
/// by [`build_observability_layer`].
pub fn observability_filter() -> EnvFilter {
    build_observability_filter()
}

/// Initialize observability with a backend-agnostic store.
///
/// Sets up a tracing subscriber with:
/// - A console `fmt::layer()` filtered by `OXY_LOG_LEVEL`
/// - A `SpanCollectorLayer` filtered by `OXY_OBSERVABILITY_LOG_LEVEL` that writes
///   span records to the observability store.
///
/// Use [`build_observability_layer`] if you need to compose with other layers
/// (e.g. Sentry, file appender).
pub fn init_observability(store: Arc<dyn ObservabilityStore>) {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(build_env_filter()))
        .with(build_observability_layer(store).with_filter(build_observability_filter()))
        .init();

    tracing::debug!("Observability initialized");
}

/// Initialize stdout logging only (no observability export)
pub fn init_stdout() {
    tracing_subscriber::registry()
        .with(build_env_filter())
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// Shutdown observability.
///
/// The actual flush/shutdown is handled by `ObservabilityStore::shutdown()`
/// called from the application entrypoint.
pub fn shutdown() {
    tracing::debug!("Observability shutdown requested");
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    /// Records the target of every event a filter lets through.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<String>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Seen {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(event.metadata().target().to_string());
        }
    }

    #[test]
    fn a_full_directive_string_is_the_operators_own_filter() {
        assert!(super::is_full_directive_string("info,opentelemetry=debug"));
        assert!(super::is_full_directive_string("opentelemetry=debug"));
        assert!(!super::is_full_directive_string("debug"));
        assert!(!super::is_full_directive_string("INFO"));
    }

    #[test]
    fn a_typo_falls_back_to_the_default_with_the_suppressions_not_to_bare_debug() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;
        let seen = Seen::default();
        // Looks like an operator filter, does not parse: must not become
        // "debug with nothing suppressed".
        let subscriber = tracing_subscriber::registry().with(
            seen.clone()
                .with_filter(super::observability_filter_for("debug,oxy=trace!")),
        );
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::debug!(target: "opentelemetry_sdk", "export");
        tracing::debug!(target: "agentic_analytics::solver", "kept");
        assert_eq!(
            seen.0.lock().unwrap().clone(),
            vec!["agentic_analytics::solver".to_string()]
        );
    }

    #[test]
    fn the_product_filter_keeps_platform_spans_and_framework_noise_out() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;
        let seen = Seen::default();
        let subscriber = tracing_subscriber::registry().with(
            seen.clone()
                .with_filter(super::observability_filter_for("debug")),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(target: "oxy_telemetry::http_trace", "request");
        tracing::info!(target: "oxy::host_call", "db.query");
        tracing::warn!(target: "oxy::app_function", "custom-app function invocation failed");
        tracing::debug!(target: "aws_smithy_runtime::client::orchestrator", "try_op");
        tracing::debug!(target: "opentelemetry_sdk", "export");
        tracing::debug!(target: "tower::buffer::worker", "x");
        tracing::debug!(target: "clickhouse::insert", "the store's own write");
        tracing::info!(target: "custom_app_function", "a tenant's ctx.log() line — kept");
        tracing::debug!(target: "agentic_analytics::solver", "kept");
        tracing::info!(target: "oxy_app::server::api::custom_apps_functions", "kept");

        assert_eq!(
            seen.0.lock().unwrap().clone(),
            vec![
                "custom_app_function".to_string(),
                "agentic_analytics::solver".to_string(),
                "oxy_app::server::api::custom_apps_functions".to_string(),
            ]
        );
    }

    use super::*;
    use async_trait::async_trait;
    use oxy_shared::errors::OxyError;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::intent_types::IntentCluster;
    use crate::types::*;

    /// Records every `insert_spans` batch; all other methods are unreachable
    /// in these tests.
    #[derive(Debug, Default)]
    struct RecordingStore {
        batches: Mutex<Vec<Vec<SpanRecord>>>,
    }

    #[async_trait]
    impl ObservabilityStore for RecordingStore {
        async fn list_traces(
            &self,
            _: i64,
            _: i64,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<(Vec<TraceRow>, i64), OxyError> {
            unimplemented!()
        }
        async fn get_trace_detail(&self, _: &str) -> Result<Vec<TraceDetailRow>, OxyError> {
            unimplemented!()
        }
        async fn get_cluster_map_data(
            &self,
            _: u32,
            _: usize,
            _: Option<&str>,
        ) -> Result<Vec<ClusterMapDataRow>, OxyError> {
            unimplemented!()
        }
        async fn get_cluster_infos(&self) -> Result<Vec<ClusterInfoRow>, OxyError> {
            unimplemented!()
        }
        async fn get_trace_enrichments(
            &self,
            _: &[String],
        ) -> Result<Vec<TraceEnrichmentRow>, OxyError> {
            unimplemented!()
        }
        async fn fetch_unprocessed_questions(
            &self,
            _: usize,
        ) -> Result<Vec<(String, String, String)>, OxyError> {
            unimplemented!()
        }
        async fn load_embeddings(
            &self,
        ) -> Result<Vec<(String, String, Vec<f32>, String, String)>, OxyError> {
            unimplemented!()
        }
        async fn store_clusters(&self, _: &[IntentCluster]) -> Result<(), OxyError> {
            unimplemented!()
        }
        async fn load_clusters(&self) -> Result<Vec<IntentCluster>, OxyError> {
            unimplemented!()
        }
        #[allow(clippy::too_many_arguments)]
        async fn store_classification(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: &str,
            _: f32,
            _: &[f32],
            _: &str,
            _: &str,
        ) -> Result<(), OxyError> {
            unimplemented!()
        }
        #[allow(clippy::too_many_arguments)]
        async fn update_classification(
            &self,
            _: &str,
            _: &str,
            _: u32,
            _: &str,
            _: f32,
            _: &[f32],
            _: &str,
            _: &str,
        ) -> Result<(), OxyError> {
            unimplemented!()
        }
        async fn get_intent_analytics(&self, _: u32) -> Result<Vec<IntentAnalyticsRow>, OxyError> {
            unimplemented!()
        }
        async fn get_outliers(&self, _: usize) -> Result<Vec<(String, String)>, OxyError> {
            unimplemented!()
        }
        async fn load_unknown_classifications(
            &self,
        ) -> Result<Vec<(String, String, Vec<f32>, String)>, OxyError> {
            unimplemented!()
        }
        async fn get_unknown_count(&self) -> Result<usize, OxyError> {
            unimplemented!()
        }
        async fn update_cluster_record(&self, _: &IntentCluster) -> Result<(), OxyError> {
            unimplemented!()
        }
        async fn get_next_cluster_id(&self) -> Result<u32, OxyError> {
            unimplemented!()
        }
        async fn store_metric_usages(&self, _: Vec<MetricUsageRecord>) -> Result<(), OxyError> {
            unimplemented!()
        }
        async fn get_metrics_analytics(&self, _: u32) -> Result<MetricAnalyticsData, OxyError> {
            unimplemented!()
        }
        async fn get_metrics_list(
            &self,
            _: u32,
            _: usize,
            _: usize,
        ) -> Result<MetricsListData, OxyError> {
            unimplemented!()
        }
        async fn get_metric_detail(&self, _: &str, _: u32) -> Result<MetricDetailData, OxyError> {
            unimplemented!()
        }
        async fn get_execution_summary(&self, _: u32) -> Result<ExecutionSummaryData, OxyError> {
            unimplemented!()
        }
        async fn get_execution_time_series(
            &self,
            _: u32,
        ) -> Result<Vec<ExecutionTimeBucketData>, OxyError> {
            unimplemented!()
        }
        async fn get_execution_agent_stats(
            &self,
            _: u32,
            _: usize,
        ) -> Result<Vec<AgentExecutionStatsData>, OxyError> {
            unimplemented!()
        }
        #[allow(clippy::too_many_arguments)]
        async fn get_execution_list(
            &self,
            _: u32,
            _: usize,
            _: usize,
            _: Option<&str>,
            _: Option<bool>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<ExecutionListData, OxyError> {
            unimplemented!()
        }
        async fn insert_spans(&self, spans: Vec<SpanRecord>) -> Result<(), OxyError> {
            self.batches.lock().unwrap().push(spans);
            Ok(())
        }
        async fn shutdown(&self) {}
    }

    fn rec(n: usize) -> SpanRecord {
        SpanRecord {
            trace_id: format!("trace-{n}"),
            span_id: format!("span-{n}"),
            parent_span_id: String::new(),
            span_name: "test".into(),
            service_name: "oxy".into(),
            span_attributes: "{}".into(),
            duration_ns: 0,
            status_code: "OK".into(),
            status_message: String::new(),
            event_data: "[]".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
        }
    }

    // Incident 2026-07-06 (dev): flushing every second at low span rates
    // committed one ~1-row parquet file per second into DuckLake, and the
    // resulting small-file explosion pushed every observability read past the
    // 30s server / 60s HTTP timeouts. The store is ClickHouse now and the unit
    // is a MergeTree part rather than a file, but the shape is the same — a
    // tiny write per second is a debt the reader pays. Small batches must be
    // HELD for the flush cadence (30s), not committed eagerly.

    #[tokio::test(start_paused = true)]
    async fn small_batches_are_held_for_the_flush_cadence() {
        let store = Arc::new(RecordingStore::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_bridge(rx, store.clone() as Arc<dyn ObservabilityStore>);

        for n in 0..3 {
            tx.send(rec(n)).unwrap();
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            store.batches.lock().unwrap().is_empty(),
            "a small batch must not commit within 5s — eager 1s commits are what \
             buried reads in tiny write units (2026-07-06)"
        );

        tokio::time::sleep(Duration::from_secs(26)).await; // t = 31s > cadence
        let batches = store.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "one combined commit after the cadence");
        assert_eq!(batches[0].len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn full_batches_flush_without_waiting() {
        let store = Arc::new(RecordingStore::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_bridge(rx, store.clone() as Arc<dyn ObservabilityStore>);

        for n in 0..100 {
            tx.send(rec(n)).unwrap();
        }
        // Yield to the bridge task without advancing past the cadence.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let batches = store.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "a full batch flushes on size, not time");
        assert_eq!(batches[0].len(), 100);
    }
}
