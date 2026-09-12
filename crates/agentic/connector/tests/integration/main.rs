//! Integration tests for `agentic-connector`, in ONE binary.
//!
//! Cargo compiles every `tests/*.rs` into its own executable, each statically
//! linking the whole dependency graph — so a test *file* costs a link, not just
//! a compile. Grouping keeps that cost proportional to crates rather than to
//! files. Add a case as a `mod` below, not as a new `tests/*.rs`.
//!
//! See `internal-docs/testing.md` for the cost model.

mod clickhouse_tagged_tests;
mod clickhouse_tests;
mod domo_tests;
mod duckdb_tests;
mod mysql_tests;
mod postgres_tests;
mod postgres_tx_tests;
