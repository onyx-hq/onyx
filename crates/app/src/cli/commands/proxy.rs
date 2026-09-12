//! `oxy proxy` — local custom-app dev against a cloud Oxy (design doc
//! `internal-docs/partner-platform.md`).
//!
//! An **outbound** sidecar: it does not serve the app (your dev server does).
//! It listens on `--port` (default **3000**, where a local `oxy serve` would be,
//! so a dev server's default Oxy proxy target already matches), and forwards the
//! Oxy calls your dev server proxies to it — attaching the `oxy login --env`
//! bearer and applying guardrails — to the resolved cloud target.
//!
//! The token lives only in this process and is added server-side as an auth
//! FALLBACK (never returned to the browser): the browser's own cookie/session is
//! forwarded transparently, and auth/login endpoints reach the backend
//! unauthenticated so sign-in works. Authorization is decided by the cloud
//! (including partner delegation — see design doc §4); the proxy only forwards.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use oxy_shared::errors::OxyError;
use reqwest::Client;

use super::app_manifest::{load_for_target_resolution, resolve_target};
use super::login;

/// Cap on a buffered request body. Requests through the proxy are small (query /
/// semantic-query JSON); large payloads are *responses*, which stream.
const MAX_REQUEST_BODY: usize = 25 * 1024 * 1024;

#[derive(clap::Args, Debug)]
pub struct ProxyArgs {
    /// Env to resolve the cloud target + cached token from (same as
    /// `oxy login --env`). First value wins; defaults to `production`.
    #[arg(long, action = clap::ArgAction::Append, value_delimiter = ',')]
    env: Vec<String>,
    /// Explicit oxy base URL; overrides `--env`.
    #[arg(long)]
    target: Option<String>,
    /// Local port to listen on. Default 3000 — stands in for a local `oxy serve`
    /// so a dev server's default Oxy proxy target already points here.
    #[arg(long, default_value_t = 3000)]
    port: u16,
    /// Forward side-effecting calls (`/fn`, agent runs, procedure runs) instead
    /// of holding them.
    #[arg(long)]
    allow_writes: bool,
    /// Forward tracking events instead of dropping them.
    #[arg(long)]
    allow_events: bool,
    /// Confirm proxying to a production target.
    #[arg(long)]
    yes: bool,
}

struct ProxyState {
    target: String,
    /// Fallback bearer for the custom-app case. `None` when the dev isn't logged
    /// in — then the proxy relies purely on the browser's own session (sign-in).
    token: Option<String>,
    allow_writes: bool,
    allow_events: bool,
    client: Client,
}

/// First non-empty `--env`, defaulting to `production` (which the prod guard
/// then gates behind `--yes`).
fn first_env(envs: &[String]) -> String {
    envs.iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "production".to_string())
}

/// `POST /api/customer-apps/{id}/events` — usage tracking. Dropped by default so
/// local dev never writes to the target's analytics.
fn is_events_path(path: &str) -> bool {
    path.starts_with("/api/customer-apps/") && path.ends_with("/events")
}

/// Auth / session endpoints (`/api/auth/*`, `/api/user`). These must reach the
/// backend UNAUTHENTICATED to establish a session (sign-in), so the proxy never
/// injects the dev bearer on them and never holds them behind `--allow-writes`.
fn is_auth_path(path: &str) -> bool {
    path.starts_with("/api/auth/") || path == "/api/user"
}

/// Whether a request should be HELD (the "side-effecting calls held by default"
/// guarantee). Allowlist, not denylist: any mutating method is held EXCEPT the
/// two POST-but-read data-plane endpoints. GET/HEAD are never mutating; tracking
/// events and auth/login endpoints have their own handling and are excluded here.
fn is_write_path(method: &Method, path: &str) -> bool {
    let mutating = matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if !mutating || is_events_path(path) || is_auth_path(path) {
        return false;
    }
    // POST-but-read: `query` / `semantic-query` carry their filter in the body.
    let is_read_post =
        *method == Method::POST && (path.ends_with("/query") || path.ends_with("/semantic-query"));
    !is_read_post
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

/// Build outbound headers. Forward the browser's own auth (Cookie / Authorization)
/// transparently so a signed-in session works, plus content-type + accept (JSON
/// bodies + SSE). Inject the dev's bearer only as a FALLBACK — when the request
/// carries no auth of its own AND isn't a login/auth endpoint (which must reach
/// the backend unauthenticated). So the dev token never overrides a real browser
/// session and never breaks sign-in.
fn build_request_headers(incoming: &HeaderMap, token: Option<&str>, path: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    for name in [
        header::CONTENT_TYPE,
        header::ACCEPT,
        header::COOKIE,
        header::AUTHORIZATION,
        // Forward the browser's origin so the backend derives the right base URL
        // (it reads Origin/Referer) — critical for the OAuth redirect_uri to match
        // what the provider issued the code for. Without these, sign-in 401s.
        header::ORIGIN,
        header::REFERER,
    ] {
        if let Some(v) = incoming.get(&name) {
            h.insert(name, v.clone());
        }
    }
    // W3C trace context from the SDK. The request's trace must be the one the
    // page stamped on its error (`error.traceId`), or that id names a trace
    // that exists nowhere. Not in the list above only because `http` has no
    // constant for it.
    for name in ["traceparent", "tracestate"] {
        if let Some(v) = incoming.get(name) {
            h.insert(HeaderName::from_static(name), v.clone());
        }
    }
    let has_auth = h.contains_key(header::COOKIE) || h.contains_key(header::AUTHORIZATION);
    if !has_auth
        && !is_auth_path(path)
        && let Some(token) = token
        && let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        h.insert(header::AUTHORIZATION, v);
    }
    h
}

/// Make a cloud backend's Set-Cookie storable by the browser on `localhost`
/// (standard dev-proxy rewrite): strip `Domain=` and `Secure`, and normalize
/// `SameSite=None` → `Lax` (None without Secure is rejected; the app→proxy call
/// is same-origin on localhost, so Lax is correct).
fn rewrite_set_cookie(value: &str) -> String {
    value
        .split(';')
        .map(str::trim)
        .filter(|part| {
            let low = part.to_ascii_lowercase();
            !low.starts_with("domain=") && low != "secure"
        })
        .map(|part| {
            if part.eq_ignore_ascii_case("samesite=none") {
                "SameSite=Lax".to_string()
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The function name when `path` is an Oxy Function invocation —
/// `/customer-apps/<org>/<slug>/fn/<name>[/…]`, the one call this proxy
/// annotates with its ids. A bundle asset that merely contains `/fn/` is not.
fn function_route(path: &str) -> Option<&str> {
    let mut seg = path.split('/').filter(|s| !s.is_empty());
    match (seg.next(), seg.next(), seg.next(), seg.next(), seg.next()) {
        (Some("customer-apps"), Some(_org), Some(_slug), Some("fn"), Some(name)) => Some(name),
        _ => None,
    }
}

/// `fn <name>` for the per-call line.
fn function_display_name(path: &str) -> String {
    format!("fn {}", function_route(path).unwrap_or("?"))
}

/// The trace id of a well-formed W3C `traceparent`: version, 32-hex non-zero
/// trace id, 16-hex non-zero span id, 2-hex flags. Anything else is rejected
/// whole — the server's extractor would, and a printed id must be real.
fn valid_traceparent_trace_id(value: &str) -> Option<&str> {
    let parts: Vec<&str> = value.split('-').collect();
    let [version, trace, span, flags] = parts.as_slice() else {
        return None;
    };
    let hex = |s: &str, len: usize| s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit());
    let nonzero = |s: &str| s.chars().any(|c| c != '0');
    (hex(version, 2)
        && hex(trace, 32)
        && nonzero(trace)
        && hex(span, 16)
        && nonzero(span)
        && hex(flags, 2))
    .then_some(*trace)
}

/// Keep a well-formed inbound `traceparent` (the SDK minted one) or mint a
/// sampled W3C header — replacing a malformed one — and return the trace id
/// either way.
fn ensure_traceparent(headers: &mut HeaderMap) -> String {
    if let Some(existing) = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .and_then(valid_traceparent_trace_id)
    {
        return existing.to_string();
    }
    // A rejected `traceparent` takes its `tracestate` with it (W3C): vendor
    // state from a trace that no longer applies must not ride on the new one.
    headers.remove("tracestate");
    let trace_id = uuid::Uuid::new_v4().simple().to_string();
    let span_id = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
    if let Ok(value) = HeaderValue::from_str(&format!("00-{trace_id}-{span_id}-01")) {
        headers.insert("traceparent", value);
    }
    trace_id
}

/// Catch-all: apply guardrails, then forward to the cloud target with the bearer,
/// streaming the response back (handles SSE + large result sets).
async fn forward(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();

    if is_events_path(&path) && !state.allow_events {
        tracing::debug!(%path, "oxy proxy: tracking event dropped (--allow-events to send)");
        return StatusCode::NO_CONTENT.into_response();
    }
    if is_write_path(&method, &path) && !state.allow_writes {
        return (
            StatusCode::CONFLICT,
            "oxy proxy: side-effecting call held. Re-run with --allow-writes to permit /fn, agent, and procedure calls against the target.",
        )
            .into_response();
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("oxy proxy: could not read request body: {e}"),
            )
                .into_response();
        }
    };

    let url = format!("{}{}", state.target, path_and_query);
    tracing::debug!(%method, %path, "oxy proxy → cloud");

    let mut out_headers = build_request_headers(&parts.headers, state.token.as_deref(), &path);
    // A function call gets a trace of its own, minted here when the SDK did
    // not already send one, so the line printed below can name the trace an
    // operator would open in HyperDX — the developer who just watched the
    // call fail should not have to find the id by hand.
    let trace_id = function_route(&path)
        .is_some()
        .then(|| ensure_traceparent(&mut out_headers));
    let upstream = state
        .client
        .request(method, &url)
        .headers(out_headers)
        .body(body_bytes.to_vec())
        .send()
        .await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oxy proxy: upstream error reaching {}: {e}", state.target),
            )
                .into_response();
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(trace_id) = trace_id {
        let request_id = upstream
            .headers()
            .get("x-oxy-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        println!(
            "  ↳ {} {}  request_id={request_id}  trace_id={trace_id}",
            status.as_u16(),
            function_display_name(&path),
        );
    }
    let mut resp_headers = HeaderMap::new();
    for (name, value) in upstream.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        let Ok(n) = HeaderName::from_bytes(name.as_str().as_bytes()) else {
            continue;
        };
        // Rewrite Set-Cookie so the cloud session cookie is accepted on localhost.
        if n == header::SET_COOKIE {
            if let Ok(s) = value.to_str()
                && let Ok(v) = HeaderValue::from_str(&rewrite_set_cookie(s))
            {
                resp_headers.append(n, v);
            }
            continue;
        }
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            resp_headers.append(n, v);
        }
    }

    let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    response
}

pub async fn handle_proxy_command(args: ProxyArgs) -> Result<(), OxyError> {
    let cwd =
        std::env::current_dir().map_err(|e| OxyError::RuntimeError(format!("current dir: {e}")))?;
    // Without `--target`, a broken oxy-app.json is an error, not "no manifest":
    // its `environments` decide the target, and silently dropping them proxies
    // somewhere else. With `--target` the file is not read.
    let manifest = load_for_target_resolution(&cwd, args.target.as_deref())?;
    let env = first_env(&args.env);

    let target = resolve_target(manifest.as_ref(), Some(&env), args.target.as_deref())
        .ok_or_else(|| {
            OxyError::RuntimeError(format!(
                "could not resolve a target for --env {env}. Pass --target <url> or add it to oxy-app.json environments."
            ))
        })?;

    // Optional: with a token, the proxy adds it as a fallback bearer for the
    // custom-app case; without one, it forwards the browser's own session so you
    // can sign in through the proxy.
    let token = std::env::var("OXY_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(|| login::load_token(&target));

    let is_prod =
        matches!(env.as_str(), "production" | "prod") || target.contains("://app.oxygen-hq.com");
    if is_prod && !args.yes {
        return Err(OxyError::RuntimeError(format!(
            "refusing to proxy to PRODUCTION ({target}) without confirmation. Re-run with --yes if that's intended."
        )));
    }

    println!();
    println!("  oxy proxy");
    println!("  → target : {target}  [{}]", env.to_uppercase());
    println!(
        "  → auth   : {}",
        if token.is_some() {
            "oxy login token (fallback); browser session forwarded"
        } else {
            "browser session only (no oxy login token — sign in through the proxy)"
        }
    );
    println!(
        "  → events : {}",
        if args.allow_events {
            "forwarded"
        } else {
            "dropped (--allow-events to send)"
        }
    );
    println!(
        "  → writes : {}",
        if args.allow_writes {
            "allowed"
        } else {
            "held (--allow-writes to permit /fn, agents, procedures)"
        }
    );
    println!();

    let state = Arc::new(ProxyState {
        target,
        token,
        allow_writes: args.allow_writes,
        allow_events: args.allow_events,
        client: Client::new(),
    });

    let app = Router::new().fallback(forward).with_state(state);
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        OxyError::RuntimeError(format!(
            "could not bind 127.0.0.1:{} — is a local oxy server already running there? Use --port. ({e})",
            args.port
        ))
    })?;

    println!(
        "  listening on http://127.0.0.1:{}  (point your dev server's Oxy proxy here)",
        args.port
    );
    println!();

    axum::serve(listener, app)
        .await
        .map_err(|e| OxyError::RuntimeError(format!("proxy server error: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_path_matches() {
        assert!(is_events_path("/api/customer-apps/abc/events"));
        assert!(!is_events_path("/api/projects/abc/query"));
    }

    #[test]
    fn holds_all_mutations_except_allowlisted_reads() {
        let post = Method::POST;
        // Known side-effecting shapes are held...
        assert!(is_write_path(&post, "/customer-apps/o/s/fn/refresh"));
        assert!(is_write_path(&post, "/api/projects/x/agents/y/asks"));
        assert!(is_write_path(&post, "/api/projects/x/procedures/y/runs"));
        // ...and so is ANY other mutating method / route (allowlist, not denylist).
        assert!(is_write_path(&Method::DELETE, "/api/projects/x/resource/y"));
        assert!(is_write_path(&Method::PATCH, "/api/projects/x/members/y"));
        assert!(is_write_path(&post, "/api/projects/x/whatever"));
        // The two POST-but-read data-plane endpoints are NOT held.
        assert!(!is_write_path(&post, "/api/projects/x/query"));
        assert!(!is_write_path(&post, "/api/projects/x/semantic-query"));
        // Reads are never held; events are governed by their own guardrail.
        assert!(!is_write_path(&Method::GET, "/api/projects/x/query"));
        assert!(!is_write_path(&post, "/api/customer-apps/x/events"));
        // Auth/login endpoints are never held (sign-in must reach the backend).
        assert!(!is_write_path(&post, "/api/auth/magic-link/verify"));
        assert!(!is_write_path(&post, "/api/auth/google"));
    }

    #[test]
    fn auth_paths_recognized() {
        assert!(is_auth_path("/api/auth/magic-link/verify"));
        assert!(is_auth_path("/api/user"));
        assert!(!is_auth_path("/api/projects/x/query"));
    }

    #[test]
    fn set_cookie_rewrite_makes_it_localhost_storable() {
        let out = rewrite_set_cookie(
            "oxy_session=abc; Domain=.oxygen-hq.com; Path=/; Secure; HttpOnly; SameSite=None",
        );
        assert!(!out.to_lowercase().contains("domain="));
        assert!(!out.to_lowercase().contains("secure"));
        assert!(!out.to_lowercase().contains("samesite=none"));
        assert!(out.contains("SameSite=Lax"));
        assert!(out.contains("oxy_session=abc"));
        assert!(out.contains("HttpOnly"));
    }

    #[test]
    fn first_env_defaults_and_picks_first() {
        assert_eq!(first_env(&[]), "production");
        assert_eq!(first_env(&["  ".into(), "dev".into()]), "dev");
        assert_eq!(first_env(&["staging".into()]), "staging");
    }
}

#[cfg(test)]
mod trace_id_tests {
    use super::*;

    #[test]
    fn only_function_calls_are_annotated() {
        assert_eq!(
            function_route("/customer-apps/acme/sales/fn/refresh"),
            Some("refresh")
        );
        assert_eq!(
            function_route("/customer-apps/o/s/fn/sync/extra"),
            Some("sync")
        );
        assert_eq!(
            function_route("/customer-apps/acme/sales/assets/app.js"),
            None
        );
        assert_eq!(
            function_route("/customer-apps/acme/sales/assets/fn/x.js"),
            None
        );
        assert_eq!(function_route("/api/fn/not-an-app"), None);
        assert_eq!(
            function_display_name("/customer-apps/acme/sales/fn/refresh"),
            "fn refresh"
        );
    }

    /// The SDK's header must survive the outbound allowlist, or the id the
    /// page stamped on its error names a trace that exists nowhere.
    #[test]
    fn the_sdks_traceparent_survives_the_outbound_allowlist() {
        let mut incoming = HeaderMap::new();
        incoming.insert(
            "traceparent",
            HeaderValue::from_static("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
        );
        incoming.insert("tracestate", HeaderValue::from_static("oxy=1"));
        let mut out = build_request_headers(&incoming, None, "/customer-apps/o/s/fn/refresh");
        assert_eq!(
            ensure_traceparent(&mut out),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(out["traceparent"], incoming["traceparent"]);
        assert_eq!(out["tracestate"], "oxy=1");
    }

    #[test]
    fn a_missing_traceparent_is_minted() {
        let mut headers = HeaderMap::new();
        let minted = ensure_traceparent(&mut headers);
        assert_eq!(minted.len(), 32);
        let sent = headers["traceparent"].to_str().unwrap().to_string();
        assert_eq!(valid_traceparent_trace_id(&sent), Some(minted.as_str()));
        assert!(sent.ends_with("-01"));
    }

    #[test]
    fn a_malformed_traceparent_is_replaced_not_forwarded() {
        for bad in [
            "garbage",
            "00-00000000000000000000000000000000-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01",
            "zz-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("traceparent", HeaderValue::from_str(bad).unwrap());
            headers.insert("tracestate", HeaderValue::from_static("vendor=stale"));
            let minted = ensure_traceparent(&mut headers);
            assert!(!bad.contains(&minted), "{bad} should have been replaced");
            assert!(valid_traceparent_trace_id(headers["traceparent"].to_str().unwrap()).is_some());
            assert!(
                headers.get("tracestate").is_none(),
                "stale tracestate must go with it"
            );
        }
    }
}
