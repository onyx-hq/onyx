// A binary is its own crate root, so the `recursion_limit` in lib.rs does not
// apply here. Laying out the futures reached from `cli()` exceeds rustc's
// default query depth since SeaORM 2.0 deepened its query types.
#![recursion_limit = "256"]

// Dev-only dynamic linking (see `oxy-app-dylib` + `just dev-backend-dyn`).
// Forcing the dylib into the link (with `-C prefer-dynamic`) makes oxy-app's
// symbols — and its ~1.4 GB of static deps — resolve dynamically from
// liboxy_app_dylib.dylib instead of being re-linked into the binary every edit.
// `as _` because we import it purely for the link edge, not to use its items.
#[cfg(feature = "dev-dynamic")]
extern crate oxy_app_dylib as _;

mod logging;

use std::process::exit;

use dotenv::dotenv;
use human_panic::Metadata;
use human_panic::setup_panic;
use oxy::sentry_config;
use oxy::theme::StyledText;
use oxy_app::cli::commands::cli;
use oxy_app::observability_boot;
use oxy_telemetry::otel::OtelConfig;
use std::env;

/// The long-lived server command this invocation runs, if any. Read before
/// clap runs because the subscriber must exist before anything logs, and
/// matched anywhere in argv rather than as "the first non-flag": a global
/// value-taking flag (`oxy --output text serve`) would otherwise make the
/// value look like the command.
///
/// Only these three get the OpenTelemetry layer. A one-shot command
/// (`oxy publish`, `oxy run`) has no reader for its trace ids, and in a shell
/// that happens to carry `OTEL_EXPORTER_OTLP_ENDPOINT` — `.env` is
/// auto-loaded — it would pay the exporter's flush at every exit.
fn server_command(args: &[String]) -> Option<&str> {
    args.iter()
        .skip(1)
        .map(String::as_str)
        .find(|a| matches!(*a, "serve" | "start" | "worker"))
}

/// Raise the process's open-file-descriptor soft limit at startup.
///
/// macOS ships a default soft `RLIMIT_NOFILE` of 256. A busy oxy instance
/// (the warehouse + LLM HTTP clients, the embedded Postgres pool, and many
/// concurrent SSE streams from data-app dashboards) blows past that and the
/// server stops accepting connections with
/// `axum::serve::listener: accept error: Too many open files (os error 24)`.
/// We bump the soft limit toward the hard cap (clamped to a sane target that
/// stays under macOS's `kern.maxfilesperproc`) so the server has headroom
/// regardless of the shell/launcher it was started from. Best-effort: any
/// failure is logged and the process continues with the inherited limit.
#[cfg(unix)]
fn raise_fd_limit() {
    // 65536 is ample headroom for a single instance and stays well under the
    // macOS per-process kernel cap on default systems.
    const DESIRED: libc::rlim_t = 65_536;
    // SAFETY: plain libc rlimit syscalls on a zeroed struct; single-threaded
    // here (before the Tokio runtime is built).
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let target = if lim.rlim_max == libc::RLIM_INFINITY {
            DESIRED
        } else {
            std::cmp::min(DESIRED, lim.rlim_max)
        };
        if lim.rlim_cur >= target {
            return; // already sufficient
        }
        let prev = lim.rlim_cur;
        lim.rlim_cur = target;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) == 0 {
            tracing::debug!(from = prev, to = target, "raised RLIMIT_NOFILE soft limit");
        } else {
            tracing::warn!(
                current = prev,
                "could not raise RLIMIT_NOFILE; if you hit \"Too many open files\", \
                 raise it manually with `ulimit -n 65536` before starting oxy"
            );
        }
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

fn main() {
    dotenv().ok();
    let _sentry_guard = sentry_config::init_sentry();
    if _sentry_guard.is_none() {
        setup_panic!(
            Metadata::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                .authors("Robert Yi <robert@oxygen-hq.com>") // temporarily using Robert email here, TODO: replace by support email
                .homepage("github.com/oxy-hq/oxygen")
                .support(
                    "- For support, please email robert@oxygen-hq.com or contact us directly via Github."
                )
        );
    }

    // Parse args early to check for flags
    let args: Vec<String> = env::args().collect();

    // Check if --enterprise flag is present (gates the observability UI/routes)
    let enterprise_enabled = args.iter().any(|a| a == "--enterprise");

    // Observability is opt-in everywhere — including `--local`. ClickHouse is
    // the sole backend, so there is no embedded store to default to: enabling
    // it implies a running ClickHouse (`oxy start` boots the container when
    // the var is set). With `--enterprise` but no backend, we warn and run
    // with observability disabled — no data is recorded and the UI surfaces a
    // "not configured" banner.
    let observability_enabled = env::var_os("OXY_OBSERVABILITY_BACKEND").is_some();
    if enterprise_enabled && !observability_enabled {
        eprintln!(
            "{}",
            "Observability disabled: OXY_OBSERVABILITY_BACKEND is not set. \
             Set it to clickhouse — with OXY_CLICKHOUSE_URL pointing at your \
             instance, or under `oxy start`, which boots one — to record traces."
                .text()
        );
    }

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    // DO NOT USE #[tokio::main]
    // https://docs.sentry.io/platforms/rust/
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            // Install the subscriber: Sentry + stderr/file + (if enterprise)
            // the SpanCollectorLayer + (if an OTLP endpoint is configured) the
            // OpenTelemetry exporters. The observability *store* isn't wired
            // yet — the `oxy start` path boots its ClickHouse container and
            // only then are `OXY_CLICKHOUSE_*` set; `observability_boot::
            // finalize()` is called from `serve.rs` once that endpoint is
            // available. The OTel resource needs the fleet role now, before
            // clap has parsed anything, so it is read from OXY_ROLE + argv.
            let command = server_command(&args);
            let role =
                oxy_telemetry::resource::role_hint(command, env::var("OXY_ROLE").ok().as_deref());
            let mut otel = OtelConfig::from_env(role);
            if command.is_none() {
                otel.sdk_disabled = true;
            }
            let telemetry_problems = logging::init(observability_enabled, &otel, command.is_some());
            for problem in telemetry_problems {
                tracing::warn!(%problem, "platform telemetry degraded");
            }
            if otel.export_enabled() {
                tracing::info!(
                    traces_endpoint = otel.traces_endpoint.as_deref().unwrap_or_default(),
                    logs_endpoint = otel.logs_endpoint.as_deref().unwrap_or_default(),
                    traces = otel.traces_exported(),
                    logs = otel.logs_exported(),
                    filter = %otel.filter,
                    role = role.unwrap_or("none"),
                    "OTLP export enabled"
                );
            }

            // Give the server enough file-descriptor headroom before it binds
            // listeners / boots the embedded Postgres — macOS defaults to a
            // soft NOFILE of 256, which busy instances exhaust (EMFILE).
            raise_fd_limit();

            // Surface crates mounted by this composition root. `oxy-api-github`
            // is the first extracted sibling; more merge in as they're pulled
            // out of oxy-app. `cli` forwards these into `serve`'s `api_router`,
            // where they join the protected tree before the auth middleware.
            // Roles travel WITH the routes. `oxy-api-github` mounts a
            // Postgres-only surface, so the FleetOk default is the truth and it
            // declares nothing. `oxy-api-onboarding` clones a repository and
            // scaffolds `config.yml` onto node-local disk, and
            // `oxy-api-partner-console`'s create-org scaffolds the new org's
            // Default workspace — neither can take that default, and no type
            // gate inside oxy-app can see across the crate line to stop them.
            let exit_code = match cli(
                oxy_api_github::routes()
                    .merge(oxy_api_partner_console::routes())
                    .merge(oxy_api_onboarding::routes()),
                oxy_api_onboarding::route_roles()
                    .iter()
                    .chain(oxy_api_partner_console::route_roles())
                    .copied()
                    .collect(),
                oxy_api_onboarding::workspace_routes(),
                oxy_api_onboarding::workspace_route_roles().to_vec(),
            )
            .await
            {
                Ok(_) => 0,
                Err(e) => {
                    tracing::error!(error = %e, "Application error");
                    sentry_config::capture_error_with_context(&*e, "CLI execution failed");
                    eprintln!("{}", format!("{e}").error());
                    1
                }
            };

            observability_boot::shutdown().await;

            // Last: flush the OTLP exporters. Blocking, bounded, and off the
            // async thread so a slow collector cannot wedge the runtime.
            match tokio::task::spawn_blocking(oxy_telemetry::otel::shutdown).await {
                Ok(problems) => {
                    for problem in problems {
                        eprintln!("oxy: {problem}");
                    }
                }
                Err(e) => eprintln!("oxy: OTLP exporter shutdown did not complete: {e}"),
            }

            // Truly last: point fd 2 back at the real stderr and let the
            // capture thread write out whatever stray lines are still queued
            // (the `eprintln!`s just above included).
            oxy_telemetry::stderr_capture::finish(std::time::Duration::from_secs(2));

            if exit_code != 0 {
                exit(exit_code);
            }
        });
}
