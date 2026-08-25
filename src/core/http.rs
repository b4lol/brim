//! Shared native HTTP client (pure-Rust TLS via rustls).
//!
//! All of Brim's HTTP access goes through this module: the COPR REST API,
//! the Flathub trending collection, and icon downloads. One process-wide
//! client (built lazily on first use) gives connection reuse across every
//! call site. Response bodies are capped so a hostile or broken server
//! cannot exhaust memory.

use std::sync::OnceLock;
use std::time::Duration;

use crate::core::error::{BrimError, Result};

/// Timeout applied to every HTTP request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for text bodies (API JSON responses).
const MAX_TEXT_BYTES: u64 = 16 * 1024 * 1024;

/// Upper bound for binary bodies (icon downloads).
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The process-wide shared client; built lazily on first use.
static SHARED: OnceLock<reqwest::Client> = OnceLock::new();

/// The shared client. Cloning a `reqwest::Client` is cheap (it shares the
/// underlying connection pool), so callers get a clone of the singleton.
pub fn client() -> reqwest::Client {
    SHARED.get_or_init(build_client).clone()
}

/// Build the client behind the shared singleton.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("brim/", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("reqwest client builder cannot fail with this configuration")
}

/// GET `url` and return the body as text; non-2xx becomes
/// [`BrimError::Http`] (mirrors the old `curl --fail` semantics). Bodies
/// larger than [`MAX_TEXT_BYTES`] are rejected.
pub async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = send(client.get(url)).await?;
    let body = body_limited(response, MAX_TEXT_BYTES).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// GET `url` with query parameters (URL-encoded by reqwest) and return the
/// body as text, with a per-request timeout overriding the client-wide
/// default — for endpoints so slow they would otherwise stall a whole
/// fan-out operation (e.g. COPR's `project/search`).
pub async fn get_text_query_timeout(
    client: &reqwest::Client,
    url: &str,
    params: &[(&str, &str)],
    timeout: Duration,
) -> Result<String> {
    let response = send(client.get(url).query(params).timeout(timeout)).await?;
    let body = body_limited(response, MAX_TEXT_BYTES).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// GET `url` and return the body as bytes, capped at [`MAX_BYTES`].
pub async fn get_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = send(client.get(url)).await?;
    body_limited(response, MAX_BYTES).await
}

/// Send a prepared request, mapping transport and status errors.
async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let response = request.send().await.map_err(http_err)?;
    let status = response.status();
    if !status.is_success() {
        let url = response.url().to_string();
        return Err(BrimError::Http(format!("GET {url} returned {status}")));
    }
    Ok(response)
}

/// Read a response body, rejecting it when it exceeds `limit` — first via
/// a declared `Content-Length`, then while streaming the chunks.
async fn body_limited(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>> {
    let url = response.url().to_string();
    if let Some(len) = response.content_length() {
        if len > limit {
            return Err(too_large(&url, limit));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_err)? {
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err(too_large(&url, limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The error produced when a body exceeds the size limit.
fn too_large(url: &str, limit: u64) -> BrimError {
    BrimError::Http(format!(
        "GET {url} exceeded the {} MiB response limit",
        limit / (1024 * 1024)
    ))
}

fn http_err(error: reqwest::Error) -> BrimError {
    BrimError::Http(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_brim_user_agent() {
        // No network: builder config only.
        let _client = client();
    }

    #[test]
    fn client_is_a_process_wide_singleton() {
        let _ = client();
        assert!(SHARED.get().is_some());
    }
}
