//! `oxy assume` — start / inspect / end an assume-role session from the CLI.
//!
//! A thin client over the existing `/api/assume` surface
//! (`server::api::admin::assume`); this file adds **no** authorization of its
//! own. The server decides who may act as whom (`may_act_as`: Oxy staff for any
//! org, a partner for an assigned client with `develop_apps`), bounds the
//! session, and audits both ends.
//!
//! Three properties are worth knowing before you use it, and each one is
//! printed as well as documented here:
//!
//! * **60 minutes, NOT renewable.** Re-starting the same org returns the
//!   *existing* session rather than extending it. A longer investigation leaves
//!   a trail of deliberate re-entries — that's the design, not a limitation.
//! * **It is your account that acts, not this terminal.** Sessions hang off your
//!   user id, so starting one here also puts your browser into the tenant, and
//!   `oxy assume end` gets you out of both.
//! * **Acting closes the staff surface.** Every `/admin/*` route 403s while a
//!   session is live. Ending is deliberately NOT behind that guard —
//!   `/api/assume` is mounted outside `/admin` precisely so the exit never sits
//!   behind the door it locks — so `oxy assume end` always works.

use clap::Parser;
use oxy::theme::StyledText;
use oxy_shared::errors::OxyError;
use serde_json::{Value, json};
use uuid::Uuid;

use super::app_manifest::{ResolvedEnv, load_for_target_resolution, resolve_env};
use super::assume_org::{get_rows, resolve_org};
use super::http;

/// Mirrors `server::api::admin::assume::MAX_SESSION`. Duplicated as a display
/// string only — the server is the authority; this is what we tell the operator
/// so expiry is never a surprise.
const MAX_SESSION_MINUTES: i64 = 60;

#[derive(Parser, Debug)]
pub struct AssumeArgs {
    #[clap(subcommand)]
    pub command: AssumeCommand,
}

#[derive(Parser, Debug)]
pub enum AssumeCommand {
    /// Begin acting as an organization. Bounded to 60 minutes and NOT
    /// renewable — re-running returns the existing session instead of
    /// extending it. A reason is required and is written to the audit log.
    Start(StartArgs),
    /// Show the assume-role session(s) currently live for your account,
    /// with the time left on each.
    Status(SessionArgs),
    /// Stop acting. Ends the session for one org, or all of them with
    /// `--all`. Always reachable — `/api/assume` sits outside the `/admin`
    /// surface that acting closes.
    End(EndArgs),
}

/// The `--env` / `--target` / `--token-env` trio every subcommand shares.
#[derive(Parser, Debug)]
pub struct SessionArgs {
    /// Environment to act on: a name (`local`, `dev`, `staging`,
    /// `production`, or a key in `oxy-app.json` `environments`) OR a URL you
    /// pasted from the browser — `https://app.oxygen-hq.com`,
    /// `https://poke-house.oxygen-hq.com`. An org URL also tells `--org`
    /// which organization you mean.
    #[arg(long, default_value = "production", value_name = "NAME|URL")]
    pub env: String,
    /// Explicit oxy base URL; overrides `--env`. Used verbatim.
    #[arg(long)]
    pub target: Option<String>,
    /// Name of the env var holding the bearer token. Default: OXY_TOKEN.
    /// Falls back to the `oxy login` cache for the target host.
    #[arg(long = "token-env", default_value = "OXY_TOKEN")]
    pub token_env: String,
}

#[derive(Parser, Debug)]
pub struct StartArgs {
    /// The organization to act as: a slug (`poke-house`), an org UUID, or a
    /// URL that names one (`https://poke-house.oxygen-hq.com`). Optional when
    /// `--env` is itself an org URL.
    #[arg(long, value_name = "SLUG|UUID|URL")]
    pub org: Option<String>,
    /// Why you are acting as this org. Required — it is recorded in the
    /// impersonation log, and an unexplained impersonation is a red flag.
    #[arg(long, short = 'r')]
    pub reason: String,
    #[clap(flatten)]
    pub session: SessionArgs,
}

#[derive(Parser, Debug)]
pub struct EndArgs {
    /// The organization to stop acting as (slug, UUID, or URL). Defaults to
    /// every live session; pass `--all` to say so explicitly.
    #[arg(long, value_name = "SLUG|UUID|URL", conflicts_with = "all")]
    pub org: Option<String>,
    /// End every live session for your account.
    #[arg(long)]
    pub all: bool,
    #[clap(flatten)]
    pub session: SessionArgs,
}

/// Resolved target + bearer — everything a request needs.
pub(super) struct Connection {
    pub(super) target: String,
    pub(super) token: String,
    pub(super) client: reqwest::Client,
    /// Org slug the `--env` / `--target` value carried, if any.
    org_hint: Option<String>,
}

fn connect(session: &SessionArgs) -> Result<Connection, OxyError> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let manifest = load_for_target_resolution(&cwd, session.target.as_deref())?;
    let ResolvedEnv { target, org_slug } =
        resolve_env(manifest.as_ref(), Some(&session.env), session.target.as_deref()).ok_or_else(
            || {
                OxyError::ConfigurationError(format!(
                    "could not resolve a target for --env {}. Pass a URL (--env https://app.oxygen-hq.com), a known env name, or --target <url>.",
                    session.env
                ))
            },
        )?;
    let token = http::resolve_bearer(&target, &session.token_env, &session.env)?;
    Ok(Connection {
        target: target.trim_end_matches('/').to_string(),
        token,
        client: http::http_client()?,
        org_hint: org_slug,
    })
}

impl Connection {
    /// Build a connection from an already-resolved target + token — the login
    /// path has both in hand and no `--env` org hint to carry. Lets it reuse
    /// `start_session` (which takes `&Connection`) without a second HTTP client.
    pub(super) fn from_parts(target: &str, token: &str) -> Result<Self, OxyError> {
        Ok(Self {
            target: target.trim_end_matches('/').to_string(),
            token: token.to_string(),
            client: http::http_client()?,
            org_hint: None,
        })
    }
}

// ── output ─────────────────────────────────────────────────────────────────

fn field<'a>(session: &'a Value, key: &str) -> &'a str {
    session.get(key).and_then(Value::as_str).unwrap_or("")
}

/// One line per live session: who you're acting as and how long you have left.
fn describe(session: &Value) -> String {
    let name = field(session, "org_name");
    let slug = field(session, "org_slug");
    let label = match (name.is_empty(), slug.is_empty()) {
        (false, false) => format!("{name} ({slug})"),
        (false, true) => name.to_string(),
        (true, false) => slug.to_string(),
        (true, true) => field(session, "org_id").to_string(),
    };
    let mins = session
        .get("expires_in_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        / 60;
    format!(
        "{label} — expires {} ({mins} min left)",
        field(session, "expires_at")
    )
}

/// The three things an operator must not be surprised by.
fn print_session_rules(target: &str) {
    println!(
        "{}",
        format!(
            "The session lasts {MAX_SESSION_MINUTES} minutes and is NOT renewable — re-running `assume start` returns this same session rather than extending it."
        )
        .warning()
    );
    println!(
        "{}",
        "It belongs to your account, not this terminal: your browser on the same login is acting as this org too."
            .tertiary()
    );
    println!(
        "{}",
        format!(
            "While it is live, the Oxy staff surface ({target}/api/admin/*) refuses you. `oxy assume end` restores it."
        )
        .tertiary()
    );
}

// ── handlers ───────────────────────────────────────────────────────────────

/// Start a session against an already-resolved connection. Shared with
/// `oxy login --assume`, which has a token in hand and no reason to re-resolve.
pub async fn start_session(
    conn: &Connection,
    org: Option<&str>,
    reason: &str,
) -> Result<(), OxyError> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(OxyError::ConfigurationError(
            "--reason must not be empty: it is recorded in the impersonation log.".into(),
        ));
    }
    let org_id = resolve_org(conn, org).await?;

    let resp = conn
        .client
        .post(format!("{}/api/assume", conn.target))
        .bearer_auth(&conn.token)
        .json(&json!({ "org_id": org_id, "reason": reason }))
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("request to {} failed: {e}", conn.target)))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(assume_error(status, org_id));
    }
    let session: Value = resp
        .json()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("read assume response: {e}")))?;

    println!(
        "{}",
        format!("Now acting as {} on {}.", describe(&session), conn.target).success()
    );
    let slug = field(&session, "org_slug");
    if !slug.is_empty() {
        println!("{}", format!("Surface: {}/{slug}", conn.target).text());
    }
    print_session_rules(&conn.target);
    Ok(())
}

/// Map the server's status codes onto the reason it actually refused, so an
/// operator isn't left guessing at a bare 403.
fn assume_error(status: reqwest::StatusCode, org_id: Uuid) -> OxyError {
    let detail = match status.as_u16() {
        403 => {
            "you are not allowed to act as this org (Oxy staff may act as any org; a partner only as an assigned client, and only with `develop_apps`)"
        }
        404 => "no organization with that id exists on this deployment",
        400 => "the request was rejected — a non-empty --reason is required",
        401 => "not authenticated — run `oxy login` for this target",
        _ => "the server refused the request",
    };
    OxyError::RuntimeError(format!(
        "could not start an assume-role session for {org_id}: {detail} ({})",
        status.as_u16()
    ))
}

pub async fn handle_assume_command(args: AssumeArgs) -> Result<(), OxyError> {
    // Mirror `oxy publish`: pick up a laptop's shell exports.
    dotenv::from_filename(".env.local").ok();
    dotenv::dotenv().ok();

    match args.command {
        AssumeCommand::Start(a) => {
            let conn = connect(&a.session)?;
            // An explicit `--org` wins; otherwise the org an `--env` URL named.
            let org = a.org.as_deref().or(conn.org_hint.as_deref());
            start_session(&conn, org, &a.reason).await
        }
        AssumeCommand::Status(a) => status(&connect(&a)?).await,
        AssumeCommand::End(a) => {
            let conn = connect(&a.session)?;
            // `--all` is the explicit spelling of the default (no org = end
            // everything), so it simply suppresses the `--env` org hint.
            let org = if a.all {
                None
            } else {
                a.org.as_deref().or(conn.org_hint.as_deref())
            };
            end(&conn, org).await
        }
    }
}

async fn status(conn: &Connection) -> Result<(), OxyError> {
    let rows = get_rows(conn, "/api/assume/current", &[])
        .await
        .ok_or_else(|| {
        OxyError::RuntimeError(format!(
            "could not read assume-role sessions from {}. Is the token still valid? Try `oxy login`.",
            conn.target
        ))
    })?;

    if rows.is_empty() {
        println!(
            "{}",
            format!("Not acting as any organization on {}.", conn.target).text()
        );
        return Ok(());
    }
    println!(
        "{}",
        format!(
            "Acting as {} organization(s) on {}:",
            rows.len(),
            conn.target
        )
        .success()
    );
    for row in &rows {
        println!("{}", format!("  {}", describe(row)).text());
    }
    print_session_rules(&conn.target);
    Ok(())
}

async fn end(conn: &Connection, org: Option<&str>) -> Result<(), OxyError> {
    // `--org` is optional here: no org means "end everything", which is what
    // the DELETE endpoint does when `org_id` is omitted.
    //
    // Resolving a slug still works mid-session even though the staff directory
    // is closed while acting: `GET /api/orgs` deliberately includes the orgs you
    // are currently acting as, so the org you need to name to get *out* is
    // always in the list you can still read.
    let org_id = match org {
        Some(_) => Some(resolve_org(conn, org).await?),
        None => None,
    };

    let mut req = conn
        .client
        .delete(format!("{}/api/assume", conn.target))
        .bearer_auth(&conn.token);
    if let Some(id) = org_id {
        req = req.query(&[("org_id", id.to_string())]);
    }

    let status = req
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("request to {} failed: {e}", conn.target)))?
        .status();
    if !status.is_success() {
        return Err(OxyError::RuntimeError(format!(
            "could not end the assume-role session ({}). `/api/assume` sits outside the admin surface, so this should not be blocked by acting — check the token.",
            status.as_u16()
        )));
    }

    let what = match org_id {
        Some(id) => format!("org {id}"),
        None => "every live session".to_string(),
    };
    println!(
        "{}",
        format!(
            "Stopped acting as {what} on {}. Staff access restored.",
            conn.target
        )
        .success()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_prefers_name_and_slug_but_degrades() {
        let s = json!({
            "org_name": "Poke House",
            "org_slug": "poke-house",
            "expires_at": "2026-07-24T12:00:00Z",
            "expires_in_seconds": 3540,
        });
        let out = describe(&s);
        assert!(out.starts_with("Poke House (poke-house)"), "{out}");
        assert!(out.contains("59 min left"), "{out}");

        let bare = json!({ "org_id": "abc", "expires_at": "", "expires_in_seconds": 0 });
        assert!(describe(&bare).starts_with("abc"));
    }

    #[test]
    fn the_bound_we_advertise_matches_the_server() {
        // `server::api::admin::assume` pins MAX_SESSION == 60 in its own test;
        // this pins that the CLI tells the operator the same number.
        assert_eq!(MAX_SESSION_MINUTES, 60);
    }
}
