//! Custom Apps platform tests — access control, visibility, the compile/serve
//! boundary, cache invalidation, and the seeded example app.
//!
//! One binary for the whole domain; see `tests/authz/main.rs` for why. Add a
//! case as a `mod` here rather than a new `tests/*.rs`.
//!
//! Most of these are database-backed via [`common::test_db`], which gives each
//! test its own database cloned from a per-run template. The binary is therefore
//! in the `db-per-test` group (`max-threads = 4`) in `.config/nextest.toml`, not
//! the fully-serialized `serial-db` group — they contend for one Postgres server,
//! not for a schema.

#[path = "../common/mod.rs"]
mod common;

mod admin_app_workspace_in_org;
mod app_scoped_secrets;
mod custom_app_access_control;
mod custom_app_activity_roles;
mod custom_app_platform_runtime;
mod custom_app_storage_routes;
mod custom_app_visibility;
mod custom_apps_boundary;
mod custom_apps_cache_invalidation;
mod custom_apps_publish_function_artifacts;
mod custom_apps_publish_workspace;
mod example_app_serving;
mod function_failure_alerts;
mod seed_example_app;
mod storage_history_query;
mod warehouse_writes_on_engines;
