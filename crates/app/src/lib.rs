//! Oxy Application - CLI and HTTP Server
//!
//! This crate provides both command-line interface and HTTP server functionality
//! for Oxy, integrating domain crates (oxy-auth, agentic-*, oxy-workflow, etc.)

// The deeply-nested async futures in the HTTP layer (Axum handlers + the
// custom-app function host's chained `with_db_timeout`/connector futures)
// exceed the default type-layout query depth of 128. rustc's own suggestion.
#![recursion_limit = "256"]

pub mod agentic_wiring;
pub mod airway_boot;
pub mod cli;
/// The admin apps list's traceability rule — see the module doc. `oxyc publish`
/// (`sdk/cli/src/publish/provenance.ts`) mirrors it for its warnings.
pub mod custom_app_provenance;
pub mod custom_app_template;
pub mod emails;
pub mod integrations;
pub mod observability_boot;
pub mod observability_setup;
pub mod server;

// Re-export commonly used items
pub use server::{api, service};
