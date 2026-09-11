//! Platform tests that don't belong to a single product domain — the compile
//! boundary and the workspace-source rules it rests on, compiled readers, the
//! CLI `build`/`run` commands, router shape in local mode, project queries,
//! workspace detail fields, and the partner-table regression guard.
//!
//! One binary for the whole group; see `tests/authz/main.rs` for why. Add a case
//! as a `mod` here rather than a new `tests/*.rs`.
//!
//! Four of these ask "where may a workspace file be read from?" rather than
//! "which pod serves this route" — `compiled_reader_is_not_a_back_door`,
//! `workspace_path_backdoor`, `workspace_path_escape_hatch` and
//! `walker_storage_divergence`. That is the line between this group and
//! `tests/routing/`: nothing here reasons about the router. They are source
//! scans and walker-semantics tests, so they open no database and mutate no
//! process-global state.
//!
//! Mixed three ways, and the split is in `.config/nextest.toml`:
//!
//! * `compiled_reader_semantic`, `toast_webhook_compile_boundary`,
//!   `airway_compile_boundary`, `anomaly_bulk_status`, `simulation_routes` and
//!   `simulation_lifecycle` are database-backed
//!   through [`common::fresh_db`] — own database each, so they sit in
//!   `db-per-test` (`max-threads = 4`).
//! * `projects_query` and `local_mode_router` call `api_router(..)`, which
//!   reaches the *shared* `OXY_DATABASE_URL` and runs `cleanup_stale_runs` — an
//!   unscoped UPDATE over `agentic_runs`. They are excluded from `db-per-test`
//!   and pinned into `serial-db` with the other shared-schema tests.
//! * everything else is in-process, but NOT ungrouped: that override matches
//!   `binary(=platform)`, so every module here except those exclusions runs
//!   under `db-per-test` (`max-threads = 4`, `retries = 2`) whether or not it
//!   opens a connection. The four source scans folded in above inherit that.
//!   Deliberate, and argued in the config: a per-binary rule stays true the day
//!   someone adds a harness-backed test to one of those files, where a module
//!   allowlist would silently go stale. `tests/routing/` is the group that got
//!   the other answer — nothing in it can reach a database, so it is in no
//!   group at all.
//!
//! `authz::shared_db_registry` fails the build if that classification drifts.
//!
//! Two members mutate process cwd —
//! `workspace_details_fields::storage_key_normalizes_relative_to_absolute` and
//! all of `metric_tree_fit_panel`. Both carry `#[serial_test::serial]`, whose
//! lock is global to this binary; anything folded in here that touches cwd,
//! environment variables or another process-global needs the same attribute — a
//! guard private to one file excludes nothing once the files share a binary.

#[path = "../common/mod.rs"]
mod common;

mod airway_compile_boundary;
mod anomaly_bulk_status;
mod artifact_naming_agrees;
mod build;
mod chat_org_standing;
mod compile;
mod compile_oltp_promote;
mod compiled_reader_is_not_a_back_door;
mod compiled_reader_semantic;
mod feature_flag_refresh;
mod frontline_app_grant;
mod frontline_devices;
mod frontline_pin;
mod local_mode_router;
mod metric_tree_fit_panel;
mod no_dropped_partner_tables;
mod notification_devices;
mod oltp_provisioner;
mod operating_graph;
mod org_default_workspace;
mod projects_query;
mod run;
mod simulation_lifecycle;
mod simulation_router;
mod simulation_routes;
mod toast_webhook_compile_boundary;
mod walker_storage_divergence;
mod work_item_gates;
mod workspace_details_fields;
mod workspace_path_backdoor;
mod workspace_path_escape_hatch;
mod world_model_cross_pod;
