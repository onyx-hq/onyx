//! Which warehouses `ctx.warehouse.upsert` can reach.
//!
//! An upsert compiles to `INSERT … ON CONFLICT (…) DO UPDATE SET …`
//! (`host::build_insert_sql`), a spelling only the Postgres family parses.
//! Anywhere else the engine refuses it in its own words, which tell an app
//! author nothing about what to do instead — on ClickHouse, `ON CONFLICT` lands
//! in the `VALUES` row stream and comes back as `Code: 27`, a row it could not
//! parse. So the host refuses first, by name, before sending anything.
//!
//! Parsing is not the same as working. `ON CONFLICT` also needs a primary key
//! or unique constraint on the conflict columns, and DuckLake tables — which is
//! what Airhouse is, reporting the DuckDB dialect — cannot carry one. There the
//! statement is let through and the engine refuses it, as does Redshift, which
//! reports the Postgres dialect without parsing `ON CONFLICT` at all.

use agentic_connector::SqlDialect;

/// `Ok` when `dialect` parses the upsert `build_insert_sql` emits; otherwise
/// the refusal the app author sees.
pub(super) fn check(dialect: SqlDialect) -> Result<(), String> {
    if dialect.parses_on_conflict() {
        return Ok(());
    }
    Err(format!(
        "warehouse.upsert is not supported on {dialect}: it compiles to \
         `INSERT … ON CONFLICT … DO UPDATE`, which only Postgres and DuckDB parse. \
         Use warehouse.insert, or warehouse.exec with this warehouse's own upsert \
         statement."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_is_refused_by_name_where_on_conflict_is_not_sql() {
        let err = check(SqlDialect::CLICKHOUSE).expect_err("ClickHouse has no ON CONFLICT");
        assert!(
            err.starts_with("warehouse.upsert is not supported on ClickHouse:"),
            "{err}"
        );
        for dialect in [
            SqlDialect::BigQuery,
            SqlDialect::Snowflake,
            SqlDialect::Other("MySQL"),
            SqlDialect::Other("DOMO"),
        ] {
            assert!(check(dialect).is_err(), "{dialect} must be refused");
        }
    }

    #[test]
    fn upsert_goes_through_where_on_conflict_parses() {
        // DuckDb is also Airhouse's dialect.
        for dialect in [SqlDialect::Postgres, SqlDialect::DuckDb, SqlDialect::Sqlite] {
            assert_eq!(check(dialect), Ok(()), "{dialect}");
        }
    }
}
