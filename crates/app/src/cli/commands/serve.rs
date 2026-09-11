use crate::cli::ServeArgs;
use crate::server::api::custom_apps_functions::seam::FunctionQueryExecutor;
use crate::server::api::custom_apps_serve::wants_html;
use crate::server::api::projects::query::DataPlaneQueryExecutor;
use crate::server::http_cache::{if_none_match, weak_etag};
use agentic_pipeline::{AirwayMigrator, AnalyticsMigrator, AutomationMigrator};
use agentic_runtime::migration::RuntimeMigrator;
use axum::handler::Handler;
use axum::http::HeaderValue;
use axum::response::IntoResponse;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::get_service,
};
use include_dir::{Dir, include_dir};
use migration::{Migrator, MigratorTrait};
use oxy::{
    config::resolve_local_workspace_path,
    database::{client::establish_connection, docker},
    state_dir::get_state_dir,
    theme::StyledText,
};
use oxy_app_core::serve_mode::ServeMode;
use oxy_cameras::CamerasMigrator;
use oxy_shared::errors::OxyError;
use std::future::IntoFuture;
use std::net::SocketAddr;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tower::{ServiceBuilder, service_fn};
use tower_http::compression::CompressionLayer;
use tower_serve_static::ServeDir;
use utoipa_swagger_ui::SwaggerUi;

#[cfg(target_os = "windows")]
static DIST: Dir = include_dir!("D:\\a\\oxy\\oxy\\crates\\core\\dist");
#[cfg(not(target_os = "windows"))]
static DIST: Dir = include_dir!("$CARGO_MANIFEST_DIR/dist");
const ASSETS_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

pub async fn start_server_and_web_app(
    args: ServeArgs,
    extra_api_routes: Router<crate::server::router::AppState>,
    extra_api_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
    extra_workspace_routes: Router<crate::server::router::AppState>,
    extra_workspace_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
) -> Result<(), OxyError> {
    // OXY_ROLE → ide | serve | worker | all (default). Read once so the
    // routing middleware can enforce the FS-routing boundary.
    crate::server::role_manifest::init_process_role_from_env();

    if std::env::var("OXY_DATABASE_URL").is_err() {
        return Err(OxyError::RuntimeError(
            "OXY_DATABASE_URL environment variable is required.\n\n\
            Options:\n\
            1. Use 'oxy start' to automatically start PostgreSQL with Docker\n\
            2. Set OXY_DATABASE_URL to your PostgreSQL connection string:\n\
               export OXY_DATABASE_URL=postgresql://user:password@localhost:5432/oxy"
                .to_string(),
        ));
    }

    // Push transport, if one is configured. Registered here rather than lazily
    // so an operator learns at boot whether push is live — the whole point of
    // `Push::name` is that "not configured" and "configured and silent" are
    // distinguishable, and that is worth one log line at startup rather than
    // being inferred from the absence of notifications.
    if !crate::server::api::notifications::web_push::register_if_configured() {
        tracing::info!(
            "no push transport configured — notifications are inbox-only \
             (set OXY_VAPID_PRIVATE_KEY, OXY_VAPID_PUBLIC_KEY and OXY_VAPID_SUBJECT)"
        );
    }

    // In local mode, autodetect a running local Airhouse stack
    // (docker-compose.airhouse.yml) and inject the well-known AIRHOUSE_*
    // defaults so per-workspace provisioning works without manual env
    // configuration. Must run before AirhouseConfig is read below.
    if args.local {
        airhouse::config::autodetect_local_airhouse().await;
    }

    // Fail fast if Airhouse vars are partially set.
    {
        use airhouse::{AirhouseConfig, REQUIRED_VARS};
        match AirhouseConfig::from_env() {
            AirhouseConfig::Enabled(cfg) => {
                tracing::info!(
                    base_url = %cfg.base_url,
                    wire_host = %cfg.wire_host,
                    wire_port = cfg.wire_port,
                    "Airhouse integration enabled"
                );
            }
            AirhouseConfig::Disabled => {
                tracing::info!("Airhouse integration not configured");
            }
            AirhouseConfig::Misconfigured => {
                return Err(OxyError::RuntimeError(format!(
                    "Airhouse integration is partially configured — set ALL of the following or NONE:\n  {}",
                    REQUIRED_VARS.join(", ")
                )));
            }
        }
    }

    // Skip when a dedicated migrator already ran the schema (the Helm
    // pre-upgrade migrate Job, or the canonical StatefulSet migrator). The chart
    // sets `OXY_SKIP_MIGRATIONS=1` on the stateless serve fleet; honouring it
    // here avoids every serve pod re-running (and, pre-advisory-lock, racing)
    // the full migrator on boot. Unset → migrate as before (single-node / dev).
    if skip_migrations_requested() {
        println!("serve: OXY_SKIP_MIGRATIONS set — skipping migrations (run by the migrate Job)");
    } else {
        println!("serve: running database migrations");
        run_database_migrations(args.enterprise).await?;
    }
    println!("serve: migrations done, initializing feature flags");
    init_feature_flags().await?;
    println!("serve: feature flags initialized, seeding app admins from env");
    seed_app_admins_from_env().await?;
    println!("serve: app admins seeded, installing airway deployment tier");

    // airway's process-wide `GlobalConfig` (timeout / retries / user-agent /
    // TLS), from the singleton `airway_deployment_config` row. Installed once
    // here, at boot, rather than at the top of an airway run — because
    // `HttpConfig::default` and `RetryConfig::default` read a process-wide
    // `OnceLock`, so one install covers every connector this process builds,
    // including the ones that never go through a run: `POST /sources/discover`
    // (the create-pipeline wizard's table picker, which does connect to the
    // vendor) and the admin policy preview. Must precede
    // `create_web_application`, which mounts those routes and starts the
    // in-process worker fleet. Never fails boot — see `airway_boot`.
    crate::airway_boot::install_deployment_tier_from_env().await;
    println!("serve: airway deployment tier resolved, finding available port");

    // Now that OXY_CLICKHOUSE_* is set (either externally for `oxy serve` or
    // by `oxy start` once its ClickHouse container is ready), resolve the
    // observability backend and spawn the bridge that drains the span channel
    // into it. No-op when OXY_OBSERVABILITY_BACKEND is unset — the layer was
    // never installed and there's no receiver to drain.
    crate::observability_boot::finalize().await;

    // Spawn the 90-day retention sweep for custom-app usage tracking. Runs
    // every 6h; the first tick fires 60s after startup so migrations can
    // settle. See `custom_apps_tracking::spawn_retention_cleanup` for the
    // failure-handling rationale, and its module doc for why this one is an
    // app-level loop where observability leaves retention to ClickHouse.
    crate::server::api::custom_apps_tracking::spawn_retention_cleanup();

    // Detect whether any cloud auth provider is configured — used only to
    // surface an informational log when `--local` is requested with providers
    // present (the providers are ignored in local mode).
    let auth_configured = std::env::var("GOOGLE_CLIENT_ID").is_ok()
        || std::env::var("OKTA_CLIENT_ID").is_ok()
        || std::env::var("MAGIC_LINK_FROM_EMAIL").is_ok();

    // Tell `oxy-auth`'s built-in authenticator whether at least one provider
    // is configured in the parsed OxyConfig. Reads the YAML, not just the env
    // vars above (the YAML check is what `BuiltInAuthenticator` historically
    // used). Lifted here so `oxy-auth` can stay free of an `oxy` dep.
    //
    // In `--local` mode the authenticator must run guest-mode regardless of
    // what providers are configured — the local-mode router pins all callers
    // to the local guest user, and the customer-apps gates (which sit in the
    // shared public router, not the local-only protected stack) call
    // `BuiltInAuthenticator` directly without consulting `AuthState`. Without
    // this override they'd reject the implicit local-guest session and the
    // bundle's data hooks would 401 in `oxy start --local`.
    {
        let yaml_provider = !args.local
            && oxy::config::oxy::get_oxy_config()
                .ok()
                .and_then(|c| c.authentication)
                .map(|a| a.google.is_some() || a.okta.is_some() || a.magic_link.is_some())
                .unwrap_or(false);
        oxy_auth::built_in::set_auth_configured(yaml_provider);
    }

    // Ensure `$OXY_STATE_DIR/workspaces/` exists, migrating the legacy
    // "projects" directory on first boot. The canonical on-disk layout is
    // owned by `oxy::adapters::workspace::workspace_root_path`.
    {
        let state = get_state_dir();
        let legacy = state.join("projects");
        let root = state.join("workspaces");
        if legacy.exists()
            && !root.exists()
            && let Err(e) = std::fs::rename(&legacy, &root)
        {
            tracing::warn!(
                "Could not migrate workspaces directory {:?} → {:?}: {}",
                legacy,
                root,
                e
            );
        }
        std::fs::create_dir_all(&root).ok();
    }

    // Retrieve the global observability storage (if initialized) for the API handlers.
    let observability = oxy_observability::global::get_global().cloned();

    let _available_port = find_available_port(args.host.clone(), args.port).await?;
    let mode = if args.local {
        if auth_configured {
            tracing::info!(
                "--local: ignoring configured auth providers — all requests will run as the local guest user"
            );
        }
        match resolve_local_workspace_path() {
            Ok(path) => {
                tracing::info!("Local mode: workspace resolved to {}", path.display());
            }
            Err(e) => {
                tracing::info!(
                    "local mode: no workspace found ({}), waiting for setup via web UI",
                    e
                );
            }
        }
        // Seed the local-mode organization + guest membership so the per-org
        // Airhouse provision flow works without a real org picker. Best-effort:
        // failures are logged but don't block startup (Airhouse may not even
        // be configured).
        if let Err(e) =
            airhouse::ensure_local_org_seeded(oxy_app_core::serve_mode::LOCAL_WORKSPACE_ID).await
        {
            tracing::warn!("local-mode org seeding failed: {e}");
        }
        ServeMode::Local
    } else {
        ServeMode::Cloud
    };
    // Capture the mode process-wide so request-agnostic code (e.g. the app email
    // sender, which defaults to a browser preview locally instead of SES) can
    // read it without threading it through every call.
    oxy_app_core::serve_mode::set_process_mode(mode);

    if matches!(mode, ServeMode::Cloud) {
        use crate::integrations::slack::config::SlackConfig;
        match SlackConfig::from_env() {
            SlackConfig::Enabled(cfg) => {
                tracing::info!("Slack integration enabled");

                // Socket Mode: open a persistent WebSocket connection when an
                // app-level token is configured. HTTP webhooks remain active
                // regardless — Socket Mode is purely additive.
                if let Some(app_level_token) = cfg.app_level_token.clone() {
                    tracing::info!("slack: Socket Mode enabled (OXY_SLACK_APP_LEVEL_TOKEN set)");
                    tokio::spawn(async move {
                        crate::integrations::slack::socket_mode::run_socket_loop(app_level_token)
                            .await;
                    });
                } else {
                    tracing::info!("slack: using HTTP webhooks (OXY_SLACK_APP_LEVEL_TOKEN unset)");
                }

                // Background cleanup: delete stale Slack rows hourly.
                // - slack_oauth_states: TTL 15 min, but we keep rows 7 days for audit.
                // - slack_seen_events: keep rows 10 minutes (just enough for Slack retry window).
                tokio::spawn(async {
                    use chrono::Duration as CDuration;
                    use tokio::time::{Duration, sleep};
                    loop {
                        sleep(Duration::from_secs(3600)).await;
                        match crate::integrations::slack::oauth::state::sweep_expired().await {
                            Ok(n) if n > 0 => {
                                tracing::info!("cleaned up {n} expired slack_oauth_states rows")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("slack_oauth_states cleanup failed: {e}"),
                        }
                        match crate::integrations::slack::services::seen_events::SeenEventsService::sweep(
                            CDuration::minutes(10),
                        )
                        .await
                        {
                            Ok(n) if n > 0 => {
                                tracing::info!("cleaned up {n} old slack_seen_events rows")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("slack_seen_events cleanup failed: {e}"),
                        }
                    }
                });
            }
            SlackConfig::Disabled => {
                tracing::info!("Slack integration disabled via OXY_SLACK_ENABLED=false");
            }
            SlackConfig::Misconfigured => {
                tracing::warn!(
                    "Slack integration is misconfigured — one or more of \
                     OXY_SLACK_CLIENT_ID / OXY_SLACK_CLIENT_SECRET / \
                     OXY_SLACK_SIGNING_SECRET / OXY_SLACK_APP_BASE_URL is unset. \
                     Webhooks will return 503."
                );
            }
        }
    }

    let shutdown_token = CancellationToken::new();
    let startup_cwd = std::env::current_dir().map_err(|e| {
        OxyError::RuntimeError(format!("Failed to resolve startup working directory: {e}"))
    })?;

    let disable_inprocess_workers = args.workers_disabled();
    tracing::info!(
        role = crate::server::role_manifest::current_process_role().as_str(),
        in_process_workers = !disable_inprocess_workers,
        "serve: fleet config derived from OXY_ROLE (workers + global driver run \
         in-process for every role except `serve`; override with --no-workers / \
         OXY_DISABLE_INPROCESS_WORKERS / OXY_INPROC_GLOBAL_WORKER)"
    );
    // Loud, impossible-to-miss signal for the single-instance footgun: a node that
    // doesn't drain the queue leaves scheduled + manual jobs (and compiles)
    // queued forever unless another node drains them. Correct for a stateless
    // `serve` replica in a fleet; a mistake for a lone instance.
    if disable_inprocess_workers {
        tracing::warn!(
            role = crate::server::role_manifest::current_process_role().as_str(),
            "serve: this node will NOT drain the agentic_task_queue — scheduled \
             and manual jobs, ELT, and compiles will NOT execute here. Correct \
             for a stateless `serve` replica when another OXY_ROLE=all/ide/worker \
             node drains the queue. For a SINGLE-instance deployment, run \
             OXY_ROLE=all (the default) or set OXY_INPROC_GLOBAL_WORKER=1 — \
             otherwise jobs sit queued forever."
        );
    }

    // The internal API server (auth-disabled :3001) is an internal-auth MIRROR of
    // the main app — internal callers and the agentic browser tests drive it — so it
    // must mount the SAME extracted surface crates. Clone the seams before the main
    // app consumes them below; axum `Router` clones are cheap.
    let internal_extra_api = extra_api_routes.clone();
    let internal_extra_api_decls = extra_api_decls.clone();
    let internal_extra_workspace = extra_workspace_routes.clone();
    let internal_extra_workspace_decls = extra_workspace_decls.clone();

    let app = create_web_application(
        mode,
        args.enterprise,
        observability.clone(),
        startup_cwd.clone(),
        shutdown_token.clone(),
        disable_inprocess_workers,
        extra_api_routes,
        extra_api_decls,
        extra_workspace_routes,
        extra_workspace_decls,
    )
    .await?;

    let internal_app = if args.internal_port > 0 {
        Some(
            create_internal_application(
                args.enterprise,
                observability,
                shutdown_token.clone(),
                internal_extra_api,
                internal_extra_api_decls,
                internal_extra_workspace,
                internal_extra_workspace_decls,
            )
            .await?,
        )
    } else {
        println!("serve: internal port disabled (internal_port=0)");
        None
    };

    println!("serve: starting application");
    serve_application(app, internal_app, args, shutdown_token).await
}

async fn init_feature_flags() -> Result<(), OxyError> {
    // `cache::init` opens its own connection, wires the `oxy-oltp` flag bridge,
    // starts the refresh, and does the first load. FAIL-FAST here (`?`): an
    // unloaded cache reads the registry default for `billing` (OFF = paywall
    // skipped for every org), so serve must not accept requests with an unknown
    // billing state. The worker discards this same error because it enforces no
    // paywall and reads only `oltp`, whose unloaded value is already safe.
    crate::server::feature_flags::cache::init().await
}

/// One-shot bootstrap: if `OXY_GLOBAL_ADMINS` is set, ensure each email is
/// present in the `app_admins` table. After this point the env var is
/// ignored — admins are managed through the OXY_OWNER admin UI. Safe to
/// re-run on every startup; existing rows are left alone.
async fn seed_app_admins_from_env() -> Result<(), OxyError> {
    let db = establish_connection()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("Failed to connect to database: {}", e)))?;
    crate::server::api::custom_apps_auth::bootstrap_app_admins_from_env(&db)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("app admin seed failed: {}", e)))?;
    Ok(())
}

/// A fixed, process-independent key for the Postgres session-level advisory lock
/// that serialises startup migrations. Every oxy process uses this same key, so
/// concurrent migrators (the split fleet starting the ide + serve nodes
/// together, or `oxy start` bringing up serve + a worker) take turns instead of
/// racing the non-idempotent `CREATE TYPE` enum DDL — which otherwise crashes one
/// node with "duplicate key value violates unique constraint
/// pg_type_typname_nsp_index". The value is arbitrary; it only has to be
/// distinct from the OTHER advisory keys oxy uses in the SAME single-bigint
/// space — Postgres does not separate the session (`pg_advisory_lock`) and
/// transaction (`pg_try_advisory_xact_lock`) variants into different
/// namespaces. The per-workspace lazy-compile lock in
/// `server/api/middlewares/workspace_context.rs` is the only other user; it
/// keys off `workspace_id & 0x7fff_ffff_ffff_ffff`, so a workspace UUID whose
/// low 63 bits equal this key COULD collide (~1-in-2^63). The collision is
/// benign: a lazy-compile that loses this lock during the brief boot-migration
/// window just skips its non-blocking dedup and re-attempts on the next
/// request — no lost work. Pick future advisory keys deliberately rather than
/// assuming this space is otherwise empty.
const MIGRATION_ADVISORY_LOCK_KEY: i64 = 0x0078_795F_6D69_6772; // "\0xy_migr"

/// True when `OXY_SKIP_MIGRATIONS` is set. The long-lived pods — serve, compile,
/// worker — consult this so a dedicated migrate Job / the canonical StatefulSet
/// migrator owns the schema and the rest skip on boot (uniform intent across the
/// fleet). `oxy migrate` itself NEVER consults this — it always runs the full
/// migrator, since it IS the dedicated migrator.
pub(crate) fn skip_migrations_requested() -> bool {
    std::env::var("OXY_SKIP_MIGRATIONS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub(crate) async fn run_database_migrations(_enterprise: bool) -> Result<(), OxyError> {
    println!("migrations: establishing database connection (this builds the connection pool)");
    let db = establish_connection()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("Failed to connect to database: {}", e)))?;

    // Serialise migrations across processes with a session-level advisory lock.
    // Multiple oxy nodes booting together (the split fleet, or `oxy start`'s
    // serve + worker) would otherwise run these migrators concurrently, and the
    // `CREATE TYPE` enum migrations are NOT concurrency-safe → a duplicate-key
    // crash on `pg_type`. Holding the lock makes the losers WAIT, then find every
    // migration already applied (a no-op). The lock is held on one dedicated
    // pooled connection (pool max is 80, so it never starves the migrators) and
    // released even when a migrator fails, so a failure can't wedge other nodes
    // behind a stuck lock. A process that dies mid-migration drops its
    // connection, and Postgres releases the lock on disconnect too.
    let mut lock_conn = db
        .get_postgres_connection_pool()
        .acquire()
        .await
        .map_err(|e| {
            OxyError::RuntimeError(format!(
                "migrations: failed to acquire lock connection: {e}"
            ))
        })?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATION_ADVISORY_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await
        .map_err(|e| {
            OxyError::RuntimeError(format!("migrations: failed to take advisory lock: {e}"))
        })?;
    println!("migrations: advisory lock held, running SeaORM migrations");

    let result = run_all_migrators(&db).await;

    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATION_ADVISORY_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await
    {
        tracing::warn!("migrations: failed to release advisory lock (frees on disconnect): {e}");
    }

    result
}

/// Run every domain's migrator in dependency order. The caller holds the
/// migration advisory lock — see [`run_database_migrations`].
async fn run_all_migrators(db: &sea_orm::DatabaseConnection) -> Result<(), OxyError> {
    // Run SeaORM migrations for PostgreSQL
    Migrator::up(db, None)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("Failed to run database migrations: {}", e)))?;
    println!("migrations: SeaORM migrations complete");

    // Run orchestrator runtime migrations (separate tracking table).
    RuntimeMigrator::up(db, None)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("runtime migrations failed: {}", e)))?;
    println!("migrations: runtime migrations complete");

    // Run analytics domain extension migrations (separate tracking table).
    AnalyticsMigrator::up(db, None)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("analytics migrations failed: {}", e)))?;
    println!("migrations: analytics migrations complete");

    // Run automation state migrations (separate tracking table).
    AutomationMigrator::up(db, None)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("workflow migrations failed: {}", e)))?;
    println!("migrations: workflow migrations complete");

    // Run airway extension migrations (separate tracking table). Must
    // follow the runtime migrator — `airway_run_extensions.run_id` FKs
    // to `agentic_runs.id`.
    AirwayMigrator::up(db, None)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("airway migrations failed: {}", e)))?;
    println!("migrations: airway migrations complete");

    // Run airhouse migrations (separate tracking table). The wrapper pre-stamps
    // `seaql_migrations_airhouse` from the central tracking table so existing
    // deployments don't re-run migrations whose tables already exist.
    airhouse::migration::up(db)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("airhouse migrations failed: {}", e)))?;
    println!("migrations: airhouse migrations complete");

    // Per-org OLTP migrations (separate tracking table). Must follow the
    // central migrator — `oltp_tenants.org_id` FKs to `organizations.id`.
    oxy_oltp::migration::up(db)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("oltp migrations failed: {}", e)))?;
    println!("migrations: oltp migrations complete");

    // Camera fleet domain migrations (separate tracking table). No FK deps on
    // other domains — sites.workspace_id is a loose UUID per
    // domain-boundaries.md P3.
    //
    // We intentionally do NOT register a post-provision hook for the
    // cameras DDL: provisioning Airhouse is "this workspace wants a
    // data warehouse" — not "this workspace wants cameras". Eagerly
    // creating `oxy_cam_*` tables in every tenant would clutter
    // warehouses that never touch the camera fleet. Instead, the DDL
    // fires lazily at camera-intent points (see
    // `oxy_cameras::service::onboarding::import` and
    // `oxy_cameras::service::registration::register_edge_box`), with
    // the lazy ensure on the ingest path
    // (`oxy_cameras::airhouse::connect_and_ensure`) as a final safety
    // net.
    CamerasMigrator::up(db, None)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("cameras migrations failed: {}", e)))?;
    println!("migrations: cameras migrations complete");

    // Observability schema (DuckDB / Postgres / ClickHouse) is initialized by
    // the backend itself during `*Storage::open()` in `main.rs`, so no separate
    // migration step is needed here.

    Ok(())
}

async fn find_available_port(host: String, port: u16) -> Result<u16, OxyError> {
    let original_web_port = port;
    let mut chosen_port = port;
    let mut port_attempts = 0u16;
    const MAX_PORT_ATTEMPTS: u16 = 100;

    loop {
        let trial = format!("{host}:{chosen_port}");
        match trial.parse::<SocketAddr>() {
            Ok(addr) => {
                match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => {
                        // Successfully bound to the port: close listener and use this port
                        drop(listener);
                        break;
                    }
                    Err(e) => {
                        if chosen_port <= 1024 && e.kind() == std::io::ErrorKind::PermissionDenied {
                            // Returned, not `exit(1)`-ed, so `main` still flushes the
                            // platform-telemetry exporters — see the `Serve` arm in `mod.rs`.
                            return Err(OxyError::RuntimeError(format!(
                                "Permission denied binding to port {chosen_port}. Try running with sudo or use a port above 1024."
                            )));
                        }
                        port_attempts += 1;
                        if port_attempts > MAX_PORT_ATTEMPTS {
                            return Err(OxyError::RuntimeError(format!(
                                "Failed to bind to any port after trying {port_attempts} ports starting from {original_web_port}. Error: {e}"
                            )));
                        }
                        println!("Port {chosen_port} is occupied. Trying next port...");
                        chosen_port += 1;
                    }
                }
            }
            Err(_) => {
                // If parse fails, fall back to binding to unspecified address
                break;
            }
        }
    }
    Ok(chosen_port)
}

async fn create_web_application(
    mode: ServeMode,
    enterprise: bool,
    observability: Option<std::sync::Arc<dyn oxy_observability::ObservabilityStore>>,
    startup_cwd: std::path::PathBuf,
    shutdown_token: CancellationToken,
    disable_inprocess_workers: bool,
    extra_api_routes: Router<crate::server::router::AppState>,
    extra_api_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
    extra_workspace_routes: Router<crate::server::router::AppState>,
    extra_workspace_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
) -> Result<Router, OxyError> {
    let (api_router, external_api_router, preagg_ctx) = crate::server::router::api_router(
        mode,
        enterprise,
        observability,
        startup_cwd,
        shutdown_token,
        disable_inprocess_workers,
        extra_api_routes,
        extra_api_decls,
        extra_workspace_routes,
        extra_workspace_decls,
    )
    .await
    .map_err(|e| OxyError::RuntimeError(format!("Failed to create API router: {}", e)))?;
    // Assembled by `router::openapi` so `oxyc openapi` serves the exact
    // same document offline — one spec, two consumers.
    let openapi_doc = crate::server::router::build_openapi_doc().await;
    println!("create_web_application: openapi_router done, assembling final router");
    let static_service = service_fn(handle_static_files);

    use crate::server::api::custom_apps_serve;
    use axum::routing::any;

    // Everything that must be subject to the host-based subdomain rewrite:
    // the data API, the custom-app serve route, the SwaggerUI tree, and
    // the static admin-SPA fallback. Global CORS + trace are applied here so
    // they wrap this whole surface.
    let main = Router::new()
        .nest("/api", api_router)
        // Customer-app subpath bundle serving. Mounted at the top level (NOT
        // under /api) because the URL is browser-facing and must redirect to
        // /login on auth failure rather than return 401. The handler
        // authenticates inline and serves the bundle from disk OR (for
        // remote-hosted source) reverse-proxies to the upstream — so we
        // accept every method (POST form submits, Next.js Server Actions,
        // bundle-internal `/api/*` fetches), not just GET.
        //
        // 32 MiB body ceiling to match the proxy's in-handler cap; without
        // this layer axum's 2 MiB default rejects larger requests long
        // before the proxy sees them.
        .route(
            "/customer-apps/{*path}",
            // brotli/gzip for bundle bytes. DefaultPredicate skips
            // text/event-stream (the /fn SSE stream) and already-encoded
            // bodies (the V0 proxy), so streaming is unaffected.
            any(custom_apps_serve::serve_dispatch).layer(
                ServiceBuilder::new()
                    .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
                    .layer(CompressionLayer::new())
                    // BENEATH compression, not only on the outer router: by the
                    // time a response climbs past `CompressionLayer` its body is
                    // gzip/br with no exact size, and `record_error_body` skips
                    // anything without one — so the outer mount can never see a
                    // custom-app error body over 32 bytes. The request span is
                    // still current here (the trace layer wraps routing).
                    .layer(axum::middleware::from_fn(
                        oxy_telemetry::http_trace::record_error_body,
                    ))
                    // Inject the function query executor (the shared data plane)
                    // so `custom_apps_functions` runs `ctx.query` through the
                    // trait without importing `projects::query`.
                    .layer(axum::Extension(std::sync::Arc::new(DataPlaneQueryExecutor)
                        as std::sync::Arc<dyn FunctionQueryExecutor>))
                    // Inject the Layer-1 preagg cache the API router owns, so a
                    // function's `ctx.semantic` compiles through the same
                    // rollup-aware path as `/api/projects/{id}/semantic-query`.
                    // This route is mounted outside the API router and so has no
                    // `AppState` to read it from; the extension is the same seam
                    // the query executor already uses.
                    .layer(axum::Extension(preagg_ctx)),
            ),
        )
        .merge(
            SwaggerUi::new("/apidoc")
                .url("/apidoc/openapi.json", openapi_doc)
                .config(
                    utoipa_swagger_ui::Config::new(["/apidoc/openapi.json"])
                        .persist_authorization(true)
                        .deep_linking(true)
                        .display_request_duration(true)
                        .try_it_out_enabled(true),
                ),
        )
        .fallback_service(static_service)
        // CORS is applied to the OUTER router so every response —
        // including 404s from the fallback_service, /customer-apps/
        // serve handler, and the SwaggerUI tree — carries CORS
        // headers. Applying it only to api_router (the previous
        // default inside `finalize_router`) meant a missed /api/
        // prefix from a bundle SDK produced a CORS-less 404 from the
        // static fallback, which the browser surfaces as a misleading
        // "CORS error" instead of the real 404. See
        // `crates/app/src/server/router/mod.rs::build_cors_layer`.
        // role_middleware lives on the OUTER router so it covers the static
        // `/ide*` SPA fallback in addition to `/api/*`. Compares the request
        // URI segment-by-segment to the role manifest; no-op when OXY_ROLE is
        // unset. Stamps X-Oxy-Served-By on every response.
        .layer(axum::middleware::from_fn(
            crate::server::role_middleware::enforce_role,
        ))
        // Admission control sits OUTSIDE enforce_role (sheds before routing /
        // proxy work) but INSIDE CORS (so the 503 still carries CORS headers).
        // Above the in-flight ceiling it returns 503 + Retry-After so a spike
        // backs off instead of cascading across the fleet; probes are exempt.
        .layer(axum::middleware::from_fn(
            crate::server::admission::admission_control,
        ))
        .layer(crate::server::router::build_cors_layer())
        // Innermost of the trace pair: the request span must be current when
        // it records a failed response's own message as `error.message`.
        .layer(axum::middleware::from_fn(
            oxy_telemetry::http_trace::record_error_body,
        ))
        .layer(create_trace_layer());

    // Subdomain-based dispatch for custom-app bundles. Rewrites a
    // `<org>--<slug>.customer-apps[-env].<zone>` Host to the equivalent
    // `/customer-apps/<org>/<slug>/...` path so the route above takes over
    // (including the reverse-proxy path for remote-hosted sources). `/api/*`
    // requests are passed through unchanged so a bundle SDK's
    // `fetch("/api/...")` lands on the data API, not the upstream.
    //
    // CRITICAL: this wraps the WHOLE `main` router as a single service so it
    // runs BEFORE routing. Attaching it with `Router::layer` instead runs it
    // AFTER routing — axum applies `.layer` per-endpoint (each route AND the
    // fallback individually) and matches the path first, so a fallback-bound
    // request (`/`, `/foo.js`, `/_next/...`) is already routed to the static
    // admin SPA before the rewrite runs; the rewritten path then goes
    // straight to the static file server and the custom app never loads
    // (the staging/prod "blank page" bug). Regression test:
    // `custom_apps_host_dispatch::subdomain_rewrite_must_run_before_routing`.
    // Two host-dispatch layers run before routing. The customer-apps rewrite
    // is first (it owns the structural `<org>--<slug>.customer-apps…` hosts);
    // the org-subdomain dispatch is second (bare `<org>.<zone>` hosts → org
    // scoping, `/a/<slug>/` app rewrite, centralized-auth bounce). On any host
    // neither matches, both fall through untouched.
    let main = ServiceBuilder::new()
        .layer(axum::middleware::from_fn(
            oxy_app_core::custom_apps_host_dispatch::subdomain_rewrite_middleware,
        ))
        .layer(axum::middleware::from_fn(
            oxy_app_core::org_host_dispatch::org_host_dispatch_middleware,
        ))
        .service(main);

    // External API surface — a sibling of `main`, so it is NOT wrapped by the
    // global CORS/trace layers OR the subdomain rewrite. It carries its OWN
    // wide-open CORS (`build_external_cors_layer`) and is API-key-only; its
    // callers use the admin host, never a custom-app subdomain. Every other
    // path falls through to `main`.
    let router = Router::new()
        // `enforce_role` also wraps the external surface. It mounts the SAME
        // handlers as `/api` under a different prefix, and being a sibling of
        // `main` it was outside the role middleware entirely — so every external
        // route was unclassified, hence FleetOk, hence answered by whichever pod
        // the LB picked. `classify` normalises the `/external/api` prefix, so one
        // manifest entry governs both surfaces.
        .nest(
            "/external/api",
            external_api_router
                .layer(axum::middleware::from_fn(
                    crate::server::role_middleware::enforce_role,
                ))
                .layer(axum::middleware::from_fn(
                    oxy_telemetry::http_trace::record_error_body,
                ))
                // Being a sibling of `main`, this surface would otherwise be
                // the one API tree with no request span in HyperDX.
                .layer(create_trace_layer()),
        )
        .fallback_service(main)
        // OUTERMOST of everything, so `x-oxy-request-id` is minted exactly once
        // and covers BOTH surfaces. Mounting it on `main` instead would miss
        // `/external/api`, which is a sibling rather than a child — the same
        // trap that left every external route unclassified for `enforce_role`
        // (see the note above). Being outside CORS/trace/admission also means a
        // preflight, a shed 503 and a static-fallback 404 all carry the header,
        // none of which reach a handler that could stamp it.
        .layer(axum::middleware::from_fn(
            crate::server::api::middlewares::request_id::request_id_middleware,
        ));
    Ok(router)
}

async fn create_internal_application(
    enterprise: bool,
    observability: Option<std::sync::Arc<dyn oxy_observability::ObservabilityStore>>,
    shutdown_token: CancellationToken,
    extra_api_routes: Router<crate::server::router::AppState>,
    extra_api_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
    extra_workspace_routes: Router<crate::server::router::AppState>,
    extra_workspace_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
) -> Result<Router, OxyError> {
    let internal_router = crate::server::router::internal_api_router(
        enterprise,
        observability,
        shutdown_token,
        extra_api_routes,
        extra_api_decls,
        extra_workspace_routes,
        extra_workspace_decls,
    )
    .await
    .map_err(|e| OxyError::RuntimeError(format!("Failed to create internal API router: {}", e)))?;

    let static_service = service_fn(handle_static_files);

    Ok(Router::new()
        .nest("/api", internal_router)
        .fallback_service(static_service)
        .layer(axum::middleware::from_fn(
            oxy_telemetry::http_trace::record_error_body,
        ))
        .layer(create_trace_layer()))
}

/// One `SERVER` span per request, named by route, with the OTel HTTP semantic
/// conventions and the inbound `traceparent` as parent — see
/// `oxy_telemetry::http_trace`. The request-id header name is passed in so
/// that crate needs no `oxy-shared` edge; the middleware minting it runs
/// OUTSIDE this layer (it is the outermost layer of the top router), so the
/// id is already on the request when the span is made.
fn create_trace_layer() -> oxy_telemetry::http_trace::OxyTraceLayer {
    oxy_telemetry::http_trace::trace_layer(
        crate::server::api::middlewares::request_id::REQUEST_ID_HEADER,
    )
}

async fn handle_static_files(
    req: Request<Body>,
) -> Result<axum::response::Response, std::convert::Infallible> {
    // The org-subdomain dispatch middleware attaches this for product routes
    // on a bare org host; when present we splice `window.__OXY_ORG__` into the
    // served index.html so the SPA boots pre-scoped to the org + default
    // project (and skips the org/workspace picker).
    let org_ctx = req
        .extensions()
        .get::<oxy_app_core::org_host_dispatch::OrgSubdomainCtx>()
        .cloned();
    let request_headers = req.headers().clone();
    let uri = req.uri().clone();
    let mut response = get_service(ServeDir::new(&DIST))
        .call(req, None::<()>)
        .await;

    // Only on a hit. Stamping `immutable` on a miss would pin the 404 in the
    // browser cache for a year — and misses under `/assets/` are exactly what
    // a client holding a stale index.html generates.
    if uri.path().starts_with("/assets/") && response.status().is_success() {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(ASSETS_CACHE_CONTROL),
        );
    }

    if response.status() == StatusCode::NOT_FOUND {
        // Two independent gates, OR'd so neither can regress the other: what
        // the client asked for (`Accept`, precise but absent on some clients)
        // and what the path looks like (a suffix, coarse but header-free).
        if !wants_html(&request_headers) || is_static_file_request(uri.path()) {
            return Ok(response);
        }
        let index_request = Request::builder()
            .uri("/index.html")
            .body(Body::empty())
            .unwrap();
        response = get_service(ServeDir::new(&DIST))
            .call(index_request, None::<()>)
            .await;
    }

    Ok(finalize_html(response, org_ctx.as_ref(), &request_headers).await)
}

/// `Cache-Control` for the SPA shell.
///
/// `no-cache` means "you may store this, but revalidate before reusing it" —
/// not "don't store it". That distinction is the whole fix. Previously the
/// shell went out with *no* cache directives at all: this handler set a header
/// only for `/assets/*`, and `tower-serve-static` emits nothing but
/// `Content-Type` (its `Last-Modified` lives behind a non-default `metadata`
/// feature we don't enable). With no directive and no validator, a browser was
/// free to replay a stored copy on a back/forward navigation without ever
/// asking us. After a deploy rotates the chunk hashes, that copy names files
/// which no longer exist, every one 404s, and the SPA never boots.
///
/// Deliberately NOT `no-store`: that would make the page ineligible for the
/// back/forward cache, turning every Back into a full reload. `no-cache`
/// governs the HTTP cache only and leaves bfcache — the fast path — alone.
const HTML_CACHE_CONTROL: &str = "no-cache";

/// Cap on the shell body we'll buffer to hash. The shell is a few KB; this is
/// a backstop, not a budget.
const HTML_BUFFER_LIMIT: usize = 16 * 1024 * 1024;

/// Extensions that mark a request as a request for a *file*, so a miss must
/// answer 404 rather than fall through to the SPA shell.
const STATIC_FILE_EXTENSIONS: &[&str] = &[
    "js",
    "mjs",
    "cjs",
    "css",
    "map",
    "wasm",
    "json",
    "html",
    "htm",
    "svg",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "avif",
    "ico",
    "woff",
    "woff2",
    "ttf",
    "otf",
    "eot",
    "txt",
    "xml",
    "webmanifest",
    "mp4",
    "webm",
];

/// True when `path` addresses a static file rather than a client route.
///
/// The SPA fallback exists so client-routed URLs (`/threads/<id>`, `/ide`)
/// load the shell. A request for a file is not a client route, and answering
/// its miss with `index.html` is actively harmful: the browser gets `text/html`
/// for a `<script type="module">`, strict MIME checking blocks it, and the page
/// goes blank with nothing the app can react to — the `vite:preloadError`
/// handler that would self-heal lives inside the bundle that just got blocked.
/// A real 404 keeps that failure loud and recoverable.
///
/// The *primary* gate is `wants_html` — a subresource announces itself in
/// `Accept` whether or not its URL carries a suffix, which is what catches an
/// extensionless `fetch("/some/data")`. This is the second gate, for clients
/// that send no usable `Accept` at all (where `wants_html` deliberately
/// defaults to "yes" so the SPA still renders for `curl`).
///
/// Matched by extension allowlist rather than "has a dot" so a client route
/// that happens to contain one keeps rendering the shell.
fn is_static_file_request(path: &str) -> bool {
    if path.starts_with("/assets/") {
        return true;
    }
    let last_segment = path.rsplit('/').next().unwrap_or_default();
    let Some((_, ext)) = last_segment.rsplit_once('.') else {
        return false;
    };
    STATIC_FILE_EXTENSIONS
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known))
}

/// Apply the shell's caching contract to an HTML response, passing anything
/// else through untouched.
///
/// Org injection happens first so the ETag covers the bytes we actually ship —
/// two org subdomains serve different shells from the same file and must not
/// share a validator.
async fn finalize_html(
    response: axum::response::Response,
    org_ctx: Option<&oxy_app_core::org_host_dispatch::OrgSubdomainCtx>,
    request_headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if !is_html {
        return response;
    }

    let response = match org_ctx {
        Some(ctx) => oxy_app_core::org_host_dispatch::inject_org_into_response(response, ctx).await,
        None => response,
    };

    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, HTML_BUFFER_LIMIT).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("static: failed to buffer html shell: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let etag = weak_etag(&bytes);
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(HTML_CACHE_CONTROL),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        parts.headers.insert(header::ETAG, value);
    }
    // Set from the body below; a value carried over from the pre-injection
    // response would be wrong.
    parts.headers.remove(header::CONTENT_LENGTH);

    if if_none_match(request_headers, &etag) {
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(header::CONTENT_TYPE);
        return axum::response::Response::from_parts(parts, Body::empty());
    }

    axum::response::Response::from_parts(parts, Body::from(bytes))
}

/// How long the plain-HTTP path lets in-flight requests drain after the
/// shutdown signal before it drops the serve future and moves on.
///
/// `axum::serve(...).with_graceful_shutdown(..)` otherwise waits for in-flight
/// requests *indefinitely*, so a single handler wedged on a 30s DB pool acquire
/// would delay the whole shutdown ~30s before `await_shutdown_hooks` even runs.
/// Bounding it keeps total shutdown ≈ this deadline + the 10s hook budget,
/// comfortably inside a default 30s Kubernetes grace period. The TLS path uses
/// `axum_server`'s immediate `handle.shutdown()` and needs no equivalent.
const GRACEFUL_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

async fn serve_application(
    app: Router,
    internal_app: Option<Router>,
    args: ServeArgs,
    shutdown_token: CancellationToken,
) -> Result<(), OxyError> {
    // Both serve branches below consume `shutdown_token`; keep a handle so the
    // shutdown-hook wait can tell a signalled shutdown from an error unwind.
    let shutdown_observer = shutdown_token.clone();

    let socket_addr = format!("{}:{}", args.host, args.port)
        .parse()
        .or_else(|_| Ok(SocketAddr::from(([0, 0, 0, 0], args.port))))
        .map_err(|e: std::net::AddrParseError| {
            OxyError::RuntimeError(format!("Invalid address: {}", e))
        })?;

    let display_host = if args.host == "0.0.0.0" {
        "localhost"
    } else {
        &args.host
    };

    let protocol = if args.http2_only { "https" } else { "http" };
    let protocol_info = if args.http2_only {
        " (HTTP/2 only)"
    } else {
        " (HTTP/1.1+HTTP/2)"
    };
    println!(
        "{} {}{}",
        "Web app running at".text(),
        format!("{}://{}:{}", protocol, display_host, args.port).secondary(),
        protocol_info
    );

    // Auth bypass is opt-in and loud: if someone set it on a box that isn't
    // theirs alone, the startup banner is where they find out. Print the API
    // endpoint, not the SPA page — the page is served by the Vite dev server on
    // its own port under `pnpm run dev`, and isn't mounted at all in local
    // mode, so this URL is the one that's always right. DEVELOPMENT.md owns
    // the browser flow.
    if crate::api::auth::is_dev_login_enabled() {
        // Name the variable that actually enabled it: on a debug build with no
        // explicit opt-in that is OXY_GLOBAL_ADMINS, and someone reading
        // "OXY_DEV_LOGIN_EMAILS" would go looking for a var they never set.
        let source = crate::api::auth::dev_login_source().unwrap_or("OXY_DEV_LOGIN_EMAILS");
        // "anyone who can reach this server" is only true for an explicit
        // allow-list; an inferred one is refused off-box, and saying otherwise
        // would train readers to ignore the louder message.
        let reach = if crate::api::auth::dev_login_is_loopback_only() {
            "this machine only (loopback) — set OXY_DEV_LOGIN_EMAILS to serve other hosts"
        } else {
            "anyone who can reach this server"
        };
        println!(
            "{} {}",
            format!("Dev sign-in ENABLED ({source}) — {reach}, via").warning(),
            format!(
                "{}://{}:{}/api/auth/dev-login",
                protocol, display_host, args.port
            )
            .secondary(),
        );
    }

    // Start internal server if enabled
    if let Some(internal_app) = internal_app {
        let internal_addr: SocketAddr = format!("{}:{}", args.internal_host, args.internal_port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                OxyError::RuntimeError(format!("Invalid internal address: {}", e))
            })?;

        let internal_display_host = if args.internal_host == "0.0.0.0" {
            "localhost"
        } else {
            &args.internal_host
        };
        println!(
            "{} {}",
            "Internal API running at".text(),
            format!("http://{}:{}", internal_display_host, args.internal_port).secondary(),
        );

        let internal_listener =
            tokio::net::TcpListener::bind(internal_addr)
                .await
                .map_err(|e| {
                    OxyError::RuntimeError(format!(
                        "Failed to bind internal server to {}: {}",
                        internal_addr, e
                    ))
                })?;

        tokio::spawn(async move {
            // Connect-info, like both main-port branches: this router merges
            // `build_public_routes`, so it serves `/auth/dev-login`, whose
            // loopback check needs a peer address to be meaningful here.
            if let Err(e) = axum::serve(
                internal_listener,
                internal_app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                tracing::error!("Internal server error: {}", e);
            }
        });
    }

    let result = if args.http2_only {
        // If TLS cert/key files exist, use HTTPS+HTTP/2
        let cert_exists = std::path::Path::new(&args.tls_cert).exists();
        let key_exists = std::path::Path::new(&args.tls_key).exists();
        let config = if cert_exists && key_exists {
            tracing::info!("Using provided TLS cert/key files for HTTPS (TLS) and HTTP/2");
            match axum_server::tls_rustls::RustlsConfig::from_pem_file(
                &args.tls_cert,
                &args.tls_key,
            )
            .await
            {
                Ok(cfg) => cfg,
                Err(e) => {
                    // Returned, not `exit(1)`-ed, so `main` still flushes the
                    // platform-telemetry exporters — see the `Serve` arm in `mod.rs`.
                    return Err(OxyError::RuntimeError(format!(
                        "Failed to load TLS cert/key: {e}"
                    )));
                }
            }
        } else {
            tracing::warn!("No TLS cert/key files found, using bundled default cert/key.");
            let default_cert: &[u8] = include_bytes!("../../../../../localhost+2.pem");
            let default_key: &[u8] = include_bytes!("../../../../../localhost+2-key.pem");
            match axum_server::tls_rustls::RustlsConfig::from_pem(
                default_cert.to_vec(),
                default_key.to_vec(),
            )
            .await
            {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(OxyError::RuntimeError(format!(
                        "Failed to load bundled TLS cert/key: {e}"
                    )));
                }
            }
        };

        // Create handle for graceful shutdown with axum_server
        let handle = axum_server::Handle::new();

        // Spawn shutdown signal handler
        let shutdown_handle = handle.clone();
        let token = shutdown_token;
        tokio::spawn(async move {
            create_shutdown_signal().await;
            tracing::info!("Shutdown signal received, stopping server...");
            // Set the shutdown flag at signal time — before any background
            // loop does one more blocking DB poll — so `is_shutting_down()`
            // gates catch immediately and in-flight polls race the shutdown
            // signal. The shutdown hook also calls this (idempotent); doing it
            // here is what makes the gates prompt. Mirrors `oxy worker`.
            agentic_runtime::transport::begin_shutdown();
            token.cancel();
            // `axum_server`'s `handle.shutdown()` is immediate (it stops
            // accepting and drops in-flight connections at once), so the TLS
            // path needs no separate drain deadline — unlike the plain-HTTP
            // `with_graceful_shutdown` path below.
            shutdown_handle.shutdown();
        });

        // ConnectInfo<SocketAddr> for downstream extractors (e.g. the
        // camera-fleet auth middleware uses it as the last-resort
        // peer-IP source when behind no XFF-setting proxy, i.e. a
        // Tailscale-native deploy where the edge dials Oxy directly).
        axum_server::bind_rustls(socket_addr, config)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .map_err(|e| OxyError::RuntimeError(format!("Server error: {}", e)))
    } else {
        let listener = tokio::net::TcpListener::bind(socket_addr)
            .await
            .map_err(|e| OxyError::RuntimeError(format!("Failed to bind to address: {}", e)))?;

        // A clone of the same token the shutdown future cancels below — the
        // drain deadline arm watches it so the clock starts at signal time,
        // not at server start.
        let drain_observer = shutdown_observer.clone();
        let shutdown = async move {
            create_shutdown_signal().await;
            // Set the shutdown flag at signal time (see the TLS branch note):
            // gates catch immediately, in-flight polls race the signal.
            agentic_runtime::transport::begin_shutdown();
            shutdown_token.cancel();
        };

        // See the matching note above the TLS branch — same
        // `ConnectInfo<SocketAddr>` plumbing for the plain HTTP path.
        let serve_fut = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .into_future();
        tokio::pin!(serve_fut);

        // Bound the in-flight-request drain: once the shutdown signal fires,
        // give handlers at most `GRACEFUL_DRAIN_DEADLINE`, then drop the serve
        // future and proceed to the (separately bounded) shutdown hooks. A
        // clean drain that finishes first still wins the select.
        tokio::select! {
            r = &mut serve_fut => {
                r.map_err(|e| OxyError::RuntimeError(format!("Server error: {}", e)))
            }
            _ = async {
                drain_observer.cancelled().await;
                tokio::time::sleep(GRACEFUL_DRAIN_DEADLINE).await;
            } => {
                tracing::warn!(
                    drain_secs = GRACEFUL_DRAIN_DEADLINE.as_secs(),
                    "graceful drain deadline reached; abandoning in-flight requests \
                     so shutdown stays inside the grace period"
                );
                Ok(())
            }
        }
    };

    // Both branches above return only after the listener has drained, which
    // is also after `shutdown_token` was cancelled — so the shutdown hooks
    // are already running. Wait for them (bounded) rather than letting
    // process exit race the claim release they perform.
    crate::server::router::recovery::await_shutdown_hooks(&shutdown_observer).await;

    result
}

async fn create_shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received shutdown signal, cleaning up...");
        },
        _ = terminate => {
            tracing::info!("Received termination signal, cleaning up...");
        },
    }

    // If the user presses Ctrl+C again while graceful shutdown is in progress,
    // force-exit immediately instead of showing ^C and hanging.
    tokio::spawn(async {
        signal::ctrl_c()
            .await
            .expect("failed to install second Ctrl+C handler");
        tracing::warn!("Received second shutdown signal, forcing exit");
        std::process::exit(1);
    });

    // Cleanup Docker containers (stop and remove all oxy-managed containers)
    docker::cleanup_containers().await;
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use axum::{Router, body::Body, routing::get};
    use tower::ServiceExt;
    use tower_http::compression::CompressionLayer;

    #[tokio::test]
    async fn custom_app_route_compresses_assets() {
        // A handler standing in for a JS asset response (>32 bytes so the
        // size predicate allows compression).
        async fn asset() -> ([(axum::http::HeaderName, &'static str); 1], String) {
            (
                [(axum::http::header::CONTENT_TYPE, "application/javascript")],
                "console.log('hello world from a custom app bundle');".repeat(4),
            )
        }
        let app = Router::new()
            .route("/x.js", get(asset))
            .layer(CompressionLayer::new());
        let res = app
            .oneshot(
                Request::get("/x.js")
                    .header("accept-encoding", "br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap()),
            Some("br"),
            "custom-app assets must be brotli-compressed"
        );
    }

    // ── SPA shell caching ────────────────────────────────────────────────
    //
    // Regression cover for the blank-page-on-back bug: a browser replayed a
    // stored index.html on a history navigation, its chunk hashes were stale
    // after a deploy, and every `/assets/*.js` miss came back as `text/html`.

    use super::{
        ASSETS_CACHE_CONTROL, HTML_CACHE_CONTROL, finalize_html, handle_static_files,
        is_static_file_request,
    };
    use crate::server::http_cache::weak_etag;
    use axum::http::{HeaderMap, StatusCode, header};
    use tower::service_fn;

    /// Drive the real handler, the way the router mounts it.
    async fn serve_get(uri: &str, accept: Option<&str>) -> axum::response::Response {
        let mut req = Request::get(uri);
        if let Some(accept) = accept {
            req = req.header(header::ACCEPT, accept);
        }
        Router::new()
            .fallback_service(service_fn(handle_static_files))
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    /// A navigation's `Accept`, as Chrome sends it.
    const NAV_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";

    // The bug this PR exists to prevent, asserted through the handler rather
    // than its helpers: a stale shell asks for a chunk that no longer exists,
    // and the answer must be a 404 — not the shell at 200, which the browser
    // blocks on MIME and cannot recover from.
    #[tokio::test]
    async fn missing_asset_404s_instead_of_returning_the_shell() {
        // `*/*` is what a `<script type="module">` sends.
        let res = serve_get("/assets/react-dom-vendor-VMPilgUW.js", Some("*/*")).await;

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_ne!(
            res.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.starts_with("text/html")),
            Some(true),
            "answering a module request with HTML is what blanks the page"
        );
        assert_ne!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(ASSETS_CACHE_CONTROL),
            "`immutable` on a miss would pin the 404 in the browser cache for a year"
        );
    }

    #[tokio::test]
    async fn extensionless_subresource_404s_too() {
        // No suffix to match on — only `Accept` distinguishes this from a
        // client route, which is why the two gates are OR'd.
        let res = serve_get("/some/sdk/data", Some("*/*")).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn client_route_still_gets_the_shell() {
        let res = serve_get("/threads/0effae8f-c261-4cd3-afb5", Some(NAV_ACCEPT)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(HTML_CACHE_CONTROL),
            "the shell must revalidate however it was reached"
        );
    }

    #[tokio::test]
    async fn client_route_gets_the_shell_without_an_accept_header() {
        // `wants_html` defaults to true when `Accept` is absent so curl and
        // header-less clients still render the SPA.
        let res = serve_get("/threads/0effae8f-c261-4cd3-afb5", None).await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    fn html_response(body: &'static str) -> axum::response::Response {
        axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "text/html")
            .body(Body::from(body))
            .unwrap()
    }

    #[test]
    fn asset_requests_never_fall_back_to_the_shell() {
        // The exact shape that blanked the page: a chunk from a stale shell.
        assert!(is_static_file_request(
            "/assets/react-dom-vendor-VMPilgUW.js"
        ));
        assert!(is_static_file_request("/assets/index-DRPa-Ace.css"));
        assert!(is_static_file_request("/favicon.ico"));
        assert!(is_static_file_request("/oxy-logo.svg"));
    }

    #[test]
    fn client_routes_still_fall_back_to_the_shell() {
        assert!(!is_static_file_request("/"));
        assert!(!is_static_file_request("/home"));
        assert!(!is_static_file_request("/threads/0effae8f-c261-4cd3-afb5"));
        assert!(!is_static_file_request("/ide"));
        // A dot in a client route is not an extension — matched by allowlist
        // precisely so this keeps rendering the SPA.
        assert!(!is_static_file_request("/apps/my.dashboard"));
    }

    #[tokio::test]
    async fn shell_is_served_revalidate_on_every_load() {
        let res = finalize_html(
            html_response("<html><head></head></html>"),
            None,
            &HeaderMap::new(),
        )
        .await;
        assert_eq!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .map(|v| v.to_str().unwrap()),
            Some(HTML_CACHE_CONTROL),
            "the shell must revalidate, or a back navigation replays a stale \
             copy whose chunk hashes no longer exist"
        );
        assert!(
            res.headers().contains_key(header::ETAG),
            "revalidation needs a validator or every navigation refetches the body"
        );
    }

    #[tokio::test]
    async fn unchanged_shell_revalidates_to_304() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            weak_etag(b"<html><head></head></html>").parse().unwrap(),
        );
        let res = finalize_html(html_response("<html><head></head></html>"), None, &headers).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn non_html_passes_through_untouched() {
        let res = axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "application/javascript")
            .body(Body::from("console.log(1)"))
            .unwrap();
        let res = finalize_html(res, None, &HeaderMap::new()).await;
        assert!(
            !res.headers().contains_key(header::CACHE_CONTROL),
            "hashed assets keep their own immutable policy"
        );
    }
}
