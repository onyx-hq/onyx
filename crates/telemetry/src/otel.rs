//! The OTLP exporters, as `tracing` subscriber layers.
//!
//! Two signals, two layers, one resource:
//!
//! - **Traces** — `tracing-opentelemetry` turns every span that passes the
//!   export filter into an OTel span (fields → attributes, `#[instrument]`
//!   errors → exception events). Parent/child follows the `tracing` span tree,
//!   so an agent run nested under its HTTP request span arrives as one trace.
//! - **Logs** — `opentelemetry-appender-tracing` ships every *event* that passes
//!   the same filter as an OTel log record, with the current span's
//!   `trace_id` / `span_id` attached by the SDK. This is what makes "show me
//!   the logs of this request" a click in HyperDX rather than a grep.
//!
//! **Trace context is always on; export is a deployment choice.** The tracer
//! layer is installed for every server-shaped process even with no endpoint
//! configured, with a provider that has no exporter: spans get W3C ids, the
//! JSON stderr lines carry `trace_id` / `span_id`, and the serve → ide hop
//! propagates `traceparent` — all of which the cluster's stdout-tailing
//! collector can use without any OTLP receiver existing. `OTEL_SDK_DISABLED`
//! is the switch for the whole thing.
//!
//! **Logs export is opt-in** (`OTEL_LOGS_EXPORTER=otlp`), the one deliberate
//! departure from the spec's default. Verified against oxy-hq/infrastructure:
//! both clusters' log shipper is a filelog DaemonSet tailing container stdout
//! into ClickHouse — the one and only shipper, by design. Shipping the same
//! lines over OTLP as well would store every log twice. The stderr JSON is the
//! log path there; OTLP logs exist for a collector that does *not* tail stdout
//! (the local all-in-one ClickStack, HyperDX Cloud).
//!
//! Events at `info` and below are **never** recorded as span events: the logs
//! already hold them (on stdout or over OTLP), a span has a 128-event cap, and
//! on a process with no exporter every one of them would still be allocated
//! into a span builder that is then dropped. `warn` and `error` are, so a
//! failing span is self-describing even when the logs table is filtered.
//!
//! Each signal resolves its own endpoint: `OTEL_EXPORTER_OTLP_ENDPOINT`
//! covers both, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `_LOGS_ENDPOINT` one
//! each, exactly as the SDK resolves them. A traces endpoint on its own turns
//! trace export on; a logs endpoint on its own does not — logs additionally
//! require `OTEL_LOGS_EXPORTER=otlp`, the opt-in above.
//!
//! Transport is OTLP/HTTP + protobuf on the collector's `:4318` — the endpoint
//! must be the HTTP one; the gRPC transport is not compiled in. The blocking
//! `reqwest` client the exporter uses is created on its own thread by the
//! crate, so building these inside a Tokio runtime is safe.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Subscriber;
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer};

use crate::resource;

/// Level the export filter defaults to. Deliberately more verbose than the
/// stderr default (`warn`): what is printed is what a human tails, what is
/// exported is what a human *searches* after the fact.
pub const DEFAULT_EXPORT_LEVEL: &str = "info";

/// How long shutdown waits for the last batches to leave. Longer than the
/// batch schedule delay (5s) so a clean exit does not drop the final spans.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(8);

/// What the environment asked for. Built once in `main` and handed to
/// [`layers`]; pure enough to unit-test the parsing.
#[derive(Debug, Clone)]
pub struct OtelConfig {
    /// Where traces go: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, else
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`. `None` = the tracer layer still runs
    /// (ids, propagation) but no span leaves.
    pub traces_endpoint: Option<String>,
    /// Where logs go, resolved the same way from `_LOGS_ENDPOINT`.
    pub logs_endpoint: Option<String>,
    /// `OTEL_SDK_DISABLED=true` — the kill switch for context AND export.
    /// `main` also sets it for one-shot CLI commands.
    pub sdk_disabled: bool,
    /// `OTEL_TRACES_EXPORTER` is anything but `none`.
    pub traces: bool,
    /// `OTEL_LOGS_EXPORTER` is `otlp` — opt-in, see the module doc.
    pub logs: bool,
    /// `OXY_OTEL_FILTER`, else `info` plus the noisy-crate suppressions.
    pub filter: String,
    /// The fleet role, for `service.name` — see [`resource::role_hint`].
    pub role: Option<&'static str>,
}

impl OtelConfig {
    /// Read the OpenTelemetry environment contract.
    pub fn from_env(role: Option<&'static str>) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let generic = env("OTEL_EXPORTER_OTLP_ENDPOINT");
        Self {
            traces_endpoint: env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").or_else(|| generic.clone()),
            logs_endpoint: env("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").or(generic),
            sdk_disabled: env("OTEL_SDK_DISABLED")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true")),
            traces: traces_exporter_enabled(env("OTEL_TRACES_EXPORTER").as_deref()),
            logs: logs_exporter_enabled(env("OTEL_LOGS_EXPORTER").as_deref()),
            filter: env("OXY_OTEL_FILTER")
                .unwrap_or_else(|| crate::directives_for_level(DEFAULT_EXPORT_LEVEL)),
            role,
        }
    }

    /// Whether the tracer layer is installed at all (ids, propagation).
    pub fn context_enabled(&self) -> bool {
        !self.sdk_disabled
    }

    /// Spans leave the process.
    pub fn traces_exported(&self) -> bool {
        self.context_enabled() && self.traces && self.traces_endpoint.is_some()
    }

    /// Log records leave the process.
    pub fn logs_exported(&self) -> bool {
        self.context_enabled() && self.logs && self.logs_endpoint.is_some()
    }

    /// Whether anything actually leaves the process.
    pub fn export_enabled(&self) -> bool {
        self.traces_exported() || self.logs_exported()
    }
}

/// `OTEL_TRACES_EXPORTER` semantics: unset means the spec default (`otlp`),
/// `none` switches the signal off, and any other value is taken as "on" — the
/// only exporter compiled in is OTLP, so there is nothing else to select.
pub fn traces_exporter_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|v| v.trim().eq_ignore_ascii_case("none"))
}

/// `OTEL_LOGS_EXPORTER` semantics: on only when it says `otlp`. Unset is off,
/// because the clusters this ships to already tail stdout (module doc).
pub fn logs_exporter_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("otlp"))
}

/// What [`layers`] hands back: the layers to compose, and anything that went
/// wrong building them. A broken exporter never fails boot — it is reported
/// once the subscriber is installed, and that signal simply stays off.
pub struct OtelLayers<S> {
    pub layers: Vec<Box<dyn Layer<S> + Send + Sync + 'static>>,
    pub problems: Vec<String>,
}

#[derive(Default)]
struct Providers {
    tracer: Option<SdkTracerProvider>,
    logger: Option<SdkLoggerProvider>,
}

static PROVIDERS: OnceLock<Mutex<Providers>> = OnceLock::new();

/// Build the layers for `config`. Installs the W3C propagator and the global
/// tracer provider as a side effect, and remembers the providers for
/// [`shutdown`]. Returns no layers only when [`OtelConfig::context_enabled`]
/// is false; without an endpoint the tracer layer is still there, exporting
/// nothing.
pub fn layers<S>(config: &OtelConfig) -> OtelLayers<S>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
{
    let mut out = OtelLayers {
        layers: Vec::new(),
        problems: Vec::new(),
    };
    if !config.context_enabled() {
        return out;
    }

    let resource = resource::build(config.role);
    global::set_text_map_propagator(TraceContextPropagator::new());
    let mut providers = Providers::default();

    let mut tracer = SdkTracerProvider::builder().with_resource(resource.clone());
    if config.traces_exported() {
        match build_span_exporter() {
            Ok(exporter) => tracer = tracer.with_batch_exporter(exporter),
            Err(e) => out
                .problems
                .push(format!("OTLP trace exporter not started: {e}")),
        }
    }
    let provider = tracer.build();
    global::set_tracer_provider(provider.clone());
    out.layers.push(trace_layer(&provider, config));
    providers.tracer = Some(provider);

    if config.logs_exported() {
        match build_log_exporter() {
            Ok(exporter) => {
                let provider = SdkLoggerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(resource)
                    .build();
                let layer = OpenTelemetryTracingBridge::new(&provider)
                    .with_filter(EnvFilter::new(&config.filter));
                out.layers.push(Box::new(layer));
                providers.logger = Some(provider);
            }
            Err(e) => out
                .problems
                .push(format!("OTLP log exporter not started: {e}")),
        }
    }

    if PROVIDERS.set(Mutex::new(providers)).is_err() {
        out.problems.push("OTel providers were already installed; the earlier set is what will be flushed at exit".into());
    }
    out
}

fn build_span_exporter() -> Result<SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    // Endpoint, headers, timeout and compression are resolved from the
    // OTEL_EXPORTER_OTLP_* variables by the builder; only the wire protocol is
    // pinned, because only the HTTP transport is compiled in.
    SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
}

fn build_log_exporter() -> Result<LogExporter, opentelemetry_otlp::ExporterBuildError> {
    LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
}

fn trace_layer<S>(
    provider: &SdkTracerProvider,
    config: &OtelConfig,
) -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
{
    let layer = tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("oxy"))
        // `level` / `target` as span attributes: the cheapest way to filter a
        // HyperDX span search down to one module.
        .with_level(true)
        .with_target(true);
    // Spans at the configured verbosity; events only when they mark a
    // failure. The logs (stdout or OTLP) already carry the rest.
    let verbosity = EnvFilter::new(&config.filter);
    let spans_and_failures =
        filter_fn(|meta| meta.is_span() || *meta.level() <= tracing::Level::WARN);
    Box::new(layer.with_filter(verbosity.and(spans_and_failures)))
}

/// Flush and stop the exporters. Blocks for up to [`SHUTDOWN_TIMEOUT`] per
/// signal; call it last, after everything that might still emit. Returns the
/// problems encountered, so the caller can print them — `tracing` itself is
/// half torn down by then.
pub fn shutdown() -> Vec<String> {
    let mut problems = Vec::new();
    let Some(lock) = PROVIDERS.get() else {
        return problems;
    };
    let providers = std::mem::take(&mut *lock.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(tracer) = providers.tracer
        && let Err(e) = tracer.shutdown_with_timeout(SHUTDOWN_TIMEOUT)
    {
        problems.push(format!("OTLP trace exporter shutdown: {e}"));
    }
    if let Some(logger) = providers.logger
        && let Err(e) = logger.shutdown_with_timeout(SHUTDOWN_TIMEOUT)
    {
        problems.push(format!("OTLP log exporter shutdown: {e}"));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(traces_endpoint: Option<&str>, logs_endpoint: Option<&str>) -> OtelConfig {
        OtelConfig {
            traces_endpoint: traces_endpoint.map(str::to_string),
            logs_endpoint: logs_endpoint.map(str::to_string),
            sdk_disabled: false,
            traces: true,
            logs: true,
            filter: crate::directives_for_level(DEFAULT_EXPORT_LEVEL),
            role: Some("serve"),
        }
    }

    #[test]
    fn traces_default_on_and_none_turns_them_off() {
        assert!(traces_exporter_enabled(None));
        assert!(traces_exporter_enabled(Some("otlp")));
        assert!(!traces_exporter_enabled(Some("none")));
        assert!(!traces_exporter_enabled(Some(" NONE ")));
    }

    #[test]
    fn logs_are_opt_in() {
        assert!(!logs_exporter_enabled(None));
        assert!(!logs_exporter_enabled(Some("none")));
        assert!(logs_exporter_enabled(Some("otlp")));
        assert!(logs_exporter_enabled(Some(" OTLP ")));
        assert!(!logs_exporter_enabled(Some("console")));
    }

    #[test]
    fn export_follows_the_per_signal_endpoints() {
        let both = config(Some("http://c:4318"), Some("http://c:4318"));
        assert!(both.traces_exported() && both.logs_exported());

        let none = config(None, None);
        assert!(!none.export_enabled());
        assert!(none.context_enabled(), "ids and propagation stay on");

        let traces_only = config(Some("http://c:4318"), None);
        assert!(traces_only.traces_exported());
        assert!(!traces_only.logs_exported());
        assert!(traces_only.export_enabled());

        let logs_only = config(None, Some("http://c:4318"));
        assert!(!logs_only.traces_exported());
        assert!(logs_only.logs_exported());

        let disabled = OtelConfig {
            sdk_disabled: true,
            ..both.clone()
        };
        assert!(!disabled.export_enabled());
        assert!(!disabled.context_enabled());

        let no_signals = OtelConfig {
            traces: false,
            logs: false,
            ..both
        };
        assert!(!no_signals.export_enabled());
    }

    #[test]
    fn without_an_endpoint_the_context_layer_installs_and_nothing_else() {
        let built = layers::<tracing_subscriber::Registry>(&config(None, None));
        assert_eq!(built.layers.len(), 1, "the tracer layer, with no exporter");
        assert!(built.problems.is_empty());
    }

    #[test]
    fn a_disabled_sdk_installs_nothing() {
        let disabled = OtelConfig {
            sdk_disabled: true,
            ..config(Some("http://c:4318"), None)
        };
        let built = layers::<tracing_subscriber::Registry>(&disabled);
        assert!(built.layers.is_empty());
        assert!(built.problems.is_empty());
    }

    #[test]
    fn shutdown_without_providers_is_a_no_op() {
        assert!(shutdown().is_empty());
    }

    /// Agent runs are created with `parent: None` (the product console lists
    /// runs as root spans) and then `follows_from` whatever started them. That
    /// only makes the run navigable in HyperDX if the OTel layer turns the
    /// relation into a span **link** — this pins that it does.
    #[test]
    fn follows_from_becomes_a_span_link_on_a_detached_root() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::trace::InMemorySpanExporter;
        use tracing_subscriber::layer::SubscriberExt;

        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        let _guard = tracing::subscriber::set_default(subscriber);

        {
            let task = tracing::info_span!("agentic_task");
            let _entered = task.enter();
            let run = tracing::info_span!(parent: None, "analytics.run");
            run.follows_from(tracing::Span::current());
            drop(run);
        }
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        let task = spans.iter().find(|s| s.name == "agentic_task").unwrap();
        let run = spans.iter().find(|s| s.name == "analytics.run").unwrap();
        assert_ne!(
            run.span_context.trace_id(),
            task.span_context.trace_id(),
            "still its own root trace"
        );
        assert_eq!(run.links.links.len(), 1, "{:?}", run.links);
        assert_eq!(
            run.links.links[0].span_context.span_id(),
            task.span_context.span_id()
        );
    }
}
