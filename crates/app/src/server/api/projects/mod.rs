//! Project-scoped HTTP endpoints that custom-app bundles call.
//!
//! Currently a single route — `POST /api/projects/{project_id}/query` —
//! which proxies a SQL or semantic-query request to one of the project's
//! databases. Cookie auth → user → org_member of project's org is the
//! gate; no per-bundle config.
//!
//! Lives in its own module so future additions (project metadata,
//! generic write endpoint) don't need to be threaded into the router
//! piecemeal.

pub mod agent_ask;
pub mod agent_run_stream;
pub mod automation_run;
pub mod cohort;
pub mod metric_tree;
pub mod metric_tree_projection;
pub mod query;
pub mod result_cache;
pub mod semantic_boundary;
pub mod semantic_query;
pub mod world_model;
