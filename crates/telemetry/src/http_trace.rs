//! One `SERVER` span per HTTP request, shaped the way HyperDX's request views
//! expect: named `"{method} {route}"`, attributes from the OpenTelemetry HTTP
//! semantic conventions, an inbound `traceparent` honoured as the parent,
//! and the server-minted `x-oxy-request-id` on the span so a support ticket's
//! id is one click from the trace.
//!
//! This replaces `tower_http::trace::DefaultMakeSpan`, whose span was named
//! `request` with `method` / `uri` fields — fine for a log line, invisible to
//! a tracing backend that groups by route.
//!
//! Two attributes are redacted before they leave the process, because this
//! store has a long retention and a wider reader set than the token vault:
//! `url.query` keeps its keys and replaces every value (`code=REDACTED`), and
//! `url.path` is replaced by the route pattern whenever that pattern has a
//! secret-shaped parameter (`/invitations/{token}/accept`). `http.route` and
//! the redacted forms carry all the debugging value the raw strings did.
//!
//! The route is `axum::extract::MatchedPath`, which is only present when the
//! layer is attached with `Router::layer` (axum matches first, then runs
//! per-route middleware). On the static fallback there is no route, so the
//! span is named by method alone — the semconv rule for unrouted requests —
//! and a low-cardinality dashboard is not polluted with one series per asset.

use std::time::Duration;

use axum::extract::MatchedPath;
use http::{HeaderMap, Request, Response};
use tower_http::classify::{ServerErrorsAsFailures, ServerErrorsFailureClass, SharedClassifier};
use tower_http::trace::{
    DefaultOnBodyChunk, DefaultOnEos, MakeSpan, OnFailure, OnRequest, OnResponse, TraceLayer,
};
use tracing::Span;
use tracing::field::Empty;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// The concrete layer type, so a router can name it in a signature.
pub type OxyTraceLayer = TraceLayer<
    SharedClassifier<ServerErrorsAsFailures>,
    OxyMakeSpan,
    OxyOnRequest,
    OxyOnResponse,
    DefaultOnBodyChunk,
    DefaultOnEos,
    OxyOnFailure,
>;

/// Build the layer. `request_id_header` is the name of the header the
/// request-id middleware stamps on the request (Oxy's `x-oxy-request-id`);
/// it is passed in rather than imported so this crate stays free of
/// `oxy-shared`.
pub fn trace_layer(request_id_header: &'static str) -> OxyTraceLayer {
    TraceLayer::new_for_http()
        .make_span_with(OxyMakeSpan { request_id_header })
        .on_request(OxyOnRequest)
        .on_response(OxyOnResponse)
        .on_failure(OxyOnFailure)
}

/// `"GET /api/threads/{id}"`, or just `"GET"` when nothing was routed.
pub fn span_name(method: &str, route: Option<&str>) -> String {
    match route {
        Some(route) => format!("{method} {route}"),
        None => method.to_string(),
    }
}

/// `a=1&code=xyz&flag` → `a=REDACTED&code=REDACTED&flag`. Keys tell you what
/// the caller was doing; values are where the OAuth codes and magic-link
/// tokens live, and the semconv asks for exactly this treatment.
pub fn redacted_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) => format!("{key}=REDACTED"),
            None => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Route parameters whose *value* must never be recorded.
const SECRET_PARAMS: &[&str] = &[
    "token",
    "code",
    "secret",
    "key",
    "password",
    "signature",
    "sig",
    "session",
];

/// The `url.path` to record: the raw path, unless the matched route carries a
/// secret-shaped parameter, in which case the route pattern itself — the only
/// variable part of such a path is the secret.
pub fn path_for_span(route: Option<&str>, path: &str) -> String {
    let secret_route = route.is_some_and(|r| {
        r.split('/').any(|seg| {
            seg.strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .is_some_and(|name| SECRET_PARAMS.contains(&name.trim_start_matches('*')))
        })
    });
    match route {
        Some(r) if secret_route => r.to_string(),
        _ => path.to_string(),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The originating client, per `X-Forwarded-For`'s first hop. Only the
/// header is consulted: behind the load balancer the socket peer is the LB.
fn client_address(headers: &HeaderMap) -> Option<String> {
    header_str(headers, "x-forwarded-for")
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Request paths that get **no span and, when they succeed, no log line**:
/// the kubelet's readiness / liveness probes and the load balancer's health
/// check, exactly as the app routes them (the api router is nested at `/api`;
/// a bare `/ready` is a 404, which kubelet counts as a failure — a documented
/// incident — so it is deliberately *not* here and stays visible). At the
/// chart's cadence (readiness every 5 s, liveness every 30 s, the ALB on top,
/// per pod) they are the majority of all requests a fleet serves, so tracing
/// them would fill the platform trace store — and the tenant-visible product
/// store, which sees the same span — with hundreds of thousands of identical
/// rows a day before a single user request.
///
/// A probe that does **not** succeed still logs: a 4xx as a `warn` from
/// `on_response`, a 5xx as the `error` from `on_failure`. Those lines carry
/// the status and latency but, having no span, no route, path or request id
/// — they say "a probe", not which; the readiness handler logs the cause
/// itself, and only probes are span-less.
pub const PROBE_PATHS: &[&str] = &["/api/health", "/api/ready", "/api/live"];

/// Whether a request path is one of [`PROBE_PATHS`] (exact match). Callers
/// pair it with a `GET` check: only a GET to these paths is a probe.
pub fn is_probe(path: &str) -> bool {
    PROBE_PATHS.contains(&path)
}

#[derive(Clone, Copy, Debug)]
pub struct OxyMakeSpan {
    request_id_header: &'static str,
}

impl<B> MakeSpan<B> for OxyMakeSpan {
    fn make_span(&mut self, req: &Request<B>) -> Span {
        // GET only: a `POST /api/health` is a 405 from a client, not a probe,
        // and keeps its span (and the throttled paths that go with it) so an
        // unauthenticated loop cannot write one unthrottled `warn` per hit.
        if req.method() == http::Method::GET && is_probe(req.uri().path()) {
            return Span::none();
        }
        let method = req.method().as_str();
        let route = req
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str);
        let name = span_name(method, route);
        let headers = req.headers();
        let span = tracing::info_span!(
            "http.server.request",
            otel.name = name.as_str(),
            otel.kind = "server",
            otel.status_code = Empty,
            http.request.method = method,
            http.route = route,
            http.response.status_code = Empty,
            error.type = Empty,
            error.message = Empty,
            url.path = path_for_span(route, req.uri().path()).as_str(),
            url.query = req.uri().query().map(redacted_query).as_deref(),
            server.address =
                header_str(headers, "x-forwarded-host").or_else(|| header_str(headers, "host")),
            client.address = client_address(headers).as_deref(),
            user_agent.original = header_str(headers, "user-agent"),
            oxy.request_id = header_str(headers, self.request_id_header),
        );
        if let Some(parent) = crate::propagation::extract(headers) {
            // Err means no export layer is installed (or the span somehow
            // started already); in both cases there is nothing to link to.
            let _ = span.set_parent(parent);
        }
        span
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OxyOnRequest;

impl<B> OnRequest<B> for OxyOnRequest {
    fn on_request(&mut self, _req: &Request<B>, span: &Span) {
        if span.is_none() {
            return; // a probe: no span, no line
        }
        tracing::debug!("request received");
    }
}

#[derive(Clone, Copy, Debug)]
pub struct OxyOnResponse;

impl<B> OnResponse<B> for OxyOnResponse {
    fn on_response(self, response: &Response<B>, latency: Duration, span: &Span) {
        let status = response.status();
        let latency_ms = latency.as_millis() as u64;
        if span.is_none() {
            // A GET to a real probe path. Healthy: no line. A 4xx — a `421`
            // from role routing, or an auth layer someone later puts in
            // front — is exactly the case worth seeing; a 5xx is
            // `on_failure`'s. (A misrouted bare `/ready` is not span-less:
            // it is a 404 with a span and a line like any request.)
            if status.is_client_error() {
                tracing::warn!(
                    status = status.as_u16(),
                    latency_ms,
                    "probe request did not succeed"
                );
            }
            return;
        }
        span.record("http.response.status_code", status.as_u16());
        if status.is_server_error() {
            // Semconv for SERVER spans: only 5xx is an error; a 4xx is the
            // client's problem and leaves the status unset. `on_failure`
            // writes the error line for a 5xx, so nothing is logged here.
            span.record("otel.status_code", "ERROR");
        } else if status.is_client_error() {
            tracing::info!(status = status.as_u16(), latency_ms, "request");
        } else {
            // Successes are `debug`: the span is the per-request record, and an
            // `info` line per request (health probes included) is an access
            // log nobody asked for at OXY_LOG_LEVEL=info.
            tracing::debug!(status = status.as_u16(), latency_ms, "request");
        }
    }
}

/// Error bodies larger than this are not read: the cause of a failed request
/// is a sentence, and anything bigger is a page, not a message.
pub const ERROR_BODY_MAX_BYTES: usize = 16 * 1024;

/// What is recorded as `error.message`, at most.
const ERROR_MESSAGE_MAX_BYTES: usize = 2 * 1024;

/// Put a failed response's own explanation on the request span.
///
/// Every handler turns its error into a body its own way — `{"message": …}`,
/// `{"error": …}`, plain text — and the layer that logs `request failed` only
/// sees the status, so the one line an operator finds for a 5xx said `502` and
/// nothing else (55 an hour in prod on 2026-09-10, cause unrecoverable). This
/// middleware reads a **small, fully-buffered** 4xx/5xx body, records its
/// message as `error.message` on the request span — which the `request failed`
/// line then carries as `span.error.message`, and HyperDX as an attribute — and
/// hands the bytes back unchanged. A streaming body (SSE, a proxied download)
/// has no exact size and is never touched.
///
/// Mount it **inside** [`trace_layer`] (`.layer(from_fn(record_error_body))`
/// before `.layer(trace_layer(..))`), so the request span is current, and
/// **beneath** any `CompressionLayer`: a compressed body reports no exact size,
/// so above one this middleware sees nothing but sub-32-byte identity bodies.
pub async fn record_error_body(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::body::HttpBody as _;
    let response = next.run(req).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let small = response
        .body()
        .size_hint()
        .exact()
        .is_some_and(|n| n > 0 && n as usize <= ERROR_BODY_MAX_BYTES);
    if !small {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, ERROR_BODY_MAX_BYTES).await {
        Ok(b) => b,
        // A well-behaved exact-size body under the cap cannot land here, but
        // one that errors mid-read can. The body is gone either way; drop any
        // `Content-Length` so hyper does not abort the connection on a length
        // mismatch, and let the status stand on its own.
        Err(_) => {
            parts.headers.remove(http::header::CONTENT_LENGTH);
            return axum::response::Response::from_parts(parts, axum::body::Body::empty());
        }
    };
    let content_type = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if let Some(message) = error_message_from_body(content_type, &bytes) {
        Span::current().record("error.message", message.as_str());
    }
    axum::response::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// The human part of an error body: the first of `message`, `error`,
/// `detail`, `error_description` that is a string (one level of `error: {…}`
/// nesting included), else — only for a JSON, `text/plain` or untyped body —
/// the text itself. An HTML error page, an upstream's XML, or anything else is
/// not a message and records nothing. Truncated at a char boundary.
pub fn error_message_from_body(content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let media = content_type
        .and_then(|ct| ct.split(';').next())
        .map(|m| m.trim().to_ascii_lowercase());
    let textual = match media.as_deref() {
        None | Some("text/plain") => true,
        Some(m) => m == "application/json" || m.ends_with("+json"),
    };
    const KEYS: &[&str] = &["message", "error", "detail", "error_description"];
    let from_json = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            let pick = |v: &serde_json::Value| {
                KEYS.iter()
                    .find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(str::to_string))
            };
            pick(&v).or_else(|| v.get("error").and_then(pick))
        });
    let message = match from_json {
        Some(m) => m,
        None if textual => text.to_string(),
        None => return None,
    };
    if message.len() <= ERROR_MESSAGE_MAX_BYTES {
        return Some(message);
    }
    let cut = message
        .char_indices()
        .take_while(|(i, _)| *i < ERROR_MESSAGE_MAX_BYTES)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    Some(format!("{}…", &message[..cut]))
}

#[derive(Clone, Copy, Debug)]
pub struct OxyOnFailure;

impl OnFailure<ServerErrorsFailureClass> for OxyOnFailure {
    fn on_failure(&mut self, class: ServerErrorsFailureClass, latency: Duration, span: &Span) {
        span.record("otel.status_code", "ERROR");
        let latency_ms = latency.as_millis() as u64;
        // Only probes are span-less; say so, since the line has no route.
        let what = if span.is_none() {
            "probe request failed"
        } else {
            "request failed"
        };
        match class {
            ServerErrorsFailureClass::StatusCode(code) => {
                span.record("error.type", code.as_str());
                tracing::error!(status = code.as_u16(), latency_ms, "{what}");
            }
            ServerErrorsFailureClass::Error(err) => {
                span.record("error.type", "transport");
                tracing::error!(error = %err, latency_ms, "{what}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use opentelemetry::trace::{SpanId, SpanKind, Status, TraceId, TracerProvider as _};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use tower::ServiceExt;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn span_names_follow_the_semconv_rule() {
        assert_eq!(
            span_name("GET", Some("/api/threads/{id}")),
            "GET /api/threads/{id}"
        );
        assert_eq!(span_name("POST", None), "POST");
    }

    #[test]
    fn query_values_are_redacted_and_keys_kept() {
        assert_eq!(
            redacted_query("code=4/0Ab&state=xyz"),
            "code=REDACTED&state=REDACTED"
        );
        assert_eq!(redacted_query("refresh"), "refresh");
        assert_eq!(redacted_query("a=1&flag&b="), "a=REDACTED&flag&b=REDACTED");
    }

    #[test]
    fn a_secret_route_parameter_hides_the_whole_path() {
        assert_eq!(
            path_for_span(
                Some("/invitations/{token}/accept"),
                "/invitations/abc123/accept"
            ),
            "/invitations/{token}/accept"
        );
        assert_eq!(path_for_span(Some("/items/{id}"), "/items/42"), "/items/42");
        assert_eq!(path_for_span(None, "/assets/app.js"), "/assets/app.js");
    }

    #[test]
    fn client_address_is_the_first_forwarded_hop() {
        let mut h = HeaderMap::new();
        assert_eq!(client_address(&h), None);
        h.insert("x-forwarded-for", " 203.0.113.9, 10.0.0.2".parse().unwrap());
        assert_eq!(client_address(&h).as_deref(), Some("203.0.113.9"));
    }

    fn app() -> Router {
        Router::new()
            .route("/items/{id}", get(|| async { "ok" }))
            .route("/boom", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .route(
                "/upstream",
                get(|| async {
                    (
                        StatusCode::BAD_GATEWAY,
                        axum::Json(serde_json::json!({
                            "code": "upstream_error",
                            "message": "airhouse refused the insert: table is read-only"
                        })),
                    )
                }),
            )
            .route("/api/ready", get(|| async { "ready" }))
            .layer(axum::middleware::from_fn(record_error_body))
            .layer(trace_layer("x-oxy-request-id"))
    }

    #[test]
    fn the_message_is_found_in_the_shapes_handlers_actually_return() {
        let m = |b: &str| error_message_from_body(Some("application/json"), b.as_bytes());
        assert_eq!(
            m(r#"{"code":"x","message":"boom"}"#).as_deref(),
            Some("boom")
        );
        assert_eq!(m(r#"{"error":"nope"}"#).as_deref(), Some("nope"));
        assert_eq!(
            m(r#"{"error":{"message":"nested"}}"#).as_deref(),
            Some("nested")
        );
        assert_eq!(
            error_message_from_body(Some("text/plain; charset=utf-8"), b"plain text failure")
                .as_deref(),
            Some("plain text failure")
        );
        assert_eq!(
            error_message_from_body(None, b"untyped failure").as_deref(),
            Some("untyped failure")
        );
        assert_eq!(
            error_message_from_body(
                Some("text/html; charset=utf-8"),
                b"<html><body>404 Not Found</body></html>"
            ),
            None,
            "an HTML error page is not a message"
        );
        assert_eq!(
            error_message_from_body(Some("application/problem+json"), br#"{"detail":"quota"}"#)
                .as_deref(),
            Some("quota")
        );
        assert_eq!(m("   ").as_deref(), None);
        let long = "é".repeat(ERROR_MESSAGE_MAX_BYTES);
        assert!(m(&long).unwrap().ends_with('…'));
    }

    #[tokio::test]
    async fn a_failed_response_puts_its_own_message_on_the_span_and_keeps_the_body() {
        let (status, spans) = spans_for(
            Request::builder()
                .uri("/upstream")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(spans.len(), 1);
        assert_eq!(
            attr(&spans[0], "error.message").as_deref(),
            Some("airhouse refused the insert: table is read-only")
        );

        let res = app()
            .oneshot(
                Request::builder()
                    .uri("/upstream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v["code"], "upstream_error",
            "the client still gets the body"
        );
    }

    /// `/customer-apps/{*path}` carries a per-route `CompressionLayer`. Above
    /// it the body is br/gzip with no exact size and the middleware sees
    /// nothing; beneath it, it sees the identity body. This pins why `serve.rs`
    /// mounts it inside that route's stack.
    #[tokio::test]
    async fn the_middleware_must_sit_beneath_compression_to_see_the_body() {
        use tower::ServiceBuilder;
        use tower_http::compression::CompressionLayer;

        async fn upstream_502() -> impl axum::response::IntoResponse {
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "code": "upstream_error",
                    "message": "the custom app's upstream refused the connection after 3 retries"
                })),
            )
        }
        let request = || {
            Request::builder()
                .uri("/app")
                .header("accept-encoding", "br")
                .body(Body::empty())
                .unwrap()
        };

        let beneath = Router::new()
            .route(
                "/app",
                get(upstream_502).layer(
                    ServiceBuilder::new()
                        .layer(CompressionLayer::new())
                        .layer(axum::middleware::from_fn(record_error_body)),
                ),
            )
            .layer(trace_layer("x-oxy-request-id"));
        let (_, spans) = spans_for_app(beneath, request()).await;
        assert_eq!(
            attr(&spans[0], "error.message").as_deref(),
            Some("the custom app's upstream refused the connection after 3 retries")
        );

        let above = Router::new()
            .route("/app", get(upstream_502).layer(CompressionLayer::new()))
            .layer(axum::middleware::from_fn(record_error_body))
            .layer(trace_layer("x-oxy-request-id"));
        let (_, spans) = spans_for_app(above, request()).await;
        assert_eq!(
            attr(&spans[0], "error.message"),
            None,
            "above compression the body has no exact size and is skipped"
        );
    }

    #[tokio::test]
    async fn a_successful_response_records_no_error_message() {
        let (_, spans) = spans_for(
            Request::builder()
                .uri("/items/7")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(attr(&spans[0], "error.message"), None);
    }

    #[test]
    fn probe_paths_are_the_served_ones_only() {
        assert!(is_probe("/api/health"));
        assert!(is_probe("/api/ready"));
        assert!(is_probe("/api/live"));
        // A bare `/ready` is a 404 in the served shape — a misrouted probe
        // must stay visible, so it is not a probe path.
        assert!(!is_probe("/ready"));
        assert!(!is_probe("/api/healthz"));
        assert!(!is_probe("/api/ready/"));
        assert!(!is_probe("/api/threads"));
    }

    #[tokio::test]
    async fn a_non_get_to_a_probe_path_is_not_a_probe_and_keeps_its_span() {
        let (status, spans) = spans_for(
            Request::builder()
                .method("POST")
                .uri("/api/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            spans.len(),
            1,
            "a 405 from a client is traced like any request"
        );
        assert_eq!(
            attr(&spans[0], "http.response.status_code").as_deref(),
            Some("405")
        );
    }

    #[tokio::test]
    async fn a_probe_request_exports_no_span_and_a_real_request_still_does() {
        let (status, spans) = spans_for(
            Request::builder()
                .uri("/api/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            spans.is_empty(),
            "a probe must not reach any store: {spans:?}"
        );

        let (status, spans) = spans_for(
            Request::builder()
                .uri("/items/7")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "GET /items/{id}");
    }

    /// Run one request under an in-memory exporter and return the finished
    /// spans. The span closes when the response body is dropped, so the body
    /// is drained before reading.
    async fn spans_for(req: Request<Body>) -> (StatusCode, Vec<SpanData>) {
        spans_for_app(app(), req).await
    }

    async fn spans_for_app(app: Router, req: Request<Body>) -> (StatusCode, Vec<SpanData>) {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        let _guard = tracing::subscriber::set_default(subscriber);

        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let _ = axum::body::to_bytes(res.into_body(), usize::MAX).await;
        provider.force_flush().unwrap();
        (status, exporter.get_finished_spans().unwrap())
    }

    fn attr(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.to_string())
    }

    #[tokio::test]
    async fn names_the_span_by_route_and_adopts_the_inbound_traceparent() {
        let req = Request::builder()
            .uri("/items/42?x=1")
            .header(
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            )
            .header("x-oxy-request-id", "11111111-2222-3333-4444-555555555555")
            .header("user-agent", "test-agent")
            .header("x-forwarded-for", "203.0.113.9")
            .header("host", "app.example.test")
            .body(Body::empty())
            .unwrap();
        let (status, spans) = spans_for(req).await;
        assert_eq!(status, StatusCode::OK);

        let span = spans
            .iter()
            .find(|s| s.name == "GET /items/{id}")
            .unwrap_or_else(|| {
                panic!(
                    "no routed span in {:?}",
                    spans.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
                )
            });
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(
            span.span_context.trace_id(),
            TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").unwrap()
        );
        assert_eq!(
            span.parent_span_id,
            SpanId::from_hex("b7ad6b7169203331").unwrap()
        );
        assert_eq!(attr(span, "http.request.method").as_deref(), Some("GET"));
        assert_eq!(attr(span, "http.route").as_deref(), Some("/items/{id}"));
        assert_eq!(attr(span, "url.path").as_deref(), Some("/items/42"));
        assert_eq!(attr(span, "url.query").as_deref(), Some("x=REDACTED"));
        assert_eq!(
            attr(span, "http.response.status_code").as_deref(),
            Some("200")
        );
        assert_eq!(
            attr(span, "server.address").as_deref(),
            Some("app.example.test")
        );
        assert_eq!(attr(span, "client.address").as_deref(), Some("203.0.113.9"));
        assert_eq!(
            attr(span, "user_agent.original").as_deref(),
            Some("test-agent")
        );
        assert_eq!(
            attr(span, "oxy.request_id").as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(span.status, Status::Unset, "a 200 leaves the status unset");
    }

    #[tokio::test]
    async fn a_5xx_is_an_error_span_and_a_bare_request_is_a_root() {
        let req = Request::builder().uri("/boom").body(Body::empty()).unwrap();
        let (status, spans) = spans_for(req).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

        let span = spans
            .iter()
            .find(|s| s.name == "GET /boom")
            .expect("routed span");
        assert!(
            matches!(span.status, Status::Error { .. }),
            "{:?}",
            span.status
        );
        assert_eq!(
            attr(span, "http.response.status_code").as_deref(),
            Some("500")
        );
        assert_eq!(attr(span, "error.type").as_deref(), Some("500"));
        assert_eq!(
            span.parent_span_id,
            SpanId::INVALID,
            "no traceparent → root span"
        );
        assert!(span.span_context.is_valid());
    }
}
