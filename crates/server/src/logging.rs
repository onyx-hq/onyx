//! The process-wide `tracing` subscriber for the `oxy` binary: one registry,
//! every sink composed at boot, before the CLI dispatches.
//!
//! Sinks, each behind its own filter so one cannot starve another:
//!
//! | Sink                          | When                            | Filter                          |
//! |-------------------------------|---------------------------------|---------------------------------|
//! | Sentry                        | always                          | `warn`+                         |
//! | stderr, human-readable        | `LogFormat::Local`              | `OXY_LOG_LEVEL` (default `warn`) |
//! | `oxy.<date>.log` in state dir | `LogFormat::Local`              | same                            |
//! | stderr, one JSON object/line  | `LogFormat::Cloud`              | same                            |
//! | stray stderr → JSON           | `LogFormat::Cloud`, server commands, unless `OXY_STDERR_CAPTURE=off` | none: every line, repeats collapsed |
//! | product observability spans   | `OXY_OBSERVABILITY_BACKEND` set | `oxy_observability` filter      |
//! | OpenTelemetry context: ids, `traceparent` | `serve` / `start` / `worker`, unless `OTEL_SDK_DISABLED` | `OXY_OTEL_FILTER` (default `info`) |
//! | OTLP traces (+ logs, opt-in)  | …and `OTEL_EXPORTER_OTLP_ENDPOINT` | same                            |
//!
//! The last two are different stores for different readers — the tenant's
//! Traces console versus the operator's HyperDX — and are configured
//! independently; see `oxy-telemetry`'s crate docs for the distinction.
//!
//! `RUST_LOG`, when set, replaces the stderr/file directives wholesale: the
//! expert escape hatch that also lifts the noisy-crate suppressions.

use std::env;
use std::io::IsTerminal;

use once_cell::sync::OnceCell;
use oxy::state_dir::get_state_dir;
use oxy_app::observability_boot;
use oxy_telemetry::otel::OtelConfig;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt};

static LOG_GUARD: OnceCell<tracing_appender::non_blocking::WorkerGuard> = OnceCell::new();

/// How many daily `oxy.log` files to keep on local installs before the oldest
/// is pruned. Bounds the on-disk log footprint for a long-lived local session.
const LOCAL_LOG_MAX_FILES: usize = 7;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, coloured on a TTY, plus the rotating file.
    Local,
    /// One JSON object per line for a log aggregator; no file.
    Cloud,
}

impl LogFormat {
    /// `OXY_LOG_FORMAT=json|text` wins; otherwise Kubernetes means Cloud.
    /// The override exists for a container that is not on Kubernetes but is
    /// still tailed by a collector — a Docker host, a CI box.
    pub fn detect() -> Self {
        match env::var("OXY_LOG_FORMAT").ok().as_deref().map(str::trim) {
            Some(v) if v.eq_ignore_ascii_case("json") => return LogFormat::Cloud,
            Some(v) if v.eq_ignore_ascii_case("text") => return LogFormat::Local,
            _ => {}
        }
        if env::var("KUBERNETES_SERVICE_HOST").is_ok() || env::var("KUBERNETES_PORT").is_ok() {
            LogFormat::Cloud
        } else {
            LogFormat::Local
        }
    }
}

/// The stderr/file directive string. `OXY_DEBUG=true` is a shortcut for
/// `OXY_LOG_LEVEL=debug`, so a developer gets verbose oxy output without
/// remembering the variable name; framework crates stay suppressed either
/// way. `RUST_LOG` bypasses all of it.
fn stderr_directives() -> String {
    if let Ok(rust_log) = env::var("RUST_LOG") {
        return rust_log;
    }
    let debug_mode = env::var("OXY_DEBUG")
        .as_deref()
        .unwrap_or("false")
        .eq_ignore_ascii_case("true");
    let level = if debug_mode {
        "debug".to_string()
    } else {
        env::var("OXY_LOG_LEVEL")
            .unwrap_or_else(|_| "warn".to_string())
            .to_lowercase()
    };
    oxy_telemetry::directives_for_level(&level)
}

/// `oxy.log` is a local-dev convenience — a developer running oxy on their
/// laptop can tail it. In cloud every process already ships its logs to the
/// cluster aggregator via stderr, so an extra on-disk file is pure waste: on
/// the serve/worker fleet the state dir is an emptyDir, and an unrotated
/// oxy.log grows until the kubelet evicts the pod under DiskPressure. So the
/// file exists on Local only — and is bounded even there with daily rotation
/// (`oxy.<YYYY-MM-DD>.log`) so a long-lived session cannot grow it without
/// limit.
fn local_file_writer() -> Option<tracing_appender::non_blocking::NonBlocking> {
    match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("oxy")
        .filename_suffix("log")
        .max_log_files(LOCAL_LOG_MAX_FILES)
        .build(get_state_dir())
    {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            LOG_GUARD.set(guard).ok();
            Some(writer)
        }
        Err(e) => {
            eprintln!("oxy: could not initialize oxy.log file appender: {e}");
            None
        }
    }
}

/// Install the subscriber. Returns anything that went wrong bringing up the
/// OTLP exporters, for the caller to log once `tracing` is live — a broken
/// collector configuration must never stop the process from starting.
///
/// `server_command` gates the stderr capture: it holds lines in a pipe until a
/// reader thread writes them, and only the server path drains that pipe at
/// exit (`oxy_telemetry::stderr_capture::finish` in `main`). A one-shot command
/// that calls `process::exit` from deep inside could lose its last lines.
pub fn init(observability_enabled: bool, otel: &OtelConfig, server_command: bool) -> Vec<String> {
    let log_format = LogFormat::detect();
    let directives = stderr_directives();
    // EnvFilter is not Clone; rebuild it from the same string per layer.
    let make_filter = || EnvFilter::new(&directives);

    let mut layers: Vec<BoxedLayer> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut json_dispatch: Option<oxy_telemetry::with_dispatch::DispatchHandle> = None;
    layers.push(Box::new(
        sentry::integrations::tracing::layer().with_filter(LevelFilter::WARN),
    ));

    match log_format {
        LogFormat::Local => {
            if let Some(writer) = local_file_writer() {
                layers.push(Box::new(
                    fmt::layer()
                        .with_target(true)
                        .with_level(true)
                        .with_writer(writer)
                        .with_ansi(false)
                        .with_filter(make_filter()),
                ));
            }
            // stderr is the conventional channel for diagnostics, so a CLI's
            // stdout stays available for piped program output. ANSI only on
            // an interactive TTY: captured stderr (Docker logs, a redirect,
            // CI) would otherwise fill with escape sequences. `.compact()`
            // drops the per-event repetition of the span-chain breadcrumb,
            // which embedded `oxy.sql=…` on every nested line.
            layers.push(Box::new(
                fmt::layer()
                    .compact()
                    .with_target(true)
                    .with_level(true)
                    .with_writer(std::io::stderr)
                    .with_ansi(std::io::stderr().is_terminal())
                    .with_filter(make_filter()),
            ));
        }
        LogFormat::Cloud => {
            // Flat JSON with trace ids — see `oxy_telemetry::json_format`.
            // The service name is stamped on each line so `kubectl logs` is
            // attributable without the collector's resource processing.
            let service = env::var("OTEL_SERVICE_NAME")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| oxy_telemetry::resource::service_name_for_role(otel.role));
            // Everything else that writes to fd 2 — airlayer's `eprintln!`
            // warnings, C libraries, the panic hook — is rewritten into the
            // same JSON shape by `stderr_capture`; our own layer then writes to
            // the original descriptor so its lines never queue behind the pipe.
            let capture = if server_command && oxy_telemetry::stderr_capture::enabled_by_env() {
                install_stderr_capture(&service, &mut problems)
            } else {
                None
            };
            let handle = match capture {
                Some(writer) => {
                    let (json, handle) = oxy_telemetry::json_format::layer(Some(service), writer);
                    layers.push(Box::new(json.with_filter(make_filter())));
                    handle
                }
                None => {
                    let (json, handle) =
                        oxy_telemetry::json_format::layer(Some(service), std::io::stderr);
                    layers.push(Box::new(json.with_filter(make_filter())));
                    handle
                }
            };
            json_dispatch = Some(handle);
        }
    }

    // OpenTelemetry BEFORE the product observability layer: layers see a new
    // span in this order, and the day `oxy-observability` adopts the OTel
    // trace id (so HyperDX's product-span source correlates with the logs —
    // see internal-docs/platform-telemetry.md) it has to find the id already
    // assigned. Harmless today.
    let otel_layers = oxy_telemetry::otel::layers::<Registry>(otel);
    layers.extend(otel_layers.layers);

    // Product observability: the SpanCollectorLayer is built here so startup
    // spans are captured, but its store is not ready yet (under `oxy start`
    // Postgres has not booted) — the receiver is stashed and `serve.rs` wires
    // the bridge once the DB URL is set. Its own filter keeps agent/automation
    // spans flowing regardless of OXY_LOG_LEVEL.
    if observability_enabled {
        let (layer, receiver) = oxy_observability::build_layer_and_receiver();
        observability_boot::stash_receiver(receiver);
        layers.push(Box::new(
            layer.with_filter(oxy_observability::observability_filter()),
        ));
    }

    tracing_subscriber::registry().with(layers).init();
    // Bind the JSON formatter's trace-id lookup to the installed subscriber.
    // `Vec<L>` does not forward `on_register_dispatch`, and this is the one
    // place outside a `tracing` callback where `get_default` is the real one.
    if let Some(handle) = json_dispatch {
        tracing::dispatcher::get_default(|dispatch| handle.bind(dispatch));
    }
    problems.extend(otel_layers.problems);
    problems
}

#[cfg(unix)]
fn install_stderr_capture(
    service: &str,
    problems: &mut Vec<String>,
) -> Option<oxy_telemetry::stderr_capture::StderrCapture> {
    match oxy_telemetry::stderr_capture::install(Some(service.to_string())) {
        Ok(capture) => Some(capture.writer()),
        Err(e) => {
            problems.push(format!("stderr capture not installed: {e}"));
            None
        }
    }
}

#[cfg(not(unix))]
fn install_stderr_capture(
    _service: &str,
    _problems: &mut Vec<String>,
) -> Option<fn() -> std::io::Stderr> {
    None
}
