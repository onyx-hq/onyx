mod admin;
mod agentic_cli;
mod airway;
mod app_manifest;
mod apps;
mod assume;
mod assume_org;
mod cameras;
pub mod clean;
mod compile;
mod env_url;
pub mod export_chart;
mod http;
mod init;
mod init_ci;
mod intent;
mod login;
mod looker;
mod make;
mod mcp;
mod migrate;
mod migrate_automations;
/// `pub` so integration tests can drive the seed directly (see
/// `tests/seed_example_app.rs`) instead of shelling out to the binary — the
/// seed is a fixture the tests are built on, not just a CLI command.
pub mod oltp;
mod proxy;
mod publish;
pub mod run;
pub mod seed;
mod seed_apps;
mod seed_compile;
mod seed_llm_keys;
mod seed_partners;
mod seed_platform_grants;
mod seed_storage;
mod seed_threads;
pub(crate) mod serve;
mod start;
mod status;
mod test;
mod worker;

use crate::cli::commands::mcp::{start_mcp_sse_server, start_mcp_stdio};
use crate::cli::commands::migrate::migrate;
use crate::cli::commands::migrate_automations::{MigrateAutomationsArgs, migrate_automations};
use crate::cli::commands::run::{RunArgs, handle_run_command};
use crate::server::service::retrieval::{ReindexInput, reindex};
use crate::server::service::sync::{SyncFilter, sync_databases};
use ::oxy::adapters::secrets::SecretsManager;
use ::oxy::adapters::workspace::builder::WorkspaceBuilder;
use ::oxy::config::model::AppConfig;
use ::oxy::config::test_config::TestFileConfig;
use ::oxy::config::*;
use ::oxy::sentry_config;
use ::oxy::theme::StyledText;
use ::oxy::theme::detect_true_color_support;
use ::oxy::theme::get_current_theme_mode;
use clap::CommandFactory;
use clap::Parser;
use make::handle_make_command;
use model::{Automation, Config};
use oxy_shared::errors::OxyError;
use serve::start_server_and_web_app;
use std::backtrace;
use std::error::Error;
use std::path::PathBuf;
use std::process::Command;
use std::process::exit;
use uuid::Uuid;

use init::init;

use ::oxy::config::WorkingCopy;
use dotenv;
use tracing::{debug, error};

#[derive(Parser, Debug)]
#[clap(
    author,
    version,
    long_version = if cfg!(debug_assertions) {
        Box::leak(format!(
            "version {}, built locally as debug, rust ver {}",
            env!("CARGO_PKG_VERSION"),
            rustc_version_runtime::version(),
        ).into_boxed_str()) as &'static str
    } else {
        Box::leak(format!(
            "version: {}\n\
            rust version: {}\n\
            commit: {commit_link}\n\
            workflow url: {workflow_link}\n",
            env!("CARGO_PKG_VERSION"),
            rustc_version_runtime::version(),
            commit_link = match (
                option_env!("GITHUB_SERVER_URL"),
                option_env!("GITHUB_REPOSITORY"),
                option_env!("GITHUB_SHA")
            ) {
                (Some(server), Some(repo), Some(sha)) => format!("{}/{}/commit/{} ({})", server, repo, sha, sha),
                _ => option_env!("GITHUB_SHA").unwrap_or("unknown").to_string(),
            },
            workflow_link = match (
                option_env!("GITHUB_SERVER_URL"),
                option_env!("GITHUB_REPOSITORY"),
                option_env!("GITHUB_RUN_ID")
            ) {
                (Some(server), Some(repo), Some(run_id)) => format!("{}/{}/actions/runs/{} ({})", server, repo, run_id, run_id),
                _ => option_env!("GITHUB_RUN_ID").unwrap_or("unknown").to_string(),
            },
        ).into_boxed_str()) as &'static str
    },
)]
struct Args {
    /// The question to ask or command to execute
    ///
    /// When no subcommand is provided, this input will be processed
    /// as a question for the default AI agent or as a query suggestion.
    #[clap(default_value = "")]
    input: String,

    /// Output format: 'text' (default) or 'code' for SQL
    ///
    /// Control how results are displayed in the terminal.
    /// Use 'code' for syntax-highlighted SQL output.
    #[clap(long, value_name = "FORMAT")]
    output: Option<String>,

    /// Subcommand to execute
    #[clap(subcommand)]
    command: Option<SubCommand>,
}

#[derive(Parser, Debug)]
struct McpArgs {
    #[clap(subcommand)]
    pub transport: McpTransport,
}

#[derive(Parser, Debug)]
enum McpTransport {
    /// Start MCP server with stdio transport
    ///
    /// Launch an MCP server using standard input/output for direct
    /// integration with local AI tools and development environments.
    Stdio {
        /// Path to the Oxy project directory (required)
        ///
        /// Specify the root directory of your Oxy project where
        /// config.yml and other project files are located.
        workspace_path: PathBuf,
    },
    /// Start MCP server with Server-Sent Events transport
    ///
    /// Launch a web-accessible MCP server that enables integration with
    /// MCP-compatible AI tools and applications via HTTP/SSE.
    Sse {
        /// Path to the Oxy project directory (optional, defaults to current directory)
        ///
        /// Specify the root directory of your Oxy project where
        /// config.yml and other project files are located.
        workspace_path: Option<PathBuf>,
        /// Port number for the MCP Server-Sent Events server
        ///
        /// Specify which port to bind the MCP SSE server for
        /// web-based integrations. Default is 8000.
        #[clap(long, default_value_t = 8000)]
        port: u16,
        /// Host address to bind the MCP SSE server
        ///
        /// Specify which host address to bind the MCP SSE server.
        /// Default is 0.0.0.0 to listen on all interfaces.
        #[clap(long, default_value = "0.0.0.0")]
        host: String,
    },
}

#[derive(Parser, Debug)]
enum SubCommand {
    /// Initialize a repository as an oxy project. Also creates a ~/.config/oxy/config.yaml file if it doesn't exist
    Init,
    /// Execute automation (.automation.yml or .procedure.yml) or SQL (.sql) files
    ///
    /// Run SQL queries against databases or execute automations for data processing.
    Run(RunArgs),
    /// Run evaluation tests defined in .test.yml files
    ///
    /// Executes the cases in a `.test.yml` file against its `.agentic.yml`
    /// target, scores each run with an LLM judge, and reports accuracy and
    /// consistency. Omit the file to run every `.test.yml` in the project.
    Test(test::TestArgs),
    /// Build vector embeddings and sync integrations
    ///
    /// Process your project files and create searchable embeddings for
    /// enhanced semantic search and retrieval functionality. Also synchronizes
    /// configured integrations like Omni and Looker metadata.
    Build(BuildArgs),
    /// Compile the workspace into the compile-boundary Postgres schema (Phase 1.6a observation mode).
    ///
    /// Walks every recognized YAML/SQL file, parses it, and writes a
    /// `revisions` row plus per-entity rows tagged with the new
    /// revision_id. Does NOT update `workspaces.current_revision_id`;
    /// runtime still reads YAML from disk in Phase 1.6a. Useful for
    /// inspecting what the compile boundary produces before any read
    /// path depends on it.
    Compile(compile::CompileArgs),
    /// Synchronize and collect metadata from connected databases
    ///
    /// Extract schema information, table structures, and relationships
    /// from your databases to enable better query suggestions and validation.
    Sync(SyncArgs),
    /// Validate configuration files for syntax and structure
    ///
    /// Check your config.yml, automation files, and agent configurations
    /// for errors and compliance with the expected schema.
    Validate(ValidateArgs),
    /// Start MCP (Model Context Protocol) server
    ///
    /// Launch an MCP server with either stdio or SSE transport for
    /// integration with AI tools and development environments.
    Mcp(McpArgs),
    /// Migrate the database schema to the latest version
    Migrate,
    /// Migrate a customer project to the Automations naming
    ///
    /// Renames legacy `.procedure.yml` / `.workflow.yml` files to the canonical
    /// `.automation.yml` extension and rewrites references to them
    /// (`src:`, `workflow_ref:`, glob includes) across the project's
    /// `.yml` / `.yaml` / `.sql` files. Use `--dry-run` to preview.
    MigrateAutomations(MigrateAutomationsArgs),
    /// Start with Docker PostgreSQL (recommended)
    ///
    /// Launch PostgreSQL in Docker and start the Oxy web server.
    /// Uses postgres:18-alpine container for modern PostgreSQL features.
    /// Data persists in Docker volume 'oxy-postgres-data'.
    Start(StartArgs),
    /// Start the web server (requires OXY_DATABASE_URL)
    ///
    /// Launch the Oxy server. Requires OXY_DATABASE_URL environment variable
    /// to be set to a PostgreSQL connection string.
    /// For automatic PostgreSQL setup, use 'oxy start' instead.
    Serve(ServeArgs),
    /// Show status of Oxy services and Docker containers
    ///
    /// Display the current status of PostgreSQL, Docker, and database
    /// connectivity along with helpful troubleshooting commands.
    Status,
    /// Test and preview terminal color theme support
    ///
    /// Display color samples and theme information to verify
    /// terminal compatibility and appearance settings.
    TestTheme,
    /// Generate JSON schema files for configuration validation
    ///
    /// Create or update schema files used by IDEs and tools
    /// for configuration file validation and autocompletion.
    GenConfigSchema(GenConfigSchemaArgs),
    /// Update the Oxy CLI to the latest available version
    ///
    /// Download and install the newest release of Oxy,
    /// ensuring you have access to the latest features and fixes.
    SelfUpdate,
    /// Execute and manage automation files with advanced options
    ///
    /// Run automation files with additional control over execution,
    /// error handling, and output formatting.
    Make(MakeArgs),

    /// Per-org OLTP Postgres: provision, apply schema, inspect.
    Oltp(crate::cli::commands::oltp::OltpArgs),

    /// Database seeding commands for development and testing
    #[clap(hide = true)]
    Seed(SeedArgs),
    /// Clean ephemeral data and reset project state
    ///
    /// Remove cached data, vector embeddings, and temporary files to reset
    /// the project to a clean state. Useful for troubleshooting data corruption.
    Clean(CleanArgs),
    /// Manage Looker integration metadata
    ///
    /// Synchronize, list, and test Looker integrations configured in your project.
    /// Use subcommands to sync metadata, list explores, or test connections.
    Looker(looker::LookerArgs),
    /// Intent classification and clustering
    ///
    /// Discover and classify user intents from agent questions using
    /// unsupervised clustering techniques (HDBSCAN) and LLM labeling.
    Intent(intent::IntentArgs),
    /// Export ECharts configuration to PNG image
    ///
    /// Render ECharts charts to PNG images using server-side rendering.
    /// Requires Node.js to be installed on the system.
    ExportChart(export_chart::ExportChartArgs),
    /// Run and debug agentic analytics pipelines
    ///
    /// Execute agentic pipelines with interactive debugging and event streaming.
    /// Supports analytics and builder domains. Use --json for LLM-readable output.
    /// Requires OXY_DATABASE_URL to be set.
    Agentic(agentic_cli::AgenticArgs),
    /// Run airway ELT pipelines.
    ///
    /// Execute a `.airway.yml` pipeline (extract → normalize → load)
    /// and stream progress events. Requires OXY_DATABASE_URL to be set.
    Airway(airway::AirwayArgs),
    /// Operator-only administration commands.
    ///
    /// Hosts deployment-wide actions like Airhouse SA rotation. These are
    /// not reachable through the user-facing API; reserve them for ops
    /// runbook flows.
    Admin(admin::AdminArgs),
    /// Run an agentic worker process standalone (no HTTP server).
    ///
    /// Drains the durable task queue (`agentic_task_queue`) from a separate
    /// process so the worker fleet can scale independently of the HTTP
    /// frontend. Pair with `oxy serve --no-workers` for fleets where the
    /// HTTP server is HTTP-only. Requires OXY_DATABASE_URL.
    Worker(worker::WorkerArgs),
    /// Camera fleet operator commands.
    ///
    /// Hosts deployment-wide actions for the camera fleet (currently:
    /// device log retention sweep). Reserved for ops / cron flows.
    Cameras(cameras::CamerasArgs),
    /// Manage custom-app registrations (create, list, delete).
    ///
    /// Wraps the admin app-registry handlers directly — no HTTP server
    /// required. Intended for ops and CI scripts.
    Apps(apps::AppsArgs),
    /// Publish a built custom-app bundle to oxy (one-way deploy).
    ///
    /// Tars `./dist` (or `--dir`) and POSTs it to
    /// `<target>/api/customer-apps/publish`. Identity comes from flags,
    /// then `OXY_*` env vars (`.env.local` is auto-loaded), then the
    /// `apps/<org>/<app>/` path. Replaces `apps ensure` + `aws s3 sync`
    /// + the `/sync` callback. Intended for CI and local testing.
    Publish(publish::PublishArgs),
    /// Generate a safe GitHub Actions workflow for trusted publishing (design §8).
    ///
    /// Writes .github/workflows/oxy-publish.yml with an isolated publish job
    /// (id-token confined to a single environment-gated job) and prints the
    /// publisher registration to complete. Reads nothing from the repo.
    #[command(name = "init-ci")]
    InitCi(init_ci::InitCiArgs),
    /// Authenticate the CLI against an oxy instance for `oxy publish`.
    ///
    /// Opens a browser to log in (loopback flow), caches the token per
    /// target host, and reports whether you're an app-admin (i.e. whether
    /// you can publish). Works against local `oxy serve` and the cloud.
    Login(login::LoginArgs),
    /// Clear the cached `oxy login` token for a target.
    Logout(login::LogoutArgs),
    /// Act as an organization (assume-role) from the terminal.
    ///
    /// Start / show / end the same explicit, audited session the admin UI
    /// uses (`/api/assume`). Oxy staff may act as any org; a partner only as
    /// an assigned client. Bounded to 60 minutes and NOT renewable, and it
    /// closes the `/admin/*` staff surface while live — `oxy assume end`
    /// restores it.
    Assume(assume::AssumeArgs),
    /// Run a local outbound proxy so a custom app in `pnpm dev` hits a cloud
    /// Oxy's real data. Reuses the `oxy login --env` token; defaults to port
    /// 3000 (a drop-in for a local `oxy serve`). Guardrails: tracking events
    /// dropped, side-effecting calls held (`--allow-events` / `--allow-writes`).
    Proxy(proxy::ProxyArgs),
}

#[derive(Parser, Debug)]
pub struct MakeArgs {
    /// Path to the automation file to execute
    file: String,
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum OutputFormat {
    Pretty,
    Json,
}

#[derive(Parser, Debug)]
pub struct BuildArgs {
    /// Drop all existing embedding tables before rebuilding
    ///
    /// Warning: This will delete all existing vector embeddings
    /// and rebuild the entire search index from scratch.
    #[clap(long, short = 'd', default_value_t = false)]
    drop_all_tables: bool,
}

#[derive(Parser, Debug)]
struct SyncArgs {
    /// Specific database to sync (syncs all if not specified)
    ///
    /// Target a single database connection from your config.yml
    /// instead of syncing metadata from all configured databases.
    database: Option<String>,
    /// Specific datasets/tables to sync within the database
    ///
    /// Limit synchronization to particular tables or schemas
    /// instead of processing the entire database structure.
    #[clap(long, short = 'd', num_args = 0..)]
    datasets: Vec<String>,
    /// Overwrite existing metadata files during sync
    ///
    /// Replace existing schema files and metadata instead of
    /// skipping tables that have already been synchronized.
    #[clap(
        long,
        short = 'o',
        default_value_t = false,
        help = "Overwrite existing files during sync"
    )]
    overwrite: bool,
}

pub use crate::cli::{ServeArgs, StartArgs};

#[derive(Parser, Debug)]
struct GenConfigSchemaArgs {
    /// Check for uncommitted schema changes in git
    ///
    /// Verify that generated schema files match the current
    /// configuration structure and fail if changes are detected.
    #[clap(long)]
    check: bool,
}

#[derive(Parser, Debug)]
struct ValidateArgs {
    /// Validate a specific file instead of all configuration files
    ///
    /// Provide a path to an automation (.automation.yml), agentic agent
    /// (.agentic.yml), or app (.app.yml) file to validate just that file.
    ///
    /// Note: .agentic.yml validation is structural only — the file is parsed
    /// against the AgentConfig schema, but `databases:` entries and `llm.ref`
    /// are not resolved against config.yml, so a structurally valid file can
    /// still fail at runtime if those references don't exist.
    #[clap(long, short)]
    file: Option<std::path::PathBuf>,
}

#[derive(Parser, Debug)]
pub struct SeedArgs {
    /// Override the workspace path (defaults to `./examples`).
    #[clap(long)]
    pub workspace_path: Option<std::path::PathBuf>,
    /// Tear down instead of seeding: drops the demo workspace AND the seeded
    /// partner + tenant rows. Leaves the Local org + guest user in place.
    #[clap(long)]
    pub clear: bool,
    /// Skip compiling + promoting the seeded workspaces. Without the compile a
    /// seeded workspace answers `503 needs_recompile` until `oxy compile
    /// --promote` runs for it.
    #[clap(long, conflicts_with = "clear")]
    pub no_compile: bool,
    /// Copy the LLM keys each seeded workspace's `config.yml` references from
    /// this shell's environment into its secrets store. Opt-in because the
    /// values are your real credentials: a URL can look local and still reach a
    /// shared database (a port-forward), so the locality check alone is not
    /// consent. `just up` passes it; the database guard still applies.
    #[clap(long, conflicts_with = "clear")]
    pub llm_keys: bool,
}

#[derive(Parser, Debug)]
pub struct CleanArgs {
    /// What to clean
    #[clap(subcommand)]
    pub target: CleanTarget,
}

#[derive(Parser, Debug)]
pub enum CleanTarget {
    /// Clear all ephemeral data (database artifacts, vector embeddings, and cache)
    ///
    /// Performs a complete cleanup of all ephemeral data including
    /// the .databases folder (semantic models and build artifacts),
    /// vector embeddings, and cached files.
    All,
    /// Clear only the .databases folder
    ///
    /// Removes the .databases folder which contains semantic models,
    /// dataset schemas, and other build artifacts created during
    /// sync and build operations. User data remains preserved.
    DatabaseFolder,
    /// Clear only vector embeddings and search indexes
    ///
    /// Removes all LanceDB vector databases and search indexes
    /// while preserving the .databases folder and cache files.
    Vectors,
    /// Clear cached files and temporary data
    ///
    /// Removes cached chart files, logs, and other temporary data
    /// while preserving .databases folder and vector embeddings.
    Cache,
}

/// Validates a single file based on its extension.
fn validate_single_file(file_path: &PathBuf, config: &Config) -> Result<(), String> {
    let file_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    match () {
        _ if file_name.ends_with(".procedure.yml") || file_name.ends_with(".automation.yml") => {
            let automation = config.load_workflow(file_path).map_err(|e| e.to_string())?;
            config
                .validate_workflow(&automation)
                .map_err(|e| e.to_string())
        }
        _ if file_name.ends_with(".agentic.yml") => {
            agentic_analytics::config::AgentConfig::from_file(file_path)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        _ if file_name.ends_with(".app.yml") => {
            let app = config.load_app(file_path).map_err(|e| e.to_string())?;
            config.validate_app(&app).map_err(|e| e.to_string())
        }
        _ if file_name.ends_with(".view.yml") || file_name.ends_with(".topic.yml") => {
            let parser_config = oxy_semantic::ParserConfig::new(
                file_path
                    .parent()
                    .and_then(|p| p.parent())
                    .unwrap_or(&config.workspace_path),
            );
            let parser = oxy_semantic::SemanticLayerParser::new(parser_config);
            if file_name.ends_with(".view.yml") {
                parser
                    .parse_view_file(file_path)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            } else {
                parser
                    .parse_topic_file(file_path)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        }
        _ => Err(format!(
            "Unknown file type: {}. Expected .automation.yml, .procedure.yml, .agentic.yml, .app.yml, .view.yml, or .topic.yml",
            file_path.display()
        )),
    }
}

/// Collect all `.view.yml` and `.topic.yml` files under `semantics/`.
fn list_semantic_files(project_path: &std::path::Path) -> Vec<PathBuf> {
    let semantics_dir = project_path.join("semantics");
    let mut files = Vec::new();
    for sub in &["views", "topics"] {
        let dir = semantics_dir.join(sub);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && (name.ends_with(".view.yml") || name.ends_with(".topic.yml"))
                {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// The committed JSON Schemas, and the Rust types they are generated from.
///
/// ONE LIST, TWO READERS: `oxy gen-config-schema` writes these files, and
/// `json_schemas_are_current` asserts the committed copies still match. They
/// were regenerated BY HAND with nothing checking, which was tolerable while
/// the only consumer was an editor's `yaml-language-server` hint — a stale
/// schema there costs a missing autocomplete. It stopped being tolerable when
/// `oxyc validate` began validating real workspaces against these files: a
/// stale schema then rejects valid config, or accepts invalid config, and the
/// person who changed the Rust type has no way to know.
pub fn config_schemas() -> Vec<(&'static str, String)> {
    vec![
        (
            "config.json",
            serde_json::to_string_pretty(&schemars::schema_for!(Config)).unwrap_or_default(),
        ),
        (
            "workflow.json",
            serde_json::to_string_pretty(&schemars::schema_for!(Automation)).unwrap_or_default(),
        ),
        (
            "agentic.json",
            serde_json::to_string_pretty(&schemars::schema_for!(
                agentic_analytics::config::AgentConfig
            ))
            .unwrap_or_default(),
        ),
        (
            "app.json",
            serde_json::to_string_pretty(&schemars::schema_for!(AppConfig)).unwrap_or_default(),
        ),
        (
            "agent-test.json",
            serde_json::to_string_pretty(&schemars::schema_for!(TestFileConfig))
                .unwrap_or_default(),
        ),
    ]
}

/// The process's panic hook. It **replaces** whatever is installed — Sentry's
/// panic integration, `human_panic`, and the hook `oxy_telemetry::stderr_capture`
/// sets at boot — rather than chaining: chaining would run Sentry's integration
/// beside the `capture_message` below and report every panic twice.
///
/// Because it replaces, it must write the panic itself. When the stderr capture
/// owns fd 2 (server commands in the JSON log format) that is one structured
/// `target: "panic"` line via `report_panic`; a multi-line `eprintln!` there would
/// arrive as one `ERROR` line plus one unstructured `INFO` line per backtrace
/// frame. Everywhere else — a terminal, a one-shot command — the plain print.
pub(crate) fn install_panic_hook() {
    std::panic::set_hook(Box::new(move |panic_info| {
        // Use eprintln! here — tracing macros must not be called inside a panic
        // hook because the current span's data may already be unwinding, causing
        // a second panic in tracing_subscriber's lookup_current.
        if !oxy_telemetry::stderr_capture::report_panic(panic_info) {
            let trace = backtrace::Backtrace::force_capture();
            eprintln!("panic occurred: {panic_info}\n{trace}");
        }

        // Capture panic in Sentry
        sentry::capture_message(
            &format!("Panic occurred: {}", panic_info),
            sentry::Level::Fatal,
        );
    }));
}

#[cfg(all(test, unix))]
mod panic_hook_tests {
    /// Child half of `the_shipped_hook_writes_one_structured_panic_line`: the
    /// boot order of a server process — stderr capture first, then the hook
    /// `cli()` installs — then a panic.
    #[test]
    fn panic_hook_child_body() {
        if std::env::var("OXY_PANIC_HOOK_CHILD").is_err() {
            return;
        }
        oxy_telemetry::stderr_capture::install(Some("oxy-test".into())).expect("installed");
        super::install_panic_hook();
        let _ = std::thread::spawn(|| panic!("boom after cli() replaced the hook")).join();
        oxy_telemetry::stderr_capture::finish(std::time::Duration::from_secs(5));
    }

    /// `cli()` replaces the panic hook after `logging::init` installed the
    /// capture's. The capture's own test calls `install` alone and so could not
    /// see that the shipped hook threw its structured line away; this one runs
    /// the real order in a child process and reads the child's stderr.
    #[test]
    fn the_shipped_hook_writes_one_structured_panic_line() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::commands::panic_hook_tests::panic_hook_child_body",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OXY_PANIC_HOOK_CHILD", "1")
            .output()
            .expect("child ran");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<serde_json::Value> = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("non-JSON line on stderr ({e}): {l:?}\n{stderr}"))
            })
            .collect();
        let panics: Vec<_> = lines.iter().filter(|v| v["target"] == "panic").collect();
        assert_eq!(
            panics.len(),
            1,
            "exactly one structured panic line: {stderr}"
        );
        assert_eq!(
            panics[0]["panic.message"],
            "boom after cli() replaced the hook"
        );
        assert!(
            panics[0]["panic.backtrace"]
                .as_str()
                .is_some_and(|b| !b.is_empty())
        );
        assert!(
            !stderr.contains("panic occurred"),
            "the plain multi-line print must not also reach the capture: {stderr}"
        );
        assert_eq!(
            lines.len(),
            1,
            "no backtrace frame arrives as its own line: {stderr}"
        );
    }
}

pub async fn cli(
    // Surface API routes composed by the top `oxy-server` crate and forwarded to
    // `serve` (the only subcommand that mounts them). Empty for every other command.
    extra_api_routes: axum::Router<crate::server::router::AppState>,
    // What those surfaces declare about their own pod placement. A surface that
    // only reads Postgres declares nothing and takes the FleetOk default;
    // `oxy-api-onboarding` clones a checkout on disk and must say so.
    extra_api_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
    // Workspace-scoped surface routes (merged inside the `/{workspace_id}` nest).
    // Same forwarding + empty-for-non-serve rule as `extra_api_routes`.
    extra_workspace_routes: axum::Router<crate::server::router::AppState>,
    extra_workspace_decls: Vec<oxy_shared::fleet_role::RouteRoleDecl>,
) -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    install_panic_hook();

    // Add breadcrumb for CLI command
    if let Some(ref command) = args.command {
        let command_name = match command {
            SubCommand::Init => "init",
            SubCommand::Run(_) => "run",
            SubCommand::Build(_) => "build",
            SubCommand::Compile(_) => "compile",
            SubCommand::Sync(_) => "sync",
            SubCommand::Validate(_) => "validate",
            SubCommand::Migrate => "migrate",
            SubCommand::MigrateAutomations(_) => "migrate-automations",
            SubCommand::Start(_) => "start",
            SubCommand::Serve(_) => "serve",
            SubCommand::Status => "status",
            SubCommand::Mcp(_) => "mcp",
            SubCommand::SelfUpdate => "self-update",
            SubCommand::Test(_) => "test",
            SubCommand::TestTheme => "test-theme",
            SubCommand::GenConfigSchema(_) => "gen-config-schema",
            SubCommand::Make(_) => "make",
            SubCommand::Oltp(_) => "oltp",
            SubCommand::Seed(_) => "seed",
            SubCommand::Clean(_) => "clean",
            SubCommand::Looker(_) => "looker",
            SubCommand::Intent(_) => "intent",
            SubCommand::ExportChart(_) => "export-chart",
            SubCommand::Agentic(_) => "agentic",
            SubCommand::Airway(_) => "airway",
            SubCommand::Admin(_) => "admin",
            SubCommand::Worker(_) => "worker",
            SubCommand::Cameras(_) => "cameras",
            SubCommand::Apps(_) => "apps",
            SubCommand::Publish(_) => "publish",
            SubCommand::InitCi(_) => "init-ci",
            SubCommand::Login(_) => "login",
            SubCommand::Proxy(_) => "proxy",
            SubCommand::Logout(_) => "logout",
            SubCommand::Assume(_) => "assume",
        };

        sentry_config::add_breadcrumb(
            &format!("Executing CLI command: {}", command_name),
            "cli",
            sentry::Level::Info,
        );
        sentry_config::add_operation_context(command_name, None);
    }

    match args.command {
        Some(SubCommand::GenConfigSchema(args)) => {
            let schemas_path = std::path::Path::new("json-schemas");
            if !schemas_path.exists() {
                std::fs::create_dir_all(schemas_path)?;
            }

            let schemas = config_schemas();

            for (filename, schema) in &schemas {
                std::fs::write(schemas_path.join(filename), schema)?;
            }

            println!("Generated schema files successfully");

            if args.check {
                let output = Command::new("git").args(["status", "--short"]).output()?;

                if !output.status.success() {
                    eprintln!(
                        "Failed to get changed files: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    exit(1);
                }

                // `git status --short` emits one "XY <path>" entry per line,
                // with rename entries as "XY <old> -> <new>". Parse line-by-line
                // and compare each path exactly — substring matching would
                // misfire on e.g. `agent.json` matching `agent-test.json`.
                let stdout = String::from_utf8_lossy(&output.stdout);
                let changed_paths: std::collections::HashSet<&str> = stdout
                    .lines()
                    .filter_map(|line| {
                        // Skip the 2-char status + 1-char separator.
                        let rest = line.get(3..)?.trim_start();
                        // For renames the affected path is after " -> ".
                        Some(rest.rsplit_once(" -> ").map(|(_, new)| new).unwrap_or(rest))
                    })
                    .collect();

                let schema_files: Vec<String> = schemas
                    .iter()
                    .map(|(filename, _)| format!("json-schemas/{filename}"))
                    .collect();

                for file in schema_files {
                    if changed_paths.contains(file.as_str()) {
                        eprintln!("Unexpected changes were found in schema files.");
                        eprintln!(
                            "Please review these changes and update the schema generation code by `cargo run gen-config-schema`."
                        );
                        exit(1)
                    }
                }
            }
        }
        Some(SubCommand::Init) => match init() {
            Ok(_) => println!("{}", "Initialization complete.".success()),
            Err(e) => eprintln!("{}", format!("Initialization failed: {e}").error()),
        },
        Some(SubCommand::Run(run_args)) => {
            sentry_config::add_operation_context("run", Some(&run_args.file));
            handle_run_command(run_args).await?;
        }
        Some(SubCommand::Test(test_args)) => {
            sentry_config::add_operation_context("test", test_args.file.as_deref());
            test::handle_test_command(test_args).await?;
        }
        Some(SubCommand::Build(build_args)) => {
            sentry_config::add_operation_context("build", None);

            handle_omni_sync().await?;

            // Synchronize Looker metadata if configured
            handle_looker_auto_sync().await?;

            // Setup
            let workspace_path = resolve_local_workspace_path()?
                .to_string_lossy()
                .to_string();
            let config_manager = ConfigBuilder::new()
                .with_workspace_path(workspace_path)?
                .build_with_working_copy(Origin::Disk, OnMissing::Fail)
                .await?;
            let secrets_manager = SecretsManager::from_environment()?;

            // Build vector embeddings
            reindex(ReindexInput {
                config: config_manager.clone(),
                secrets_manager,
                drop_all_tables: build_args.drop_all_tables,
            })
            .await?;

            println!("✅ Build complete");
        }
        Some(SubCommand::Compile(compile_args)) => {
            sentry_config::add_operation_context("compile", None);
            if let Err(e) = compile::run_compile(compile_args).await {
                eprintln!("{}", format!("Compile failed: {e}").error());
                exit(1);
            }
        }
        Some(SubCommand::Sync(sync_args)) => {
            sentry_config::add_operation_context("sync", None);
            if let Some(ref db) = sync_args.database {
                sentry_config::add_database_context(db, None);
            }
            let config = ConfigBuilder::new()
                .with_workspace_path(&resolve_local_workspace_path()?)?
                .build_with_working_copy(Origin::Disk, OnMissing::Fail)
                .await?;

            let secrets_manager = SecretsManager::from_environment()?;
            let filter = sync_args.database.clone().map(|db| SyncFilter {
                database: Some(db),
                datasets: sync_args.datasets.clone(),
                tables: vec![],
            });
            debug!(sync_args = ?sync_args, "Syncing");
            println!("🔄Syncing databases");
            let sync_metrics =
                sync_databases(config.clone(), secrets_manager, filter, sync_args.overwrite)
                    .await?;
            println!(
                "✅Sync finished:\n\n{}",
                sync_metrics
                    .into_iter()
                    .map(|m| m.map_or_else(|e| e.to_string().error().to_string(), |v| v.to_string()))
                    .collect::<Vec<_>>()
                    .join("\n---\n")
            )
        }
        Some(SubCommand::Validate(args)) => {
            let config: ::oxy::config::ConfigManager<WorkingCopy> = ConfigBuilder::new()
                .with_workspace_path(&resolve_local_workspace_path()?)?
                .build_with_working_copy(Origin::Disk, OnMissing::Fail)
                .await?;

            if let Some(file_path) = args.file {
                let validation_result = validate_single_file(&file_path, config.get_config());
                match validation_result {
                    Ok(_) => println!("{}", format!("{} is valid", file_path.display()).success()),
                    Err(e) => {
                        println!("{}", e.error());
                        exit(1)
                    }
                }
            } else {
                // Validate all files, collecting all errors
                let cfg = config.get_config();
                let mut errors: Vec<String> = Vec::new();
                let mut valid_count = 0;

                for automation_file in cfg.list_workflows(&cfg.workspace_path) {
                    match validate_single_file(&automation_file, cfg) {
                        Ok(_) => valid_count += 1,
                        Err(e) => errors.push(format!("{}: {}", automation_file.display(), e)),
                    }
                }

                for agentic_file in cfg.list_agentic_agents(&cfg.workspace_path) {
                    match validate_single_file(&agentic_file, cfg) {
                        Ok(_) => valid_count += 1,
                        Err(e) => errors.push(format!("{}: {}", agentic_file.display(), e)),
                    }
                }

                for app_file in cfg.list_apps(&cfg.workspace_path) {
                    match validate_single_file(&app_file, cfg) {
                        Ok(_) => valid_count += 1,
                        Err(e) => errors.push(format!("{}: {}", app_file.display(), e)),
                    }
                }

                // Validate semantic model files (.view.yml, .topic.yml)
                for semantic_file in list_semantic_files(&cfg.workspace_path) {
                    match validate_single_file(&semantic_file, cfg) {
                        Ok(_) => valid_count += 1,
                        Err(e) => errors.push(format!("{}: {}", semantic_file.display(), e)),
                    }
                }

                if errors.is_empty() {
                    println!(
                        "{}",
                        format!("All {} config files are valid", valid_count).success()
                    );
                } else {
                    for err in &errors {
                        println!("{}", err.error());
                    }
                    println!(
                        "{}",
                        format!(
                            "\n{} file(s) failed validation, {} file(s) valid",
                            errors.len(),
                            valid_count
                        )
                        .error()
                    );
                    exit(1)
                }
            }
        }
        Some(SubCommand::Oltp(args)) => {
            if let Err(e) = crate::cli::commands::oltp::oltp(args).await {
                eprintln!("{}", format!("{e}").error());
                exit(1);
            }
        }
        Some(SubCommand::Migrate) => {
            if let Err(e) = migrate().await {
                eprintln!("{}", format!("Migration failed: {e}").error());
                exit(1);
            } else {
                println!("{}", "Migration completed successfully".success());
            }
        }
        Some(SubCommand::MigrateAutomations(args)) => {
            if let Err(e) = migrate_automations(args) {
                eprintln!("{}", format!("Automation migration failed: {e}").error());
                exit(1);
            }
        }
        Some(SubCommand::Start(start_args)) => {
            if let Err(e) = start::start_database_and_server(
                start_args,
                extra_api_routes,
                extra_api_decls,
                extra_workspace_routes,
                extra_workspace_decls,
            )
            .await
            {
                // Returned rather than `exit(1)`-ed: `main` prints the same
                // message, records it to Sentry, and — the reason this changed —
                // flushes the platform-telemetry exporters, so the failure that
                // crash-loops a pod is the last thing HyperDX shows for it.
                return Err(OxyError::RuntimeError(format!("Failed to start: {e}")).into());
            }
        }
        Some(SubCommand::Serve(serve_args)) => {
            if let Err(e) = start_server_and_web_app(
                serve_args,
                extra_api_routes,
                extra_api_decls,
                extra_workspace_routes,
                extra_workspace_decls,
            )
            .await
            {
                // Returned rather than `exit(1)`-ed: `main` prints the same
                // message, records it to Sentry, and — the reason this changed —
                // flushes the platform-telemetry exporters, so the failure that
                // crash-loops a pod is the last thing HyperDX shows for it.
                return Err(OxyError::RuntimeError(format!("Server failed: {e}")).into());
            }
        }
        Some(SubCommand::Worker(worker_args)) => {
            if let Err(e) = worker::run_worker(worker_args).await {
                // Returned rather than `exit(1)`-ed: `main` prints the same
                // message, records it to Sentry, and — the reason this changed —
                // flushes the platform-telemetry exporters, so the failure that
                // crash-loops a pod is the last thing HyperDX shows for it.
                return Err(OxyError::RuntimeError(format!("Worker failed: {e}")).into());
            }
        }
        Some(SubCommand::Status) => {
            if let Err(e) = status::show_status().await {
                eprintln!("{}", format!("Failed to get status: {e}").error());
                exit(1);
            }
        }
        Some(SubCommand::Mcp(mcp_args)) => match mcp_args.transport {
            McpTransport::Stdio { workspace_path } => {
                let env_path = workspace_path.join(".env");
                dotenv::from_path(env_path).ok();
                let _ = start_mcp_stdio(workspace_path).await;
            }
            McpTransport::Sse {
                workspace_path,
                port,
                host,
            } => {
                let workspace_path = match workspace_path {
                    Some(path) => path,
                    None => resolve_local_workspace_path()?,
                };
                let cancellation_token = start_mcp_sse_server(port, host, workspace_path)
                    .await
                    .expect("Failed to start MCP SSE server");

                tokio::signal::ctrl_c().await.unwrap();
                println!("Shutting down server...");
                cancellation_token.cancel();
            }
        },
        Some(SubCommand::SelfUpdate) => {
            if let Err(e) = handle_check_for_updates().await {
                error!(error = %e, "Failed to update");
                eprintln!("{}", format!("Failed to update: {e}").error());
                exit(1);
            }
        }
        Some(SubCommand::TestTheme) => {
            println!("Initial theme mode: {:?}", get_current_theme_mode());
            println!("True color support: {:?}", detect_true_color_support());
            println!("{}", "analysis".primary());
            println!("{}", "success".success());
            println!("{}", "warning".warning());
            eprintln!("{}", "error".error());
            println!("{}", "https://github.com/oxy-hq/oxygen/".secondary());
            println!("{}", "-region".tertiary());
            println!("{}", "Viewing repository".info());
            println!("{}", "text".text());
        }
        Some(SubCommand::Make(make_args)) => {
            handle_make_command(&make_args).await?;
        }

        Some(SubCommand::Seed(seed_args)) => {
            handle_seed_command(seed_args).await?;
        }

        Some(SubCommand::Clean(clean_args)) => {
            handle_clean_command(clean_args).await?;
        }

        Some(SubCommand::Looker(looker_args)) => {
            looker::handle_looker_command(looker_args).await?;
        }
        Some(SubCommand::Intent(intent_args)) => {
            intent::handle_intent_command(intent_args).await?;
        }

        Some(SubCommand::ExportChart(export_chart_args)) => {
            export_chart::handle_export_chart_command(export_chart_args).await?;
        }

        Some(SubCommand::Agentic(agentic_args)) => {
            agentic_cli::handle_agentic_command(agentic_args).await?;
        }

        Some(SubCommand::Airway(airway_args)) => {
            airway::handle_airway_command(airway_args).await?;
        }

        Some(SubCommand::Admin(admin_args)) => {
            admin::handle_admin_command(admin_args).await?;
        }

        Some(SubCommand::Cameras(cameras_args)) => {
            cameras::handle_cameras_command(cameras_args).await?;
        }

        Some(SubCommand::Apps(apps_args)) => {
            apps::handle_apps_command(apps_args).await?;
        }

        Some(SubCommand::Publish(publish_args)) => {
            publish::handle_publish_command(publish_args).await?;
        }

        Some(SubCommand::InitCi(args)) => {
            init_ci::handle_init_ci_command(args).await?;
        }

        Some(SubCommand::Login(login_args)) => {
            login::handle_login_command(login_args).await?;
        }

        Some(SubCommand::Logout(logout_args)) => {
            login::handle_logout_command(logout_args).await?;
        }

        Some(SubCommand::Proxy(proxy_args)) => {
            proxy::handle_proxy_command(proxy_args).await?;
        }

        Some(SubCommand::Assume(assume_args)) => {
            assume::handle_assume_command(assume_args).await?;
        }

        None => {
            Args::command().print_help().unwrap();
        }
    }

    Ok(())
}

async fn handle_omni_sync() -> Result<(), OxyError> {
    use crate::server::service::omni_sync::OmniSyncService;
    use omni::{OmniApiClient, OmniError as AdapterOmniError};

    // Load configuration to get Omni integration settings
    let workspace_path = resolve_local_workspace_path()?;

    let project = WorkspaceBuilder::<WorkingCopy>::new(Uuid::nil())
        .with_working_copy(&workspace_path, None, OnMissing::Fail)
        .await?
        .build()
        .await
        .map_err(|e| OxyError::from(anyhow::anyhow!("Failed to create project: {e}")))?;

    let config = project.config_manager.clone();

    // Get all Omni integration configurations - if none found, skip silently
    let omni_integrations: Vec<_> = config
        .get_config()
        .integrations
        .iter()
        .filter_map(|integration| match &integration.integration_type {
            ::oxy::config::model::IntegrationType::Omni(omni_integration) => {
                Some((integration.name.clone(), omni_integration.clone()))
            }
            _ => None,
        })
        .collect();

    if omni_integrations.is_empty() {
        // No Omni integrations configured, skip silently
        return Ok(());
    }

    println!(
        "🔗 Synchronizing {} Omni integration(s)...",
        omni_integrations.len()
    );

    let mut all_sync_results = Vec::new();
    let mut total_successful_topics = Vec::new();

    for (integration_name, omni_integration) in omni_integrations {
        println!("\n🔗 Processing integration: {}", integration_name);

        // Resolve API key from environment variable
        let api_key = project
            .secrets_manager
            .resolve_secret(&omni_integration.api_key_var)
            .await?
            .unwrap();
        let base_url = omni_integration.base_url.clone();
        let topics = omni_integration.topics.clone();

        // Sync all configured topics for this integration
        println!("🔄 Synchronizing Omni metadata for {} topics", topics.len());
        let topics_to_sync: Vec<_> = topics.iter().collect();

        let api_client =
            OmniApiClient::new(base_url.clone(), api_key.clone()).map_err(|e| match e {
                AdapterOmniError::ConfigError(msg) => {
                    OxyError::ConfigurationError(format!("Omni configuration error: {}", msg))
                }
                _ => OxyError::RuntimeError(format!("Failed to create Omni API client: {}", e)),
            })?;

        let sync_service =
            OmniSyncService::new(api_client, &workspace_path, integration_name.clone());

        // Perform synchronization for each topic in this integration
        println!("📥 Fetching metadata from Omni API...");

        let mut integration_results = Vec::new();
        for topic in &topics_to_sync {
            println!(
                "  📋 Syncing topic: {} (model: {})",
                topic.name, topic.model_id
            );
            let sync_result = sync_service
                .sync_metadata(&topic.model_id, &topic.name)
                .await
                .map_err(|e| {
                    OxyError::RuntimeError(format!(
                        "Sync operation failed for topic '{}' (model '{}'): {}",
                        topic.name, topic.model_id, e
                    ))
                })?;
            integration_results.push(sync_result);
        }

        if let Some(first_result) = integration_results.into_iter().next() {
            total_successful_topics.extend(first_result.successful_topics.clone());
            all_sync_results.push(first_result);
        }
    }

    // Display overall results
    println!("\n{}", "🎉 Omni synchronization completed!".success());

    if !all_sync_results.is_empty() {
        let overall_success = all_sync_results.iter().all(|r| r.is_success());
        let partial_success = all_sync_results.iter().any(|r| r.is_partial_success());

        if overall_success {
            println!(
                "{}",
                "All integrations synchronized successfully.".success()
            );
        } else if partial_success {
            println!(
                "{}",
                "Partial synchronization completed with some errors.".warning()
            );
            // Show error summaries from failed integrations
            for sync_result in &all_sync_results {
                if let Some(error_summary) = sync_result.error_summary() {
                    println!("\n{}", "Errors encountered:".warning());
                    println!("{}", error_summary.error());
                }
            }
        } else {
            println!("{}", "Some integrations failed to synchronize.".error());
            for sync_result in &all_sync_results {
                if let Some(error_summary) = sync_result.error_summary() {
                    println!("\n{}", "Errors encountered:".error());
                    println!("{}", error_summary.error());
                }
            }
            return Err(OxyError::RuntimeError(
                "Some Omni sync operations failed".to_string(),
            ));
        }

        // Show all successful topics across all integrations
        if !total_successful_topics.is_empty() {
            println!("\n{}", "Successfully synchronized topics:".success());
            for topic in &total_successful_topics {
                println!("  ✅ {}", topic);
            }
        }
    }

    Ok(())
}

async fn handle_looker_auto_sync() -> Result<(), OxyError> {
    let workspace_path = resolve_local_workspace_path()?;

    let project = WorkspaceBuilder::<WorkingCopy>::new(Uuid::nil())
        .with_working_copy(&workspace_path, None, OnMissing::Fail)
        .await?
        .with_runs_manager(::oxy::adapters::runs::RunsManager::noop())
        .build()
        .await
        .map_err(|e| OxyError::from(anyhow::anyhow!("Failed to create project: {e}")))?;

    let looker_integrations: Vec<_> = project
        .config_manager
        .get_config()
        .integrations
        .iter()
        .filter_map(|integration| match &integration.integration_type {
            ::oxy::config::model::IntegrationType::Looker(_) => Some(integration.name.clone()),
            _ => None,
        })
        .collect();

    if looker_integrations.is_empty() {
        return Ok(());
    }

    looker::handle_looker_sync(looker::LookerSyncArgs {
        integration: None,
        model: None,
        explore: None,
        force: false,
    })
    .await
}

async fn handle_check_for_updates() -> Result<(), OxyError> {
    println!("{}", "Checking for updates...".info());

    let target = format!(
        "{}-{}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        std::env::consts::FAMILY
    );

    let status = tokio::task::spawn_blocking(move || {
        self_update::backends::github::Update::configure()
            .repo_owner("oxy-hq")
            .repo_name("oxy")
            .bin_name(&format!("oxy-{target}"))
            .show_download_progress(true)
            .current_version(self_update::cargo_crate_version!())
            .build()
            .map_err(|e| OxyError::RuntimeError(format!("Update configuration failed: {e}")))?
            .update()
            .map_err(|e| OxyError::RuntimeError(format!("Update failed: {e}")))
    })
    .await
    .map_err(|e| OxyError::RuntimeError(format!("Task join error: {e}")))??;

    if status.updated() {
        println!(
            "{}",
            "Update successful! Restart the application.".success()
        );
    } else {
        println!("{}", "No updates available.".info());
    }
    Ok(())
}

async fn handle_seed_command(seed_args: SeedArgs) -> Result<(), OxyError> {
    use seed::*;
    if seed_args.clear {
        // Guard BOTH teardowns UP FRONT. clear_demo has no is_local() gate of its
        // own, so without this it would run (unguarded) before clear_partner_tenants
        // rejected a non-local DB — a partial, guard-bypassing teardown. Refuse
        // first; delete nothing on a remote DB.
        seed_partners::refuse_if_not_local()?;
        clear_demo().await?;
        seed_partners::clear_partner_tenants().await
    } else {
        // Seeds the demo workspace AND (folded in) the partner + tenant data —
        // one command, no `--partners` flag — then compiles, and stores LLM keys
        // only when asked to (`--llm-keys`).
        let options = SeedOptions {
            compile: !seed_args.no_compile,
            llm_keys: seed_args.llm_keys,
        };
        seed_demo_with(seed_args.workspace_path, options).await
    }
}

async fn handle_clean_command(clean_args: CleanArgs) -> Result<(), OxyError> {
    use clean::*;

    let config_manager = ConfigBuilder::new()
        .with_workspace_path(&resolve_local_workspace_path()?)?
        .build_with_working_copy(Origin::Disk, OnMissing::Fail)
        .await?;

    match clean_args.target {
        CleanTarget::All => {
            clean_all(true, &config_manager).await?;
        }
        CleanTarget::DatabaseFolder => {
            clean_database_folder(true, &config_manager).await?;
        }
        CleanTarget::Vectors => {
            clean_vectors(true, &config_manager).await?;
        }
        CleanTarget::Cache => {
            clean_cache(true, &config_manager).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod json_schema_freshness {
    use super::config_schemas;

    /// The committed `json-schemas/*.json` still match the Rust types.
    ///
    /// They are generated by `oxy gen-config-schema`, BY HAND, and nothing
    /// checked. That was survivable while the only consumer was an editor
    /// hint; `oxyc validate` now validates real workspaces against these
    /// files, so a stale one rejects valid config or accepts invalid config
    /// and the person who changed the Rust struct never finds out.
    ///
    /// Reads the checkout, so it no-ops where the sources are not present —
    /// the same shape `route_catalog`'s tree walk uses.
    #[test]
    fn json_schemas_are_current() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../json-schemas");
        if !root.is_dir() {
            return;
        }

        let mut stale = Vec::new();
        for (filename, generated) in config_schemas() {
            let path = root.join(filename);
            let committed = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => {
                    stale.push(format!("{filename} is missing"));
                    continue;
                }
            };
            // Compared as VALUES, not as text: `to_string_pretty` key order is
            // stable but whitespace at the end of a file is not worth failing
            // a build over.
            // Parsed with `?`-shaped handling rather than `unwrap_or_default`:
            // that turned two unparsable documents into `Null == Null` and
            // reported them as current.
            let a = match serde_json::from_str::<serde_json::Value>(&committed) {
                Ok(v) => v,
                Err(e) => {
                    stale.push(format!("{filename} is not valid JSON: {e}"));
                    continue;
                }
            };
            let b = match serde_json::from_str::<serde_json::Value>(&generated) {
                Ok(v) => v,
                Err(e) => {
                    stale.push(format!("{filename} could not be generated: {e}"));
                    continue;
                }
            };
            if a != b {
                stale.push(format!("{filename} differs from schema_for!()"));
            }
        }

        assert!(
            stale.is_empty(),
            "the committed JSON Schemas are out of date:\n  {}\n\n\
             Run `cargo run --bin oxy -- gen-config-schema` and commit the result. \
             `oxyc validate` validates real workspaces against these files, so a stale \
             one rejects valid config or accepts invalid config.",
            stale.join("\n  ")
        );
    }

    /// Schemas that are committed but NOT generated from a Rust type.
    ///
    /// Each is hand-maintained and has no `schema_for!()` to compare against,
    /// so the freshness check above cannot cover them. Listed by name so the
    /// exemption is a decision rather than an omission.
    const HAND_MAINTAINED: &[&str] = &[
        // The agentic browser-test harness's own format (web-app/tests/agentic).
        "flow-test.json",
        // The semantic layer's cross-workspace file; no single Rust type.
        "global-semantics.json",
    ];

    /// Every committed schema is either generated or explicitly exempt.
    ///
    /// THIS IS THE CHECK THAT WAS MISSING. `json_schemas_are_current` walks the
    /// GENERATED list and asks "is the committed copy current" — so a file that
    /// nothing generates is invisible to it. `automation.json` lived that way:
    /// committed, titled `Automation` like `workflow.json`, 6 KB behind it, and
    /// the web app's Monaco editor validated every `.automation.yml` against it.
    /// Two committed definitions of one Rust type, and the gate could not see
    /// the second one because it only ever looked at the first.
    #[test]
    fn every_committed_schema_is_generated_or_exempt() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../json-schemas");
        // A MISSING DIRECTORY IS A FAILURE, not a pass. Returning quietly here
        // would turn the whole gate into a no-op the moment `json-schemas/`
        // moved or was renamed — green, and blind to exactly the orphan it
        // exists to catch.
        let entries = std::fs::read_dir(&root).unwrap_or_else(|e| {
            panic!("could not read {}: {e}", root.display());
        });

        let generated: Vec<String> = config_schemas()
            .into_iter()
            .map(|(name, _)| name.to_string())
            .collect();

        let mut orphans = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            if generated.contains(&name) || HAND_MAINTAINED.contains(&name.as_str()) {
                continue;
            }
            orphans.push(name);
        }
        orphans.sort();

        assert!(
            orphans.is_empty(),
            "these schemas are committed but nothing generates them, and they are not \
             listed as hand-maintained: {orphans:?}\n\n\
             A schema nobody generates is a second definition of whatever it describes, \
             and it drifts silently — `automation.json` was 6 KB behind `workflow.json` \
             while the IDE validated against it. Either add it to `config_schemas()`, \
             add it to HAND_MAINTAINED with a reason, or delete it."
        );
    }
}
