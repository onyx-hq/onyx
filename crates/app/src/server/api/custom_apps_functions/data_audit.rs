//! The audit record for every data-plane write a custom-app function makes.
//!
//! Neither data plane can say *who* wrote: Neon exposes no self-serve
//! `pgaudit` or `log_statement`, and a DuckLake commit's `author` is whatever
//! the writer claims. The one place every `ctx.oltp`, `ctx.tx` and
//! `ctx.warehouse` write crosses is the host, so the host is the audit
//! record, in three layers that degrade independently:
//!
//! 1. **On the database session** — `application_name` is set per
//!    transaction to [`session_tag`] (`oxyfn:<invocation>:<trace id>`, under
//!    Postgres's 63-byte cap), so `pg_stat_activity`, Neon's query views and
//!    the OTel Postgres receiver can name the invocation behind a statement.
//! 2. **With the statement** — every SQL text travels with a sqlcommenter
//!    [`tag`]: the app, function, invocation and W3C traceparent. Where it
//!    goes is the connector's call, not the host's
//!    (`DatabaseConnector::execute_statement_tagged`), because only the
//!    connector knows what its engine reads as SQL. By default it trails the
//!    statement as a comment ([`commented`] is the same bytes, for the planes
//!    that run on their own connection), which survives into the provider's
//!    own logs and `pg_stat_activity` (`pg_stat_statements` strips comments;
//!    that view is for shapes, not audit), and which Airhouse can lift into
//!    `set_commit_message` server-side — see `customer-apps-functions.md`.
//!    ClickHouse sends it beside the SQL as `log_comment`: there everything
//!    after `VALUES` is row data, and a trailing tag failed every insert.
//! 3. **In `audit_events`** — after a write commits, one hash-chained row
//!    ([`entry`]) with the verified actor, the target schema/table, the verb,
//!    the row count and the trace id. Reads never produce one.
//!
//! None of it carries SQL text or values: the trailer and the row hold
//! identifiers, the verb and the table, exactly what the host-op spans hold.

use std::fmt::Write as _;

use oxy_app_core::audit::{ActorType, AuditEntry};
use serde_json::json;
use uuid::Uuid;

use super::host_call_attrs::{QuerySummary, identifier_like};

/// Who is running, as far as a write needs to be attributed. Built once per
/// invocation from the run arguments; the host owns it for the isolate's life.
#[derive(Debug, Clone)]
pub struct InvocationIdentity {
    pub invocation_id: Uuid,
    pub function_name: String,
    /// `route` / `schedule` / `airway` / `manual`.
    pub mode: String,
    pub request_id: Option<Uuid>,
    pub app_slug: String,
    /// The verified user when a human called the route; `None` on a schedule,
    /// an Airway step or a manual run, which execute as the platform.
    pub user_id: Option<Uuid>,
    pub user_email: Option<String>,
}

/// Postgres truncates `application_name` past `NAMEDATALEN - 1` bytes, with a
/// notice nobody reads. Everything the tag carries has to fit.
pub(super) const APPLICATION_NAME_MAX: usize = 63;

/// `oxyfn:<first 8 of the invocation id>:<32-hex trace id or ->`.
pub(super) fn session_tag(identity: &InvocationIdentity, trace_id: Option<&str>) -> String {
    let inv = identity.invocation_id.simple().to_string();
    let trace = trace_id
        .filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or("-");
    let tag = format!("oxyfn:{}:{trace}", &inv[..8]);
    debug_assert!(tag.len() <= APPLICATION_NAME_MAX);
    tag
}

/// The statement that names the session for the rest of the transaction.
/// The tag is `[a-z0-9:-]` by construction, so the literal needs no escaping.
pub(super) fn set_application_name_sql(tag: &str) -> String {
    format!("SET LOCAL application_name = '{tag}'")
}

/// The sqlcommenter pairs naming the invocation (`key='value',…`, values
/// URL-encoded). Keys are fixed; values are identifiers the platform minted.
///
/// It has no placement of its own. Hand it to
/// `DatabaseConnector::execute_statement_tagged` with the statement untouched:
/// the host once put it at the end of every statement itself, and on
/// ClickHouse that is inside the `VALUES` row stream.
pub(super) fn tag(identity: &InvocationIdentity, traceparent: Option<&str>) -> String {
    let mut out = format!(
        "oxy.app='{}',oxy.fn='{}',oxy.invocation='{}'",
        encode(&identity.app_slug),
        encode(&identity.function_name),
        identity.invocation_id.simple()
    );
    if let Some(tp) = traceparent {
        let _ = write!(out, ",traceparent='{}'", encode(tp));
    }
    out
}

/// `sql` carrying [`tag`] as a trailing comment on its own line — for
/// `ctx.oltp` and `ctx.tx`, which run on a Postgres-wire session of their own
/// rather than through a connector, and where a trailing comment is SQL. The
/// same bytes a connector's default placement produces.
pub(super) fn commented(
    sql: &str,
    identity: &InvocationIdentity,
    traceparent: Option<&str>,
) -> String {
    agentic_connector::with_trailing_comment(sql, &tag(identity, traceparent))
}

/// Percent-encode everything outside the sqlcommenter unreserved set. Keeps
/// `'`, `\` and `*/` out of the trailer whatever a value contains.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'0'..=b'9' | b'a'..=b'z' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// The plane a `ctx.tx()` destination belongs to, from its connector's
/// dialect: Airhouse speaks DuckDB over pgwire, a plain Postgres destination
/// is `postgres`, anything else is named by its dialect. `oltp` is reserved
/// for the app's own silo, which only `ctx.oltp` reaches.
pub(super) fn plane_for_dialect(dialect: agentic_connector::SqlDialect) -> &'static str {
    use agentic_connector::SqlDialect as D;
    match dialect {
        D::DuckDb => "airhouse",
        D::Postgres => "postgres",
        D::Sqlite => "sqlite",
        D::BigQuery => "bigquery",
        D::Snowflake => "snowflake",
        _ => "other",
    }
}

/// Record what a statement touched on the current host-op span, once, from
/// the summary the host already computed: the namespace (schema or
/// database), the verb and the table — never the SQL. The fields are
/// declared `Empty` by `runtime::host_call_span`.
pub(super) fn record_db_span(summary: &QuerySummary, namespace: Option<&str>, table: Option<&str>) {
    let span = tracing::Span::current();
    if let Some(ns) = namespace.filter(|ns| identifier_like(ns)) {
        span.record("db.namespace", ns);
    }
    if !summary.verb.is_empty() {
        span.record("db.query.summary", summary.verb.as_str());
    }
    let table = table
        .filter(|t| identifier_like(t))
        .unwrap_or(summary.table.as_str());
    if !table.is_empty() {
        span.record("db.collection.name", table);
    }
}

/// Fold a write into the invocation's buffer: the same (plane, namespace,
/// verb, table) becomes one record with the statements counted and the rows
/// summed, so a loop of ten thousand inserts is one line, not ten thousand.
pub(super) fn coalesce(buffer: &mut Vec<WriteRecord>, write: WriteRecord) {
    if let Some(existing) = buffer.iter_mut().find(|w| {
        w.plane == write.plane
            && w.namespace == write.namespace
            && w.verb == write.verb
            && w.table == write.table
    }) {
        existing.statements += write.statements;
        existing.rows = match (existing.rows, write.rows) {
            (Some(a), Some(b)) => Some(a + b),
            (a, None) => a,
            (None, b) => b,
        };
    } else {
        buffer.push(write);
    }
}

/// The invocation's write buffer, with the one state that matters: once
/// drained it is **closed**, and a write noted after that — a detached host
/// call that commits after the isolate was cancelled or timed out — is
/// handed back to the caller to record on its own, so no committed write
/// ever goes unaudited. The close and the check happen under one lock.
#[derive(Debug)]
pub(super) struct WriteBuffer {
    open: Option<Vec<WriteRecord>>,
}

impl WriteBuffer {
    pub fn new() -> Self {
        Self {
            open: Some(Vec::new()),
        }
    }

    /// Buffer the write, or return it when the buffer has already been
    /// drained: the caller must then record it immediately.
    pub fn note(&mut self, write: WriteRecord) -> Option<WriteRecord> {
        match &mut self.open {
            Some(buffer) => {
                coalesce(buffer, write);
                None
            }
            None => Some(write),
        }
    }

    /// Take everything buffered and close the buffer for good.
    pub fn drain(&mut self) -> Vec<WriteRecord> {
        self.open.take().unwrap_or_default()
    }

    #[cfg(test)]
    pub fn is_closed(&self) -> bool {
        self.open.is_none()
    }
}

/// Record only the namespace on the current host-op span — for a `begin`,
/// which has no statement to summarise.
pub(super) fn record_db_namespace(namespace: &str) {
    if identifier_like(namespace) {
        tracing::Span::current().record("db.namespace", namespace);
    }
}

/// A verb that changes data or schema. Reads never become an audit row.
pub(super) fn is_write_verb(verb: &str) -> bool {
    matches!(
        verb,
        "INSERT"
            | "UPDATE"
            | "DELETE"
            | "MERGE"
            | "UPSERT"
            | "REPLACE"
            | "COPY"
            | "TRUNCATE"
            | "CREATE"
            | "ALTER"
            | "DROP"
            | "GRANT"
            | "REVOKE"
    )
}

/// One write, as the audit row describes it.
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct WriteRecord {
    /// `oltp` (the app's silo), `airhouse`, `postgres`, or another
    /// dialect name — see [`plane_for_dialect`].
    pub plane: &'static str,
    /// The schema (OLTP) or database (Airhouse) the statement ran against.
    pub namespace: String,
    pub verb: String,
    /// The table the summary found, or empty when the statement had none
    /// that survived the identifier bound.
    pub table: String,
    /// Rows the statements reported, summed; `None` when the plane does not
    /// report a count.
    pub rows: Option<u64>,
    /// How many statements this record stands for (coalesced).
    pub statements: u64,
}

impl WriteRecord {
    /// `oltp:app_orders.orders`, the audit row's `target_id`.
    pub fn target(&self) -> String {
        if self.table.is_empty() || !identifier_like(&self.table) {
            format!("{}:{}", self.plane, self.namespace)
        } else {
            format!("{}:{}.{}", self.plane, self.namespace, self.table)
        }
    }
}

/// The hash-chained row for a committed write (or a committed transaction's
/// writes). The actor is the verified user when a human called the route,
/// otherwise the platform acting for the app.
pub(super) fn entry(
    action: &'static str,
    identity: &InvocationIdentity,
    org_id: Uuid,
    project_id: Uuid,
    writes: &[WriteRecord],
    trace_id: Option<&str>,
) -> AuditEntry {
    let first = writes.first();
    let mut e = match (identity.user_id, identity.user_email.as_deref()) {
        (Some(uid), email) => AuditEntry::new(email.unwrap_or("unknown").to_string(), action)
            .actor(uid, ActorType::User),
        (None, _) => {
            let mut e = AuditEntry::new(format!("system:app:{}", identity.app_slug), action);
            e.actor_type = ActorType::System;
            e
        }
    };
    e = e.org(org_id).workspace(project_id);
    if let Some(w) = first {
        e = e.target(
            format!("{}.table", w.plane),
            w.target(),
            format!("{} {}", w.verb, w.target()),
        );
    }
    e.metadata(json!({
        "app_slug": identity.app_slug,
        "function": identity.function_name,
        "invocation_id": identity.invocation_id,
        "mode": identity.mode,
        "request_id": identity.request_id,
        "trace_id": trace_id,
        "writes": writes,
    }))
}

/// Actions, one per plane and one per transaction, so a search on the action
/// column answers "who wrote to Airhouse" without parsing metadata.
pub(super) const ACTION_OLTP_WRITE: &str = "app.oltp.write";
pub(super) const ACTION_TX_COMMIT: &str = "app.tx.commit";
pub(super) const ACTION_WAREHOUSE_WRITE: &str = "app.warehouse.write";

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(human: bool) -> InvocationIdentity {
        InvocationIdentity {
            invocation_id: Uuid::parse_str("0af76519-16cd-43dd-8448-eb211c80319c").unwrap(),
            function_name: "list-settings".into(),
            mode: "route".into(),
            request_id: Some(Uuid::nil()),
            app_slug: "orders-app".into(),
            user_id: human
                .then(|| Uuid::parse_str("b7ad6b71-6920-3331-0000-000000000001").unwrap()),
            user_email: human.then(|| "ana@example.test".to_string()),
        }
    }

    #[test]
    fn the_session_tag_fits_application_name_and_names_the_invocation() {
        let tag = session_tag(&identity(true), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!(tag, "oxyfn:0af76519:4bf92f3577b34da6a3ce929d0e0e4736");
        assert!(tag.len() <= APPLICATION_NAME_MAX);
        assert_eq!(session_tag(&identity(true), None), "oxyfn:0af76519:-");
        assert_eq!(
            session_tag(&identity(true), Some("not-a-trace")),
            "oxyfn:0af76519:-"
        );
        assert_eq!(
            set_application_name_sql(&tag),
            "SET LOCAL application_name = 'oxyfn:0af76519:4bf92f3577b34da6a3ce929d0e0e4736'"
        );
    }

    #[test]
    fn the_trailer_rides_on_its_own_line_and_encodes_hostile_values() {
        let mut id = identity(true);
        id.app_slug = "x'*/; drop table t --".into();
        let out = commented(
            "select 1 -- trailing comment",
            &id,
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        let (sql, trailer) = out.split_once("\n/*").expect("trailer on a new line");
        assert_eq!(sql, "select 1 -- trailing comment");
        assert!(trailer.ends_with("*/"));
        assert!(
            !trailer[..trailer.len() - 2].contains("*/"),
            "no early close"
        );
        assert_eq!(
            trailer.matches('\'').count(),
            8,
            "four values, each quoted once, nothing inside"
        );
        assert!(trailer.contains("oxy.app='x%27%2A%2F%3B%20drop%20table%20t%20--'"));
        assert!(trailer.contains("oxy.fn='list-settings'"));
        assert!(trailer.contains("oxy.invocation='0af7651916cd43dd8448eb211c80319c'"));
        assert!(
            trailer
                .contains("traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'")
        );
        assert!(!commented("select 1;", &id, None).contains("traceparent"));
    }

    #[test]
    fn only_writes_are_audited() {
        for v in [
            "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP", "TRUNCATE", "COPY",
        ] {
            assert!(is_write_verb(v), "{v}");
        }
        for v in ["SELECT", "WITH", "SHOW", "EXPLAIN", "SET", ""] {
            assert!(!is_write_verb(v), "{v}");
        }
    }

    #[test]
    fn a_human_write_is_attributed_to_the_user_and_a_scheduled_one_to_the_app() {
        let w = WriteRecord {
            plane: "oltp",
            namespace: "app_orders_app".into(),
            verb: "INSERT".into(),
            table: "orders".into(),
            rows: Some(3),
            statements: 1,
        };
        let human = entry(
            ACTION_OLTP_WRITE,
            &identity(true),
            Uuid::nil(),
            Uuid::nil(),
            std::slice::from_ref(&w),
            Some("abc"),
        );
        assert_eq!(human.action, "app.oltp.write");
        assert_eq!(human.actor_email, "ana@example.test");
        assert!(matches!(human.actor_type, ActorType::User));
        assert_eq!(
            human.target_id.as_deref(),
            Some("oltp:app_orders_app.orders")
        );
        assert_eq!(human.target_type.as_deref(), Some("oltp.table"));
        assert_eq!(human.metadata["writes"][0]["rows"], 3);
        assert_eq!(human.metadata["trace_id"], "abc");
        assert_eq!(human.metadata["function"], "list-settings");

        let system = entry(
            ACTION_TX_COMMIT,
            &identity(false),
            Uuid::nil(),
            Uuid::nil(),
            &[w],
            None,
        );
        assert_eq!(system.actor_email, "system:app:orders-app");
        assert!(matches!(system.actor_type, ActorType::System));
        assert!(system.actor_user_id.is_none());
        assert!(system.metadata["trace_id"].is_null());
    }

    #[test]
    fn a_target_without_a_bounded_table_names_only_the_namespace() {
        let w = WriteRecord {
            plane: "airhouse",
            namespace: "warehouse".into(),
            verb: "CREATE".into(),
            table: String::new(),
            rows: None,
            statements: 1,
        };
        assert_eq!(w.target(), "airhouse:warehouse");
    }

    #[test]
    fn a_transaction_on_airhouse_is_named_by_its_plane() {
        use agentic_connector::SqlDialect;
        assert_eq!(plane_for_dialect(SqlDialect::DuckDb), "airhouse");
        assert_eq!(plane_for_dialect(SqlDialect::Postgres), "postgres");
        let w = WriteRecord {
            plane: plane_for_dialect(SqlDialect::DuckDb),
            namespace: "warehouse".into(),
            verb: "UPDATE".into(),
            table: "orders".into(),
            rows: Some(2),
            statements: 1,
        };
        let e = entry(
            ACTION_TX_COMMIT,
            &identity(true),
            Uuid::nil(),
            Uuid::nil(),
            std::slice::from_ref(&w),
            None,
        );
        assert_eq!(e.target_id.as_deref(), Some("airhouse:warehouse.orders"));
        assert_eq!(e.target_type.as_deref(), Some("airhouse.table"));
    }

    #[test]
    fn a_write_noted_after_the_drain_is_handed_back_to_be_recorded_now() {
        let mk = || WriteRecord {
            plane: "oltp",
            namespace: "app_x".into(),
            verb: "INSERT".into(),
            table: "orders".into(),
            rows: Some(1),
            statements: 1,
        };
        let mut buf = WriteBuffer::new();
        assert!(buf.note(mk()).is_none(), "buffered while open");
        assert!(buf.note(mk()).is_none());
        let drained = buf.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].statements, 2);
        assert!(buf.is_closed());
        // The late write — a detached host call committing after cancel —
        // is not swallowed: the caller gets it back to record immediately.
        let late = buf.note(mk()).expect("returned, not buffered");
        assert_eq!(late.table, "orders");
        assert!(
            buf.drain().is_empty(),
            "a second drain finds nothing and stays closed"
        );
        assert!(buf.is_closed());
    }

    #[test]
    fn writes_to_the_same_target_coalesce_into_one_record() {
        let mk = |verb: &str, table: &str, rows: Option<u64>| WriteRecord {
            plane: "oltp",
            namespace: "app_x".into(),
            verb: verb.into(),
            table: table.into(),
            rows,
            statements: 1,
        };
        let mut buf = Vec::new();
        for _ in 0..1000 {
            coalesce(&mut buf, mk("INSERT", "orders", Some(1)));
        }
        coalesce(&mut buf, mk("INSERT", "orders", None));
        coalesce(&mut buf, mk("UPDATE", "orders", Some(4)));
        coalesce(&mut buf, mk("INSERT", "lines", Some(2)));
        assert_eq!(buf.len(), 3);
        assert_eq!(buf[0].statements, 1001);
        assert_eq!(buf[0].rows, Some(1000), "a None leaves the sum alone");
        assert_eq!(buf[1].verb, "UPDATE");
        assert_eq!(buf[2].table, "lines");
        let e = entry(
            ACTION_OLTP_WRITE,
            &identity(false),
            Uuid::nil(),
            Uuid::nil(),
            &buf,
            None,
        );
        assert_eq!(e.metadata["writes"][0]["statements"], 1001);
    }
}
