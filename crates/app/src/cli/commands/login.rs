//! `oxy login` / `oxy logout` — browser-based CLI auth for `oxy publish`.
//!
//! Mirrors the `gh`/`fly`/`vercel` loopback pattern: bind an ephemeral
//! `127.0.0.1` port, open the browser to `<target>/cli-auth?port&state`, and
//! capture the session token the web app hands back to the loopback. The
//! token is the existing JWT (no new minting endpoint); the web `/cli-auth`
//! page reads it from the logged-in session and redirects to the loopback.
//!
//! After capturing the token we call `GET /api/user` and report whether the
//! user is an **app-admin** — i.e. whether they can actually `oxy publish` —
//! so they find out at login time, not on a 403 later.
//!
//! Works identically against a local `oxy serve` (loopback both ways) and
//! the cloud. Tokens are cached per target host in `~/.config/oxy/credentials.json`.

use std::collections::HashMap;
use std::path::PathBuf;

use clap::Parser;
use oxy::theme::StyledText;
use oxy_shared::errors::OxyError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::app_manifest::{ResolvedEnv, load_for_target_resolution, resolve_env, resolve_target};

#[derive(Parser, Debug)]
pub struct LoginArgs {
    /// Environment(s) to authenticate against. Repeat the flag or use a
    /// comma-separated list to log into several at once
    /// (`--env dev --env staging`, or `--env dev,staging,production`).
    /// Each env resolves a target URL via oxy-app.json `environments`, the
    /// built-in defaults, or — when the value is a URL — the URL itself
    /// (`--env https://app.oxygen-hq.com`, `--env https://poke-house.oxygen-hq.com`).
    /// The browser opens once per env in sequence. Default: `production`.
    #[arg(long, action = clap::ArgAction::Append, value_delimiter = ',', value_name = "NAME|URL")]
    env: Vec<String>,
    /// Explicit oxy base URL; overrides `--env`. Only meaningful when a
    /// single env is given (or when `--env` is omitted altogether).
    #[arg(long)]
    target: Option<String>,
    /// After logging in, immediately start an assume-role session for this
    /// organization (slug, org UUID, or an org URL). Requires `--reason`.
    /// The session lasts 60 minutes and is NOT renewable; end it with
    /// `oxy assume end`. Single-env only. Pass a bare `--assume` (no value) to
    /// act as the org an `--env` URL already names, e.g.
    /// `oxy login --env https://poke-house.oxygen-hq.com --assume -r "triage"`.
    #[arg(
        long,
        value_name = "SLUG|UUID|URL",
        num_args = 0..=1,
        default_missing_value = "",
        requires = "reason"
    )]
    assume: Option<String>,
    /// Why you are acting as the org — recorded in the impersonation log.
    /// Only valid with `--assume`.
    #[arg(long, short = 'r', requires = "assume")]
    reason: Option<String>,
}

#[derive(Parser, Debug)]
pub struct LogoutArgs {
    /// Environment(s) whose cached token to clear. Same multi-value
    /// syntax as `oxy login --env`. Default: `production`.
    #[arg(long, action = clap::ArgAction::Append, value_delimiter = ',')]
    env: Vec<String>,
    /// Explicit oxy base URL; overrides `--env`.
    #[arg(long)]
    target: Option<String>,
}

/// Resolve the list of env names to operate on, defaulting to
/// `production` when `--env` is omitted. Lets the handler treat
/// single- and multi-env invocations uniformly.
fn envs_with_default(envs: &[String]) -> Vec<String> {
    if envs.is_empty() {
        vec!["production".to_string()]
    } else {
        envs.iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

// ── credentials cache ──────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct CredentialStore(HashMap<String, HostCredential>);

#[derive(Debug, Serialize, Deserialize)]
struct HostCredential {
    token: String,
    email: String,
    is_app_admin: bool,
}

fn credentials_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oxy").join("credentials.json"))
}

/// Cache key for a target: host[:port], so dev/prod/local tokens are
/// stored separately.
fn host_key(target: &str) -> String {
    url::Url::parse(target)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_else(|| target.trim_end_matches('/').to_string())
}

fn read_store() -> CredentialStore {
    credentials_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_store(store: &CredentialStore) -> Result<(), OxyError> {
    let path = credentials_path()
        .ok_or_else(|| OxyError::RuntimeError("no config dir available".into()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OxyError::RuntimeError(format!("mkdir {}: {e}", parent.display())))?;
    }
    let json = serde_json::to_string_pretty(store)
        .map_err(|e| OxyError::RuntimeError(format!("serialize credentials: {e}")))?;
    std::fs::write(&path, json)
        .map_err(|e| OxyError::RuntimeError(format!("write {}: {e}", path.display())))?;
    // The file holds bearer tokens — restrict to the owner on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Cached token for `target`, if any. Used by `oxy publish` as the auth
/// fallback when `OXY_TOKEN` isn't set.
pub fn load_token(target: &str) -> Option<String> {
    read_store()
        .0
        .get(&host_key(target))
        .map(|c| c.token.clone())
        .filter(|t| !t.is_empty())
}

// ── login flow ─────────────────────────────────────────────────────────────

pub async fn handle_login_command(args: LoginArgs) -> Result<(), OxyError> {
    // Strict unless `--target` was passed: the manifest's `environments` pick
    // the target, so a broken file would log you in somewhere else.
    let manifest = load_for_target_resolution(
        &std::env::current_dir().unwrap_or_default(),
        args.target.as_deref(),
    )?;
    let envs = envs_with_default(&args.env);

    // `--target` only makes sense with a single env. With several envs
    // the target list comes from oxy-app.json `environments` (or the
    // built-in defaults), and a single override would silently apply
    // to all of them — confusing. Refuse early.
    if envs.len() > 1 && args.target.is_some() {
        return Err(OxyError::ConfigurationError(
            "--target is only valid when logging into a single env".into(),
        ));
    }
    // Same reasoning for `--assume`: one session names one org on one
    // deployment, so a multi-env login has no single answer.
    if envs.len() > 1 && args.assume.is_some() {
        return Err(OxyError::ConfigurationError(
            "--assume is only valid when logging into a single env".into(),
        ));
    }

    // Resolve every target up-front so we fail fast on a typo before
    // popping any browser windows.
    let targets: Vec<(String, ResolvedEnv)> = envs
        .iter()
        .map(|env| {
            resolve_env(manifest.as_ref(), Some(env), args.target.as_deref())
                .map(|t| (env.clone(), t))
                .ok_or_else(|| {
                    OxyError::ConfigurationError(format!(
                        "could not resolve a target for --env {env}. Pass a URL (--env https://app.oxygen-hq.com), --target <url>, or add it to oxy-app.json environments."
                    ))
                })
        })
        .collect::<Result<_, _>>()?;

    if targets.len() > 1 {
        println!(
            "{}",
            format!(
                "Logging into {} environments: {}",
                targets.len(),
                targets
                    .iter()
                    .map(|(e, _)| e.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .text()
        );
    }

    // Loop. Errors during one env don't abort the rest — collect them
    // and surface at the end so a multi-env run isn't all-or-nothing
    // (e.g. dev OAuth might be configured differently than prod).
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    for (env, resolved) in &targets {
        if targets.len() > 1 {
            println!(
                "{}",
                format!("──── {env} ({}) ────", resolved.target).secondary()
            );
        }
        match login_one(&resolved.target).await {
            Ok(token) => tokens.push(token),
            Err(e) => failures.push((env.clone(), e.to_string())),
        }
    }

    if !failures.is_empty() {
        let summary = failures
            .iter()
            .map(|(e, msg)| format!("  {e}: {msg}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(OxyError::RuntimeError(format!(
            "{}/{} logins failed:\n{summary}",
            failures.len(),
            targets.len()
        )));
    }

    // `--assume` is the one-shot "log in and act as". Guarded to a single env
    // above, so exactly one target/token is in hand here.
    if let Some(org_arg) = args.assume.as_deref()
        && let (Some((_, resolved)), Some(token)) = (targets.first(), tokens.first())
    {
        let reason = args.reason.as_deref().unwrap_or_default();
        // A bare `--assume` inherits the org the `--env` URL named (parity with
        // `oxy assume start`); an explicit value wins. `None` here (bare form,
        // but the URL named no org) lets `start_session` give the same
        // "which org?" error as the standalone command.
        let org = if org_arg.trim().is_empty() {
            resolved.org_slug.as_deref()
        } else {
            Some(org_arg)
        };
        let conn = super::assume::Connection::from_parts(&resolved.target, token)?;
        return super::assume::start_session(&conn, org, reason).await;
    }
    Ok(())
}

/// Run the loopback login flow against a single target and persist the
/// token + reported email/admin status. Extracted from the original
/// handler so it can be called in a loop without duplicating the
/// 60-odd lines of browser/auth dance. Returns the captured token so
/// `--assume` can act immediately without re-reading the store.
async fn login_one(target: &str) -> Result<String, OxyError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| OxyError::RuntimeError(format!("could not bind loopback port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OxyError::RuntimeError(format!("loopback addr: {e}")))?
        .port();
    let state = uuid::Uuid::new_v4().to_string();

    let auth_url = format!("{target}/cli-auth?port={port}&state={state}");
    println!(
        "{}",
        format!("Opening {auth_url} in your browser to log in…").text()
    );
    println!(
        "{}",
        "If it doesn't open automatically, paste that URL into your browser.".tertiary()
    );
    open_browser(&auth_url);

    let token = wait_for_callback(listener, &state).await?;
    let user = fetch_user(target, &token).await?;

    let mut store = read_store();
    store.0.insert(
        host_key(target),
        HostCredential {
            token: token.clone(),
            email: user.email.clone(),
            is_app_admin: user.is_app_admin,
        },
    );
    write_store(&store)?;

    println!(
        "{}",
        format!("Logged in as {} ({target}).", user.email).success()
    );
    if user.is_app_admin {
        println!("{}", "Global admin: yes — you can `oxy publish`.".success());
    } else {
        println!(
            "{}",
            "Global admin: no — you can't publish yet. Ask #platform to add you to OXY_GLOBAL_ADMINS."
                .error()
        );
    }
    Ok(token)
}

pub async fn handle_logout_command(args: LogoutArgs) -> Result<(), OxyError> {
    let manifest = load_for_target_resolution(
        &std::env::current_dir().unwrap_or_default(),
        args.target.as_deref(),
    )?;
    let envs = envs_with_default(&args.env);

    if envs.len() > 1 && args.target.is_some() {
        return Err(OxyError::ConfigurationError(
            "--target is only valid when logging out of a single env".into(),
        ));
    }

    // Resolve every target up-front so a bad --env at position 3
    // doesn't silently drop the in-memory removals we already did for
    // positions 1 and 2 before erroring out. Mirrors the up-front
    // resolution `handle_login_command` does for the same reason.
    let targets: Vec<(String, String)> = envs
        .iter()
        .map(|env| {
            resolve_target(manifest.as_ref(), Some(env), args.target.as_deref())
                .map(|t| (env.clone(), t))
                .ok_or_else(|| {
                    OxyError::ConfigurationError(format!(
                        "could not resolve a target for --env {env}"
                    ))
                })
        })
        .collect::<Result<_, _>>()?;

    let mut store = read_store();
    let mut any_removed = false;
    for (_env, target) in &targets {
        let removed = store.0.remove(&host_key(target)).is_some();
        any_removed |= removed;
        if removed {
            println!("{}", format!("Logged out of {target}.").success());
        } else {
            println!("{}", format!("No cached credentials for {target}.").text());
        }
    }
    if any_removed {
        write_store(&store)?;
    }
    Ok(())
}

/// Best-effort open of the system browser; failures are non-fatal (we
/// already printed the URL).
fn open_browser(url: &str) {
    let (bin, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

const SUCCESS_HTML: &str = "<!doctype html><meta charset=utf-8><title>oxy login</title>\
     <body style=\"font-family:system-ui;padding:3rem;text-align:center\">\
     <h2>Logged in to oxy ✓</h2><p>You can close this tab and return to your terminal.</p>";

/// Accept loopback connections until we see a valid `/callback?token&state`
/// (matching our nonce), then return the token. Ignores stray requests
/// (favicon, etc.). 5-minute budget.
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<String, OxyError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| {
                OxyError::RuntimeError(
                    "timed out waiting for the browser to complete login (5 min)".into(),
                )
            })?
            .map_err(|e| OxyError::RuntimeError(format!("loopback accept: {e}")))?;

        let request_target = match read_request_target(&mut stream).await {
            Some(t) => t,
            None => {
                respond(&mut stream, 400, "Bad request").await;
                continue;
            }
        };

        let parsed = url::Url::parse(&format!("http://localhost{request_target}")).ok();
        let Some(parsed) = parsed else {
            respond(&mut stream, 404, "Not found").await;
            continue;
        };
        if parsed.path() != "/callback" {
            respond(&mut stream, 404, "Not found").await;
            continue;
        }

        let mut token = None;
        let mut state = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "token" => token = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if state.as_deref() != Some(expected_state) {
            respond(
                &mut stream,
                400,
                "State mismatch — please retry `oxy login`.",
            )
            .await;
            continue;
        }
        match token.filter(|t| !t.is_empty()) {
            Some(tok) => {
                respond_html(&mut stream, SUCCESS_HTML).await;
                return Ok(tok);
            }
            None => {
                respond(&mut stream, 400, "No token in callback.").await;
            }
        }
    }
}

/// Read just the request line and return its target (path+query), e.g.
/// `/callback?token=…&state=…`.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    // "GET /callback?... HTTP/1.1"
    line.split_whitespace().nth(1).map(str::to_string)
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn respond_html(stream: &mut TcpStream, html: &str) {
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

#[derive(Debug, Deserialize)]
struct UserResp {
    email: String,
    is_app_admin: bool,
}

/// `GET /api/user` with the captured bearer. The public endpoint returns
/// `Option<UserInfo>`; a `null` means the token didn't authenticate.
async fn fetch_user(target: &str, token: &str) -> Result<UserResp, OxyError> {
    let url = format!("{}/api/user", target.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| OxyError::RuntimeError(format!("http client init: {e}")))?;
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("GET {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(OxyError::RuntimeError(format!(
            "login token rejected by {url} ({})",
            resp.status()
        )));
    }
    let body: Option<UserResp> = resp
        .json()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("parse /api/user: {e}")))?;
    body.ok_or_else(|| {
        OxyError::RuntimeError("login token did not resolve to a user (got null)".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_separates_envs() {
        assert_eq!(host_key("http://localhost:3000"), "localhost:3000");
        assert_eq!(host_key("https://app.oxygen-hq.com"), "app.oxygen-hq.com");
        assert_eq!(
            host_key("https://app-dev.oxygen-hq.com/"),
            "app-dev.oxygen-hq.com"
        );
    }
}
