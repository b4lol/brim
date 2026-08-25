//! HTTP request routing for the Brim REST API and embedded SPA.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::{PackageManager, SourceType, SystemStats};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Body;
use hyper::header::HeaderValue;
use hyper::{Method, Request, Response, StatusCode};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::web::static_files;

/// Maximum accepted size for a JSON request body.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// How long a `/api/stats` response is served from the cache.
const STATS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Header carrying the per-session API token.
const TOKEN_HEADER: &str = "x-brim-token";

/// Shared server state passed to every request handler.
pub struct State {
    mgr: Arc<PackageManager>,
    /// Per-session bearer token required on every `/api/*` request.
    token: Arc<str>,
    /// Short-lived cache for `/api/stats`; at most one recompute per TTL.
    stats_cache: Mutex<Option<(Instant, SystemStats)>>,
}

impl State {
    pub fn new(mgr: Arc<PackageManager>, token: Arc<str>) -> Self {
        Self {
            mgr,
            token,
            stats_cache: Mutex::new(None),
        }
    }
}

/// JSON body accepted by `POST /api/install` and `POST /api/remove`.
///
/// `source` uses the API's lowercase names (`fedora`, `copr`, `flatpak`,
/// `debian`) rather than brim-core's serde variant names; `null`/missing
/// means "any".
#[derive(serde::Deserialize)]
struct PackageRequest {
    id: String,
    source: Option<String>,
}

/// Handle a single HTTP request against the shared server state.
///
/// Generic over the body type so tests can drive it with `Full<Bytes>`
/// while the server passes hyper's `Incoming`.
pub async fn handle<B>(req: Request<B>, state: Arc<State>) -> Response<Full<Bytes>>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // DNS-rebinding guard: every request must name a loopback host.
    if !loopback_host(&req) {
        return json_error(StatusCode::FORBIDDEN, "forbidden host");
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(str::to_string);

    // Every /api/* endpoint requires the per-session token; static assets
    // (which cannot change system state) are served without it.
    if path.starts_with("/api/") && !authorized(&req, &state.token) {
        return json_error(StatusCode::FORBIDDEN, "missing or invalid token");
    }

    match (method, path.as_str()) {
        (Method::GET, "/") => static_response(
            static_files::INDEX_HTML,
            "text/html; charset=utf-8",
            true,
            "no-cache",
        ),
        (Method::GET, "/style.css") => {
            static_response(static_files::STYLE_CSS, "text/css", false, "max-age=300")
        }
        (Method::GET, "/app.js") => static_response(
            static_files::APP_JS,
            "application/javascript",
            false,
            "max-age=300",
        ),
        (Method::GET, "/api/packages") => packages(state.mgr.clone(), query.as_deref()).await,
        (Method::GET, "/api/stats") => stats(&state).await,
        (Method::POST, "/api/install")
        | (Method::POST, "/api/remove")
        | (Method::POST, "/api/upgrade")
            if foreign_origin(&req) =>
        {
            json_error(StatusCode::FORBIDDEN, "forbidden origin")
        }
        (Method::POST, "/api/install") => {
            transaction(req, state.mgr.clone(), TransactionKind::Install).await
        }
        (Method::POST, "/api/remove") => {
            transaction(req, state.mgr.clone(), TransactionKind::Remove).await
        }
        (Method::POST, "/api/upgrade") => upgrade(state.mgr.clone()).await,
        _ => json_error(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Check the per-session token on an `/api/*` request.
fn authorized<B>(req: &Request<B>, token: &str) -> bool {
    req.headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|provided| constant_time_eq(provided.as_bytes(), token.as_bytes()))
}

/// Compare two byte strings in time proportional to their length only.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// DNS-rebinding guard applied to every request.
///
/// Browsers send `Host` on every HTTP/1.1 request; a page on an arbitrary
/// website whose DNS name rebinds to 127.0.0.1 arrives with that site's
/// `Host` and is rejected. Missing or unparseable values fail closed.
fn loopback_host<B>(req: &Request<B>) -> bool {
    let Some(host) = req.headers().get(hyper::header::HOST) else {
        return false;
    };
    let Ok(host) = host.to_str() else {
        return false;
    };
    match host_name(host) {
        Some(name) => {
            name == "127.0.0.1" || name == "::1" || name.eq_ignore_ascii_case("localhost")
        }
        None => false,
    }
}

/// Extract the host (no port) from a `Host` header value: `host[:port]`,
/// with IPv6 literals in brackets. Anything else (bare IPv6, non-numeric
/// ports, empty host) returns `None` so callers fail closed.
fn host_name(authority: &str) -> Option<&str> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed.find(']')?;
        let rest = &bracketed[end + 1..];
        (&bracketed[..end], rest.strip_prefix(':').unwrap_or(rest))
    } else {
        match authority.split_once(':') {
            // A second colon means a bare IPv6 literal: fail closed.
            Some((h, p)) if !h.is_empty() && !p.contains(':') => (h, p),
            Some(_) => return None,
            None => (authority, ""),
        }
    };
    if host.is_empty() || (!port.is_empty() && !port.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    Some(host)
}

/// CSRF guard for state-changing POSTs.
///
/// A drive-by form POST from an arbitrary website carries that site's
/// `Origin`; reject anything whose host is not exactly a loopback name.
/// Requests with no `Origin` header (curl, non-browser clients) proceed.
/// Unparseable origins fail closed.
fn foreign_origin<B>(req: &Request<B>) -> bool {
    let Some(origin) = req.headers().get(hyper::header::ORIGIN) else {
        return false;
    };
    let Ok(origin) = origin.to_str() else {
        return true; // unparseable origin: fail closed
    };
    match origin_host(origin) {
        Some(host) => !matches!(host, "127.0.0.1" | "localhost" | "::1"),
        None => true,
    }
}

/// Extract the host (no port) from an `Origin` header value.
///
/// Requires a `scheme://` prefix; strips any path and userinfo, then splits
/// off the port (IPv6 literals keep their brackets during the split).
/// Returns `None` for anything that does not look like `scheme://host[:port]`.
fn origin_host(origin: &str) -> Option<&str> {
    let (_, rest) = origin.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        // IPv6 literal: host ends at the closing bracket.
        let end = bracketed.find(']')?;
        Some(&bracketed[..end])
    } else {
        Some(authority.split(':').next().unwrap_or(authority))
    }
}

/// `GET /api/packages?q=<query>[&source=fedora|copr|flatpak]` — merged search
/// results, optionally restricted to one source, or `[]` without a query.
async fn packages(mgr: Arc<PackageManager>, query: Option<&str>) -> Response<Full<Bytes>> {
    let source_raw = query.and_then(|q| query_param(q, "source")).map(url_decode);
    let source = match parse_source(source_raw.as_deref()) {
        Ok(s) => s,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, &msg),
    };
    let query = query
        .and_then(|q| query_param(q, "q"))
        .map(url_decode)
        .filter(|q| !q.is_empty());
    match query {
        None => json_response(StatusCode::OK, &Vec::<crate::core::Package>::new()),
        Some(q) => {
            let results = mgr.search(&q, source).await;
            json_response(StatusCode::OK, &results)
        }
    }
}

/// `GET /api/stats` — dashboard statistics across all backends.
///
/// Results are cached for `STATS_CACHE_TTL`; the stats walk can be slow
/// (it shells out to the backends) and the dashboard polls it frequently.
async fn stats(state: &State) -> Response<Full<Bytes>> {
    // Holding the lock across the recompute also serializes concurrent
    // misses, so a burst of polls triggers at most one backend walk.
    let mut cache = state.stats_cache.lock().await;
    if let Some((fetched, cached)) = cache.as_ref() {
        if fetched.elapsed() < STATS_CACHE_TTL {
            return json_response(StatusCode::OK, cached);
        }
    }
    let fresh = state.mgr.system_stats().await;
    *cache = Some((Instant::now(), fresh.clone()));
    json_response(StatusCode::OK, &fresh)
}

/// Which per-package transaction a `POST` is asking for.
enum TransactionKind {
    Install,
    Remove,
}

/// `POST /api/install` and `POST /api/remove` — real per-package transactions
/// routed by brim-core.
async fn transaction<B>(
    req: Request<B>,
    mgr: Arc<PackageManager>,
    kind: TransactionKind,
) -> Response<Full<Bytes>>
where
    B: Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let body = match Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            return json_error(StatusCode::PAYLOAD_TOO_LARGE, "body too large");
        }
        Err(e) => {
            // Connection/IO failure while reading, not a size violation.
            eprintln!("failed to read request body: {e}");
            return json_error(StatusCode::BAD_REQUEST, "could not read request body");
        }
    };
    let payload: PackageRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")),
    };
    let id = payload.id.trim();
    if id.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "missing package id");
    }
    let source = match parse_source(payload.source.as_deref()) {
        Ok(s) => s,
        Err(msg) => return json_error(StatusCode::BAD_REQUEST, &msg),
    };
    let outcome = match kind {
        TransactionKind::Install => mgr.install(id, source).await,
        TransactionKind::Remove => mgr.remove(id, source).await,
    };
    match outcome {
        Ok(result) => json_response(StatusCode::OK, &result),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `POST /api/upgrade` — real upgrade transaction across all backends.
async fn upgrade(mgr: Arc<PackageManager>) -> Response<Full<Bytes>> {
    match mgr.upgrade().await {
        Ok(result) => json_response(StatusCode::OK, &result),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Map the API's lowercase source names to brim-core's `SourceType`.
fn parse_source(raw: Option<&str>) -> Result<Option<SourceType>, String> {
    match raw {
        None => Ok(None),
        Some("fedora") => Ok(Some(SourceType::FedoraOfficial)),
        Some("copr") => Ok(Some(SourceType::Copr)),
        Some("flatpak") => Ok(Some(SourceType::Flatpak)),
        Some("debian") => Ok(Some(SourceType::Debian)),
        Some(other) => Err(format!("unknown source: {other}")),
    }
}

/// Extract `key=value` from a raw query string (first match wins).
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v);
            }
        }
    }
    None
}

/// Decode a URL query component (`%XX` escapes, `+` as space).
fn url_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Build a static-asset response with the given content type.
///
/// `is_html` adds the framing/scripting headers that only the SPA shell
/// needs; `cache_control` becomes the `Cache-Control` value.
fn static_response(
    body: &'static str,
    content_type: &'static str,
    is_html: bool,
    cache_control: &'static str,
) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    let headers = resp.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    headers.insert(
        hyper::header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    add_security_headers(headers, is_html);
    resp
}

/// Build a JSON response from any serializable value.
///
/// Serialization of brim-core's models cannot realistically fail; if it ever
/// does, return a 500 error body instead of panicking.
fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<Full<Bytes>> {
    let body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(_) => {
            let mut resp = Response::new(Full::new(Bytes::from_static(
                br#"{"error":"serialization failed"}"#,
            )));
            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            let headers = resp.headers_mut();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            add_security_headers(headers, false);
            return resp;
        }
    };
    let mut resp = Response::new(Full::new(Bytes::from(body)));
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    add_security_headers(headers, false);
    resp
}

/// Insert the security headers shared by every response.
///
/// All responses get `nosniff`; the HTML shell additionally forbids framing
/// and restricts content to same-origin (inline styles are used by the SPA).
fn add_security_headers(headers: &mut hyper::HeaderMap, is_html: bool) {
    headers.insert(
        hyper::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if is_html {
        headers.insert(
            hyper::header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        );
        headers.insert(
            hyper::header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self'",
            ),
        );
    }
}

/// Build a `{"error": ...}` JSON response.
fn json_error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    json_response(status, &serde_json::json!({ "error": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SystemStats;
    use http_body_util::BodyExt;
    use hyper::Method;

    const TEST_TOKEN: &str = "test-token";

    fn state() -> Arc<State> {
        // Hermetic: an empty backend set never shells out to the system.
        Arc::new(State::new(
            Arc::new(PackageManager::with_backends(vec![])),
            TEST_TOKEN.into(),
        ))
    }

    fn get(uri: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(Full::new(Bytes::new()))
            .expect("request")
    }

    async fn body_bytes(resp: Response<Full<Bytes>>) -> Bytes {
        resp.into_body().collect().await.expect("body").to_bytes()
    }

    #[tokio::test]
    async fn api_stats_returns_json() {
        let resp = handle(get("/api/stats"), state()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let stats: SystemStats = serde_json::from_slice(&body).expect("SystemStats JSON");
        assert_eq!(stats.installed, 0);
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let resp = handle(get("/nope"), state()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_bytes(resp).await;
        let err: serde_json::Value = serde_json::from_slice(&body).expect("error JSON");
        assert_eq!(err["error"], "not found");
    }

    #[tokio::test]
    async fn packages_without_query_returns_empty_array() {
        let resp = handle(get("/api/packages"), state()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let pkgs: serde_json::Value = serde_json::from_slice(&body).expect("packages JSON");
        assert_eq!(pkgs, serde_json::json!([]));
    }

    #[tokio::test]
    async fn root_serves_spa_html() {
        let resp = handle(get("/"), state()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let html = String::from_utf8(body.to_vec()).expect("utf-8");
        assert!(html.contains("<title>Brim"));
    }

    #[test]
    fn parse_source_accepts_lowercase_api_names() {
        assert_eq!(parse_source(None).unwrap(), None);
        assert_eq!(
            parse_source(Some("fedora")).unwrap(),
            Some(SourceType::FedoraOfficial)
        );
        assert_eq!(parse_source(Some("copr")).unwrap(), Some(SourceType::Copr));
        assert_eq!(
            parse_source(Some("flatpak")).unwrap(),
            Some(SourceType::Flatpak)
        );
        assert_eq!(
            parse_source(Some("debian")).unwrap(),
            Some(SourceType::Debian)
        );
        assert!(parse_source(Some("FedoraOfficial")).is_err());
        assert!(parse_source(Some("bogus")).is_err());
    }

    #[tokio::test]
    async fn install_rejects_unknown_source() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/install")
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(Full::new(Bytes::from_static(
                br#"{"id": "htop", "source": "bogus"}"#,
            )))
            .expect("request");
        let resp = handle(req, state()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    fn post(uri: &str, origin: Option<&str>) -> Request<Full<Bytes>> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .header(TOKEN_HEADER, TEST_TOKEN);
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        builder.body(Full::new(Bytes::new())).expect("request")
    }

    #[tokio::test]
    async fn upgrade_rejects_foreign_origin() {
        let resp = handle(post("/api/upgrade", Some("https://evil.example")), state()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = body_bytes(resp).await;
        let err: serde_json::Value = serde_json::from_slice(&body).expect("error JSON");
        assert_eq!(err["error"], "forbidden origin");
    }

    #[tokio::test]
    async fn install_rejects_foreign_origin() {
        let resp = handle(post("/api/install", Some("https://evil.example")), state()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn upgrade_accepts_loopback_origin() {
        for origin in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "https://127.0.0.1:8080",
            "https://localhost:8080",
        ] {
            let resp = handle(post("/api/upgrade", Some(origin)), state()).await;
            assert_ne!(resp.status(), StatusCode::FORBIDDEN, "origin: {origin}");
        }
    }

    #[tokio::test]
    async fn upgrade_accepts_missing_origin() {
        let resp = handle(post("/api/upgrade", None), state()).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn install_accepts_loopback_origin() {
        // Not 403: with an empty JSON body the handler proceeds to a 400.
        let resp = handle(post("/api/install", Some("http://127.0.0.1:8080")), state()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn install_accepts_missing_origin() {
        let resp = handle(post("/api/install", None), state()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn remove_rejects_foreign_origin() {
        let resp = handle(post("/api/remove", Some("https://evil.example")), state()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn remove_rejects_blank_id() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/remove")
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(Full::new(Bytes::from_static(br#"{"id": "  "}"#)))
            .expect("request");
        let resp = handle(req, state()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn remove_reaches_manager_with_valid_body() {
        // With an empty backend set the manager reports NotFound, which the
        // handler surfaces as a 500 — proof the request passed validation.
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/remove")
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(Full::new(Bytes::from_static(
                br#"{"id": "htop", "source": "fedora"}"#,
            )))
            .expect("request");
        let resp = handle(req, state()).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn url_decode_handles_escapes_and_plus() {
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("100%"), "100%");
    }

    #[tokio::test]
    async fn upgrade_rejects_loopback_suffix_origins() {
        // Prefix lookalikes must not pass the CSRF guard.
        for origin in [
            "http://127.0.0.1.evil.example",
            "http://localhost.evil.example",
        ] {
            let resp = handle(post("/api/upgrade", Some(origin)), state()).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "origin: {origin}");
        }
    }

    #[tokio::test]
    async fn upgrade_rejects_garbage_origin() {
        let resp = handle(post("/api/upgrade", Some("not a url")), state()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn upgrade_accepts_loopback_origins_with_any_port() {
        for origin in [
            "http://127.0.0.1:8080",
            "http://localhost:3000",
            "https://[::1]:8443",
            "http://user@127.0.0.1:8080",
        ] {
            let resp = handle(post("/api/upgrade", Some(origin)), state()).await;
            assert_eq!(resp.status(), StatusCode::OK, "origin: {origin}");
        }
    }

    #[test]
    fn origin_host_parses_authority() {
        assert_eq!(origin_host("http://127.0.0.1:8080"), Some("127.0.0.1"));
        assert_eq!(origin_host("https://localhost/path"), Some("localhost"));
        assert_eq!(origin_host("http://[::1]:8080"), Some("::1"));
        assert_eq!(origin_host("http://user@localhost"), Some("localhost"));
        assert_eq!(
            origin_host("http://127.0.0.1.evil.example"),
            Some("127.0.0.1.evil.example")
        );
        assert_eq!(origin_host("not a url"), None);
        assert_eq!(origin_host("http://"), None);
    }

    #[tokio::test]
    async fn install_rejects_empty_or_blank_id() {
        for body in [&br#"{"id": ""}"#[..], &br#"{"id": "   "}"#[..]] {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/api/install")
                .header(hyper::header::HOST, "127.0.0.1:8080")
                .header(TOKEN_HEADER, TEST_TOKEN)
                .body(Full::new(Bytes::from_static(body)))
                .expect("request");
            let resp = handle(req, state()).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let body = body_bytes(resp).await;
            let err: serde_json::Value = serde_json::from_slice(&body).expect("error JSON");
            assert_eq!(err["error"], "missing package id");
        }
    }

    #[tokio::test]
    async fn install_rejects_oversized_body() {
        let big = vec![b'x'; MAX_BODY_BYTES + 1];
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/install")
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .header(TOKEN_HEADER, TEST_TOKEN)
            .body(Full::new(Bytes::from(big)))
            .expect("request");
        let resp = handle(req, state()).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn packages_accepts_url_encoded_query() {
        let resp = handle(get("/api/packages?q=a%20b"), state()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let pkgs: serde_json::Value = serde_json::from_slice(&body).expect("packages JSON");
        assert_eq!(pkgs, serde_json::json!([]));
    }

    #[tokio::test]
    async fn packages_accepts_valid_source_filter() {
        for uri in [
            "/api/packages?q=x&source=fedora",
            "/api/packages?q=x&source=copr",
            "/api/packages?q=x&source=flatpak",
        ] {
            let resp = handle(get(uri), state()).await;
            assert_eq!(resp.status(), StatusCode::OK, "uri: {uri}");
        }
    }

    #[tokio::test]
    async fn packages_rejects_unknown_source_filter() {
        let resp = handle(get("/api/packages?q=x&source=bogus"), state()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_bytes(resp).await;
        let err: serde_json::Value = serde_json::from_slice(&body).expect("error JSON");
        assert_eq!(err["error"], "unknown source: bogus");
    }

    #[tokio::test]
    async fn responses_carry_security_headers() {
        let resp = handle(get("/api/stats"), state()).await;
        assert_eq!(
            resp.headers()
                .get("x-content-type-options")
                .expect("nosniff"),
            "nosniff"
        );

        let resp = handle(get("/"), state()).await;
        let headers = resp.headers();
        assert_eq!(
            headers.get("x-content-type-options").expect("nosniff"),
            "nosniff"
        );
        assert_eq!(headers.get("x-frame-options").expect("frame"), "DENY");
        assert!(headers
            .get("content-security-policy")
            .expect("csp")
            .to_str()
            .expect("csp str")
            .contains("default-src 'self'"));
    }

    #[tokio::test]
    async fn api_requires_token() {
        // A request without the session token never reaches the API.
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/stats")
            .header(hyper::header::HOST, "127.0.0.1:8080")
            .body(Full::new(Bytes::new()))
            .expect("request");
        let resp = handle(req, state()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn requests_reject_foreign_host() {
        // DNS-rebinding guard: a non-loopback Host fails closed, even on
        // token-free routes.
        for uri in ["/api/stats", "/"] {
            let req = Request::builder()
                .method(Method::GET)
                .uri(uri)
                .header(hyper::header::HOST, "evil.example")
                .header(TOKEN_HEADER, TEST_TOKEN)
                .body(Full::new(Bytes::new()))
                .expect("request");
            let resp = handle(req, state()).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "uri: {uri}");
        }
    }
}
