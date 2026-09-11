//! Span attributes for warehouse queries, after the OpenTelemetry database
//! semantic conventions.
//!
//! Every [`DatabaseConnector`](crate::DatabaseConnector) query method carries
//! `#[tracing::instrument]` built from these helpers, so each statement a
//! warehouse runs is one `CLIENT` span named `"{operation} {system}"`
//! (`SELECT snowflake`) with `db.system.name`, `db.operation.name` and the
//! statement itself on `db.query.text`. Before this, `agentic-connector` had no
//! span at all: a slow agent run showed as one long parent with nothing inside
//! it saying the time was the warehouse.
//!
//! A failed statement is recorded on the span by `#[instrument(err)]` at
//! `info`, not `error`: agents generate SQL that the warehouse rejects as a
//! normal part of solving, and those are answered by a retry, not by a page —
//! at `warn` or above each one would also go to Sentry.

/// Upper bound on `db.query.text`, so a generated statement with a thousand
/// inlined literals does not become a megabyte attribute.
pub const QUERY_TEXT_MAX_BYTES: usize = 8 * 1024;

/// `db.operation.name`: the statement's leading keyword, upper-cased
/// (`SELECT`, `WITH`, `INSERT`, `DESCRIBE`). Leading comments and whitespace
/// are skipped; an empty or unrecognisable statement is `UNKNOWN`.
pub fn operation_name(sql: &str) -> String {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if word.is_empty() {
        "UNKNOWN".to_string()
    } else {
        word.to_ascii_uppercase()
    }
}

/// `otel.name` for a query span: `"{operation} {system}"`.
pub fn span_name(sql: &str, system: &str) -> String {
    format!("{} {system}", operation_name(sql))
}

/// `db.query.text`: the statement, cut at a char boundary under
/// [`QUERY_TEXT_MAX_BYTES`] with the original length appended when cut.
pub fn query_text(sql: &str) -> String {
    if sql.len() <= QUERY_TEXT_MAX_BYTES {
        return sql.to_string();
    }
    let cut = sql
        .char_indices()
        .take_while(|(i, _)| *i < QUERY_TEXT_MAX_BYTES)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}… ({} bytes)", &sql[..cut], sql.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operation_is_the_leading_keyword_past_comments() {
        assert_eq!(operation_name("select 1"), "SELECT");
        assert_eq!(
            operation_name("  WITH x AS (select 1) select * from x"),
            "WITH"
        );
        assert_eq!(operation_name("-- agent: revenue\nSELECT 1"), "SELECT");
        assert_eq!(
            operation_name("/* oxy */ insert into t values (1)"),
            "INSERT"
        );
        assert_eq!(operation_name("DESCRIBE orders"), "DESCRIBE");
        assert_eq!(operation_name(""), "UNKNOWN");
        assert_eq!(operation_name("(select 1)"), "UNKNOWN");
        assert_eq!(span_name("select 1", "snowflake"), "SELECT snowflake");
    }

    #[test]
    fn long_statements_are_cut_and_say_so() {
        assert_eq!(query_text("select 1"), "select 1");
        let long = format!("select '{}'", "é".repeat(QUERY_TEXT_MAX_BYTES));
        let cut = query_text(&long);
        assert!(cut.len() < long.len());
        assert!(cut.ends_with(&format!("({} bytes)", long.len())));
    }
}
