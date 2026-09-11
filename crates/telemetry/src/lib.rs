//! Platform telemetry — what an **operator** of Oxy sees in ClickStack /
//! HyperDX: every `tracing` span and event this process emits, shipped over
//! OTLP to the cluster's OpenTelemetry collector, plus the structured log
//! format the container runtime captures from stderr.
//!
//! Not to be confused with `oxy-observability`, the **product** feature: that
//! crate's `SpanCollectorLayer` writes agent / automation spans into the
//! tenant-facing ClickHouse (`OXY_CLICKHOUSE_*`) that the in-app Traces console
//! reads. The two are different stores answering different questions, and they
//! are wired independently — a deployment can run either, both, or neither.
//!
//! One `tracing` subscriber, several sinks. `oxy-server`'s `logging` module
//! composes them; this crate owns the pieces:
//!
//! | Module          | What it provides                                                      |
//! |-----------------|-----------------------------------------------------------------------|
//! | [`otel`]        | The OTLP trace + log exporters as subscriber layers, and their shutdown |
//! | [`resource`]    | Who is emitting: `service.name` per fleet role, version, environment  |
//! | [`json_format`] | The stderr JSON line: flat fields, `trace_id` / `span_id`, current span |
//! | [`http_trace`]  | One `SERVER` span per HTTP request, named by route, W3C parent honoured |
//! | [`propagation`] | `traceparent` injection for the internal serve → ide hop               |
//! | [`stderr_capture`] | Every stray stderr line (dependencies, C libs, panics) as JSON, repeats collapsed |
//!
//! ## Configuration is the OpenTelemetry environment contract
//!
//! The context layer (ids on every span and log line, `traceparent` on the
//! internal hop) is on for every server-shaped process; `OTEL_SDK_DISABLED`
//! turns it off. Nothing *leaves* until `OTEL_EXPORTER_OTLP_ENDPOINT` (or a
//! per-signal `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `_LOGS_ENDPOINT`) is set —
//! the SDK's own `localhost:4318` default is deliberately *not* honoured, or a
//! laptop without a collector would log an export failure every five seconds.
//! From there the standard variables apply: `OTEL_SERVICE_NAME`,
//! `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_EXPORTER_OTLP_HEADERS` (HyperDX Cloud's
//! `authorization`), `OTEL_TRACES_SAMPLER` / `_ARG`, `OTEL_BSP_*`,
//! `OTEL_SDK_DISABLED`, `OTEL_TRACES_EXPORTER` (`none` switches traces off) and
//! `OTEL_LOGS_EXPORTER` (`otlp` switches logs *on* — opt-in, see `otel`). The
//! one Oxy-specific knob is
//! `OXY_OTEL_FILTER`, a `RUST_LOG`-style directive string that decides what is
//! *exported*, independent of what is *printed* (`OXY_LOG_LEVEL`).
//!
//! The full operator guide is `internal-docs/platform-telemetry.md`.

pub mod http_trace;
pub mod json_format;
pub mod otel;
pub mod propagation;
pub mod resource;
pub mod stderr_capture;
pub mod with_dispatch;

/// Framework crates whose `info`/`debug` output is wire-level chatter, not
/// something an operator acts on: HTTP frames, raw SQL, TLS handshakes. Both
/// the stderr layers and the OTLP layers append this to whatever level the
/// operator asked for, so raising `OXY_LOG_LEVEL=debug` stays readable.
/// The framework half is `oxy_shared::log_noise::FRAMEWORK_NOISE_DIRECTIVES`
/// (prefix semantics and the measurements behind each entry are documented
/// there); this constant is that list plus the one platform-only entry below.
/// `RUST_LOG` / `OXY_OTEL_FILTER` bypass it entirely — the expert escape hatch.
///
/// `custom_app_function` is not framework chatter but a privacy boundary: it
/// is the target of a tenant's `ctx.log()` lines from an Oxy Function, whose
/// content routinely carries application data. The platform store (stderr,
/// OTLP) gets an app's `warn`/`error` lines with their trace id and nothing
/// below; the product store keeps every line behind the app-admin gate
/// through its own filter, which this list does not touch.
pub const NOISY_CRATE_DIRECTIVES: &str = concat!(
    oxy_shared::framework_noise_directives!(),
    ",custom_app_function=warn"
);

/// The framework half alone — what every filter shares, the product
/// observability store included. Lives in `oxy-shared` so that store's crate
/// does not take the OTLP SDK for one string.
pub use oxy_shared::log_noise::FRAMEWORK_NOISE_DIRECTIVES;

/// `"{level},{NOISY_CRATE_DIRECTIVES}"` — the directive string both the stderr
/// and the export filters are built from when the operator gives only a level.
pub fn directives_for_level(level: &str) -> String {
    format!("{level},{NOISY_CRATE_DIRECTIVES}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::EnvFilter;

    /// Parsing proves the string is well-formed; this proves the *prefix*
    /// semantics the list relies on — a segment-vs-prefix mistake would pass
    /// the parse test and suppress nothing.
    #[test]
    fn the_noisy_list_suppresses_by_prefix_and_leaves_oxy_alone() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> Layer<S> for Seen {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.0.lock().unwrap().push(format!(
                    "{}:{}",
                    event.metadata().target(),
                    event.metadata().level()
                ));
            }
        }
        let seen = Seen::default();
        let filter = EnvFilter::new(directives_for_level("debug"));
        let subscriber = tracing_subscriber::registry().with(seen.clone().with_filter(filter));
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::debug!(target: "opentelemetry_sdk", "BatchSpanProcessor.ExportingDueToTimer");
        tracing::debug!(target: "opentelemetry-otlp", "HttpClient.ExportStarted");
        tracing::debug!(target: "aws_smithy_runtime_api::client::interceptors", "x");
        tracing::debug!(target: "aws_sdk_s3::operation", "x");
        tracing::debug!(target: "hyper_rustls::config", "x");
        tracing::debug!(target: "tower_http::trace", "x");
        tracing::debug!(target: "tower::buffer::worker", "x");
        tracing::debug!(target: "clickhouse::insert", "x");
        tracing::info!(target: "custom_app_function", "a tenant's ctx.log() line");
        tracing::warn!(target: "opentelemetry_sdk", "BatchSpanProcessor.SpanDropped");
        tracing::debug!(target: "oxy_app::server::api", "kept");
        tracing::debug!(target: "agentic_llm::genai", "kept");

        let seen = seen.0.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                "opentelemetry_sdk:WARN".to_string(),
                "oxy_app::server::api:DEBUG".to_string(),
                "agentic_llm::genai:DEBUG".to_string(),
            ],
            "framework debug (tower family included) and a tenant's ctx.log() info are \
             suppressed on the platform side; warns and oxy's own debug pass"
        );
    }

    #[test]
    fn the_noisy_list_parses_as_env_filter_directives() {
        // A typo here would make EnvFilter::new panic at boot, in every mode.
        for level in ["warn", "info", "debug", "trace"] {
            let directives = directives_for_level(level);
            EnvFilter::try_new(&directives)
                .unwrap_or_else(|e| panic!("{directives:?} did not parse: {e}"));
        }
    }
}
