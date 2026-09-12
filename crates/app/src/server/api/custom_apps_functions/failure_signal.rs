//! What a failed invocation says on the platform's side.
//!
//! In 0.5.140–0.5.144 every `ctx.warehouse.insert` against ClickHouse failed,
//! and every failure was recorded — in `app_function_invocations.error` and the
//! tenant-facing ClickHouse sink. Neither is a place anyone on call looks. No
//! platform log line named the failure, the invocation span was never marked
//! failed, and nothing paged; a customer reported it, and a first-pass
//! investigation that searched the platform logs found nothing and guessed.
//!
//! This is the shape of a failure the platform can hold without holding its
//! message: a coarse [`Failure::kind`] (a HyperDX facet, a Slack line) and a
//! [`fingerprint`] that names "the same failure" across invocations and apps.
//! The message itself stays in the tenant's store, behind the app-admin gate —
//! the same rule `host_call_attrs` keeps for host-op spans. A fingerprint is
//! pseudonymous rather than anonymous: it hides the message, but someone who
//! already holds a message and a guessed value can check the guess against it.

use sha2::{Digest, Sha256};

/// How an invocation failed, with nothing of what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Failure {
    /// `threw` (the app's code threw) | `internal` (the runtime failed) |
    /// `platform` (the invocation never reached the app's code) | `timeout` |
    /// `http_5xx`. Bounded on purpose: a facet with one value per message is no
    /// facet.
    pub kind: &'static str,
    /// See [`fingerprint`].
    pub fingerprint: String,
}

impl Failure {
    /// The failure an invocation's outcome describes, or `None` when it did not
    /// fail. A cancellation is someone asking it to stop, not the app failing.
    /// A handler that caught an error and answered 5xx did fail — that is how a
    /// broken host call looks from an app that handles its errors.
    pub fn of(status: &str, http_status: u16, error: Option<&str>) -> Option<Self> {
        let (kind, fingerprint) = match status {
            "cancelled" => return None,
            "success" if http_status < 500 => return None,
            // Digested whole, not normalized: the status IS the signature, and
            // normalizing would fold a new 500 into a function's usual 503.
            "success" => ("http_5xx", digest(&format!("http status {http_status}"))),
            "timeout" => ("timeout", Self::timeout_fingerprint()),
            _ => {
                let message = error.unwrap_or_default();
                (kind_of_error(message), fingerprint(message))
            }
        };
        Some(Self { kind, fingerprint })
    }

    /// The fingerprint every timeout shares, for the reaper, which marks
    /// orphaned rows `timeout` without running [`Failure::of`] per row.
    pub fn timeout_fingerprint() -> String {
        digest("timeout")
    }

    /// Mark the enclosing invocation span failed and put one WARN line on the
    /// platform log. Call inside the `custom_app_function` span: the line then
    /// carries that span's app, function, invocation and request ids, which is
    /// how on-call gets from HyperDX to the invocation row and its message.
    #[cfg(feature = "custom-app-functions")]
    pub fn report(&self) {
        let span = tracing::Span::current();
        span.record("otel.status_code", "ERROR");
        span.record("error.type", self.kind);
        // Braced: `type` is a keyword, so the field is a string literal, and
        // unbraced the macro cannot tell a literal field from the message.
        tracing::warn!(
            target: "oxy::app_function",
            { error.fingerprint = %self.fingerprint, "error.type" = %self.kind },
            "custom-app function invocation failed"
        );
    }
}

/// Who an `error` status belongs to, from the prefix `RuntimeError` puts on the
/// message. Anything without one failed before the isolate ran — a workspace
/// or build lookup, a missing function — which is the platform's, not the app's.
fn kind_of_error(message: &str) -> &'static str {
    if message.starts_with("function threw:") {
        "threw"
    } else if message.starts_with("internal runtime error") {
        "internal"
    } else {
        "platform"
    }
}

/// Bytes of normalized message that feed the digest. Past this, messages that
/// share a prefix are the same failure; stack traces and quoted rows vary
/// further down without saying anything new.
const FINGERPRINT_PREFIX: usize = 240;

/// A stable name for "the same failure": 16 hex chars of a digest over the
/// message with its data taken out.
///
/// Two invocations that failed the same way differ in everything the message
/// says about the data — quoted values, row numbers, ids, the rows ClickHouse
/// quotes back. Quoted runs become `?` and every word containing a digit
/// becomes `#` before hashing, so the Code 27 insert failure is one
/// fingerprint across every invocation and every app it hit.
pub(super) fn fingerprint(message: &str) -> String {
    let normalized = normalize(message);
    let end = normalized
        .char_indices()
        .nth(FINGERPRINT_PREFIX)
        .map_or(normalized.len(), |(i, _)| i);
    digest(&normalized[..end])
}

fn digest(input: &str) -> String {
    hex::encode(&Sha256::digest(input.as_bytes())[..8])
}

fn normalize(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(FINGERPRINT_PREFIX * 2));
    let mut chars = message.chars().peekable();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.chars().any(|c| c.is_ascii_digit()) {
            out.push('#');
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            // A quote opens a quoted run only where a value could start: after
            // a word it is an apostrophe (`doesn't`), and treating it as a quote
            // would swallow the rest of the message.
            '\'' | '"' | '`' if word.is_empty() => {
                flush(&mut word, &mut out);
                skip_quoted(c, &mut chars);
                out.push('?');
            }
            c if c.is_alphanumeric() || c == '_' => word.push(c),
            c if c.is_whitespace() => {
                flush(&mut word, &mut out);
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            c => {
                flush(&mut word, &mut out);
                out.push(c);
            }
        }
    }
    flush(&mut word, &mut out);
    out.trim().to_string()
}

/// Consume a quoted run up to its closing `quote`, honouring `\`-escapes and a
/// doubled quote. An unterminated run swallows the rest, which is still one `?`.
fn skip_quoted(quote: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == quote {
            if chars.peek() == Some(&quote) {
                chars.next();
            } else {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error every ClickHouse insert returned in 0.5.140–0.5.144, as two
    /// different invocations of two different apps saw it.
    const CODE_27_A: &str = "function threw: Error: warehouse insert failed: query failed: HTTP 400 \
        Bad Request: Code: 27. DB::Exception: Cannot parse input: expected '(' before: \
        '/*oxy.app=\\'bookkeeping\\',oxy.fn=\\'upload-report\\',oxy.invocation=\\'0af7651916cd\\'*/': \
        at row 63: While executing ValuesBlockInputFormat. (CANNOT_PARSE_INPUT_ASSERTION_FAILED) \
        (version 25.8.4.13 (official build))";
    const CODE_27_B: &str = "function threw: Error: warehouse insert failed: query failed: HTTP 400 \
        Bad Request: Code: 27. DB::Exception: Cannot parse input: expected '(' before: \
        '/*oxy.app=\\'receiving\\',oxy.fn=\\'submit\\',oxy.invocation=\\'884e14953bc3\\'*/': \
        at row 2: While executing ValuesBlockInputFormat. (CANNOT_PARSE_INPUT_ASSERTION_FAILED) \
        (version 25.8.4.13 (official build))";

    #[test]
    fn the_same_failure_in_different_apps_and_rows_is_one_fingerprint() {
        assert_eq!(fingerprint(CODE_27_A), fingerprint(CODE_27_B));
        assert_eq!(fingerprint(CODE_27_A).len(), 16);
    }

    #[test]
    fn a_different_failure_is_a_different_fingerprint() {
        let missing_table = "function threw: Error: warehouse insert failed: query failed: HTTP 404 \
            Not Found: Code: 60. DB::Exception: Table default.receiving does not exist. \
            (UNKNOWN_TABLE) (version 25.8.4.13 (official build))";
        assert_ne!(fingerprint(CODE_27_A), fingerprint(missing_table));
        assert_ne!(
            fingerprint("function threw: Error: name is required"),
            fingerprint("function threw: Error: partySize is required")
        );
    }

    #[test]
    fn the_data_in_a_message_does_not_reach_the_fingerprint_input() {
        let normalized = normalize(CODE_27_A);
        for leaked in ["bookkeeping", "upload-report", "0af7651916cd", "63"] {
            assert!(!normalized.contains(leaked), "{leaked} in {normalized}");
        }
        assert_eq!(
            normalize("row 'it''s' at \"col\" id 4bf92f35 uuid 0b9f4c7e-1d2a"),
            "row ? at ? id # uuid #-#"
        );
        // An apostrophe is not a quote: the words after it still count.
        assert_ne!(
            fingerprint("Table t doesn't exist. (UNKNOWN_TABLE)"),
            fingerprint("Table t doesn't exist. (UNKNOWN_DATABASE)")
        );
    }

    #[test]
    fn only_a_failed_outcome_is_a_failure() {
        assert_eq!(Failure::of("success", 200, None), None);
        assert_eq!(
            Failure::of("success", 404, None),
            None,
            "a 4xx is an answer"
        );
        assert_eq!(
            Failure::of("cancelled", 0, Some("function was cancelled")),
            None
        );

        let caught = Failure::of("success", 500, None).expect("a 5xx is a failure");
        assert_eq!(caught.kind, "http_5xx");
        assert_ne!(
            caught.fingerprint,
            Failure::of("success", 503, None).unwrap().fingerprint
        );

        assert_eq!(
            Failure::of("timeout", 0, Some("function execution timed out"))
                .unwrap()
                .kind,
            "timeout"
        );
        assert_eq!(
            Failure::of("error", 0, Some("internal runtime error: isolate died"))
                .unwrap()
                .kind,
            "internal"
        );
        let threw = Failure::of("error", 0, Some(CODE_27_A)).unwrap();
        assert_eq!(threw.kind, "threw");
        assert_eq!(threw.fingerprint, fingerprint(CODE_27_B));
        // Never reached the app's code: the platform's failure, not the app's.
        assert_eq!(
            Failure::of(
                "error",
                0,
                Some("workspace lookup failed: connection refused")
            )
            .unwrap()
            .kind,
            "platform"
        );
        assert_eq!(
            Failure::of("timeout", 0, None).unwrap().fingerprint,
            Failure::timeout_fingerprint(),
            "the reaper's timeouts group with the runtime's"
        );
    }
}
