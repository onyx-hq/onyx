//! Pure helpers behind the per-host-op spans in `runtime::run` — what a
//! `ctx.query` / `ctx.fetch` / … is allowed to say about itself on the
//! platform trace.
//!
//! The rule for every attribute here: shape, never payload. A query summary
//! is a verb and a table, not the SQL (which can embed literals from user
//! input); a fetch target is a scheme, host and port, not the URL (which is
//! where API keys travel as query strings). The tenant's own store already
//! holds the content behind the app-admin gate; the operator store gets the
//! timing and the type of failure.

/// The `tracing` target of every host-op span. Platform-only: the product
/// `SpanCollectorLayer` collects every span at its level with no name filter,
/// so without this a function looping 200 queries would write 200 span rows
/// per invocation into the tenant-facing store — a store the docs describe as
/// "agent/automation spans only". `oxy_observability::observability_filter`
/// switches this target off; the OTLP trace layer keeps it.
pub(super) const HOST_CALL_TARGET: &str = "oxy::host_call";

/// A token that can be a table or verb name, bounded — anything else (a
/// 4 KB expression, an unquoted fragment) is not recorded at all. This is the
/// guarantee "never the SQL" rests on: whatever reaches a span is one
/// whitespace-delimited, identifier-shaped token of at most 64 chars, taken
/// after [`strip_string_literals`] has removed standard single-quoted
/// literals. It is not a dialect-aware lexer — a backslash-escaped quote or a
/// double-quoted literal can still end a literal early — so the bound, not
/// the stripping, is what holds.
pub(super) fn identifier_like(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '-'))
}

/// The first verb and the first table of a SQL text, for `db.operation.name`
/// and `db.collection.name`. Best-effort on a whitespace scan — a CTE reads
/// as `WITH` with no table, which is honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QuerySummary {
    pub verb: String,
    pub table: String,
}

/// `sql` past any leading whitespace, `-- …` line comments and `/* … */` block
/// comments, so the verb is the statement's first keyword rather than `--`. An
/// unterminated block comment leaves nothing.
fn skip_leading_comments(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, next)| next);
        } else {
            return rest;
        }
        rest = rest.trim_start();
    }
}

/// Replace every single-quoted string literal (`''` escapes included) with a
/// space, so a keyword inside a literal — `select 'x from secret' …` — is
/// never mistaken for the real one. Unterminated literals swallow the rest.
pub(super) fn strip_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        out.push(' ');
        loop {
            match chars.next() {
                None => return out,
                Some('\'') if chars.peek() == Some(&'\'') => {
                    chars.next();
                }
                Some('\'') => break,
                Some(_) => {}
            }
        }
    }
    out
}

pub(super) fn db_query_summary(sql: &str) -> QuerySummary {
    let sql = strip_string_literals(skip_leading_comments(sql));
    let tokens: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ';' || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    let verb = tokens
        .first()
        .map(|t| t.to_ascii_uppercase())
        .unwrap_or_default();
    let after = |keyword: &str| -> Option<String> {
        tokens
            .iter()
            .position(|t| t.eq_ignore_ascii_case(keyword))
            .and_then(|i| tokens.get(i + 1))
            .map(|t| t.trim_matches(|c| c == '`' || c == '"').to_string())
    };
    let table = match verb.as_str() {
        "SELECT" | "DELETE" => after("FROM"),
        "INSERT" | "REPLACE" => after("INTO"),
        "UPDATE" => tokens.get(1).map(|t| t.to_string()),
        "CREATE" | "DROP" | "ALTER" | "TRUNCATE" => after("TABLE"),
        _ => None,
    }
    .unwrap_or_default();
    let keep = |s: String| {
        if identifier_like(&s) {
            s
        } else {
            String::new()
        }
    };
    QuerySummary {
        verb: keep(verb),
        table: keep(table),
    }
}

/// Where a `ctx.fetch` goes — never the path or query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FetchTarget {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

pub(super) fn fetch_target(url: &str) -> FetchTarget {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => (String::new(), url),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Drop userinfo if a caller embedded credentials in the URL.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => match p.parse::<u16>() {
            Ok(port) => (h.to_string(), Some(port)),
            Err(_) => (authority.to_string(), None),
        },
        _ => (authority.to_string(), None),
    };
    FetchTarget {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    }
}

/// `error.type` for a failed host op, from the message the host returned.
/// Coarse on purpose: these become a HyperDX facet, and a facet with one
/// value per distinct message is no facet.
pub(super) fn classify_host_error(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("timed out") || m.contains("timeout") || m.contains("deadline") {
        "timeout"
    } else if m.contains("permission") || m.contains("denied") || m.contains("forbidden") {
        "permission_denied"
    } else if m.contains("not allowed") || m.contains("blocked") || m.contains("capability") {
        "not_allowed"
    } else if m.contains("not found") || m.contains("does not exist") || m.contains("no such") {
        "not_found"
    } else if m.contains("cancel") {
        "cancelled"
    } else {
        "host_call_failed"
    }
}

/// Row count and truncation flag from a `ctx.query` reply, when the reply
/// has the `{ rows: [...], truncated: bool }` shape.
pub(super) fn rows_and_truncated(value: &serde_json::Value) -> (Option<u64>, Option<bool>) {
    let rows = value
        .get("rows")
        .and_then(|r| r.as_array())
        .map(|r| r.len() as u64);
    let truncated = value.get("truncated").and_then(|t| t.as_bool());
    (rows, truncated)
}

/// The upstream status from a `ctx.fetch` reply.
pub(super) fn fetch_status(value: &serde_json::Value) -> Option<u64> {
    value.get("status").and_then(|s| s.as_u64())
}

/// The FaaS semconv trigger for an invocation `mode`.
pub(super) fn faas_trigger(mode: &str) -> &'static str {
    match mode {
        "route" => "http",
        "schedule" => "timer",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_summary_is_verb_and_table_never_the_text() {
        let s = db_query_summary("select a, b from orders where id = 'secret-42'");
        assert_eq!(s.verb, "SELECT");
        assert_eq!(s.table, "orders");
        assert_eq!(
            db_query_summary("INSERT INTO `ledger` (x) VALUES (1)").table,
            "ledger"
        );
        assert_eq!(
            db_query_summary("update Accounts set x = 1").table,
            "Accounts"
        );
        assert_eq!(db_query_summary("delete from t;").table, "t");
        let cte = db_query_summary("with x as (select 1) select * from x");
        assert_eq!(cte.verb, "WITH");
        assert_eq!(cte.table, "");
        assert_eq!(db_query_summary("").verb, "");
    }

    #[test]
    fn a_leading_comment_is_not_the_verb() {
        // The verb decides whether a write is audited (`is_write_verb`), so a
        // comment-prefixed INSERT read as verb `--` was a write with no row.
        let line = db_query_summary("-- receiving report\nINSERT INTO receipts (a) VALUES (1)");
        assert_eq!(
            (line.verb.as_str(), line.table.as_str()),
            ("INSERT", "receipts")
        );
        let block = db_query_summary("  /* nightly */ /* two */\n update ledger set x = 1");
        assert_eq!(
            (block.verb.as_str(), block.table.as_str()),
            ("UPDATE", "ledger")
        );
        assert_eq!(db_query_summary("-- only a comment").verb, "");
        assert_eq!(db_query_summary("/* unterminated insert into t").verb, "");
    }

    #[test]
    fn a_literal_after_from_is_dropped_not_recorded() {
        // The first FROM is inside a string literal in the select list: with
        // literals stripped first, the real FROM is the one that is read.
        let s = db_query_summary("select 'x from secret-42' , a from t");
        assert_eq!(s.verb, "SELECT");
        assert_eq!(s.table, "t");
        // Whitespace before the closing quote would have made `secret42` an
        // identifier-shaped token; stripping the literal removes the chance.
        assert_eq!(
            db_query_summary("select 'x from secret42 ' , a from t").table,
            "t"
        );
        assert_eq!(
            db_query_summary("select 'it''s from here' from t2").table,
            "t2"
        );
        assert_eq!(db_query_summary("select 'unterminated from x").table, "");
        assert_eq!(strip_string_literals("a 'b''c' d"), "a   d");
        let long = format!("select * from {}", "t".repeat(65));
        assert_eq!(db_query_summary(&long).table, "", "bounded at 64");
        assert_eq!(
            db_query_summary("'x' from t").verb,
            "FROM",
            "the literal contributes nothing; the next token is the verb"
        );
        assert!(identifier_like("orders_v2.$tmp-1"));
        assert!(!identifier_like("secret-42'"));
    }

    #[test]
    fn fetch_target_keeps_scheme_host_port_and_drops_the_rest() {
        let t = fetch_target("https://user:pw@api.stripe.com:8443/v1/charges?key=sk_live_123");
        assert_eq!(t.scheme, "https");
        assert_eq!(t.host, "api.stripe.com");
        assert_eq!(t.port, Some(8443));
        let t = fetch_target("http://example.test/path");
        assert_eq!(
            (t.scheme.as_str(), t.host.as_str(), t.port),
            ("http", "example.test", None)
        );
        // Not a URL at all: no scheme, and whatever came in is the "host" —
        // there is nothing sensitive to strip and nothing to panic on.
        let t = fetch_target("not a url");
        assert_eq!(t.scheme, "");
        assert_eq!(t.port, None);
    }

    #[test]
    fn host_errors_classify_coarsely() {
        assert_eq!(classify_host_error("query timed out after 30s"), "timeout");
        assert_eq!(
            classify_host_error("permission denied for table x"),
            "permission_denied"
        );
        assert_eq!(
            classify_host_error("host not allowed by allow_hosts"),
            "not_allowed"
        );
        assert_eq!(
            classify_host_error("relation \"x\" does not exist"),
            "not_found"
        );
        assert_eq!(classify_host_error("something odd"), "host_call_failed");
    }

    #[test]
    fn reply_shapes_are_read_defensively() {
        let v = serde_json::json!({ "rows": [1, 2, 3], "truncated": true });
        assert_eq!(rows_and_truncated(&v), (Some(3), Some(true)));
        assert_eq!(rows_and_truncated(&serde_json::json!("nope")), (None, None));
        assert_eq!(
            fetch_status(&serde_json::json!({ "status": 502 })),
            Some(502)
        );
        assert_eq!(fetch_status(&serde_json::json!({})), None);
    }

    #[test]
    fn faas_trigger_follows_the_invocation_mode() {
        assert_eq!(faas_trigger("route"), "http");
        assert_eq!(faas_trigger("schedule"), "timer");
        assert_eq!(faas_trigger("manual"), "other");
        assert_eq!(faas_trigger("airway"), "other");
    }
}
