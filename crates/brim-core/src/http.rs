//! Shared native HTTP client (pure-Rust TLS via rustls).
//!
//! All of Brim's HTTP access goes through this module: the COPR REST API,
//! the Flathub trending collection, and icon downloads. One client per
//! call site gives connection reuse for bursts (e.g. icon floods).

use std::time::Duration;

use crate::error::{BrimError, Result};

/// Timeout applied to every HTTP request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Build the shared client.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("brim/", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT)
        .build()
        .expect("reqwest client builder cannot fail with this configuration")
}

/// GET `url` and return the body as text; non-2xx becomes
/// [`BrimError::Http`] (mirrors the old `curl --fail` semantics).
pub async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = send(client.get(url)).await?;
    response.text().await.map_err(http_err)
}

/// GET `url` with query parameters (URL-encoded by reqwest) and return the
/// body as text.
pub async fn get_text_query(
    client: &reqwest::Client,
    url: &str,
    params: &[(&str, &str)],
) -> Result<String> {
    let response = send(client.get(url).query(params)).await?;
    response.text().await.map_err(http_err)
}

/// GET `url` and return the body as bytes.
pub async fn get_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = send(client.get(url)).await?;
    Ok(response.bytes().await.map_err(http_err)?.to_vec())
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
}
