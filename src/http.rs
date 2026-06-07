use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Reqwest client preconfigured with `Gbandit-Client` so backends can
/// route per-version behaviour (e.g. archive format support).
pub(crate) fn http_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    let value = format!("gbandit-cli/{}", crate::BUILD_VERSION);
    let header =
        reqwest::header::HeaderValue::from_str(&value).expect("build version must be ASCII");
    headers.insert(
        reqwest::header::HeaderName::from_static("gbandit-client"),
        header,
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("gbandit-cli"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("reqwest client must build with static headers")
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

pub(crate) async fn parse_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T> {
    let status = response.status();
    if status.is_success() {
        let bytes = response
            .bytes()
            .await
            .context("failed to read response body")?;
        return serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "failed to decode response JSON (status {status}): {}",
                body_snippet(&bytes)
            )
        });
    }

    bail!(parse_error(response).await)
}

pub(crate) async fn parse_error(response: reqwest::Response) -> String {
    let status = response.status();
    let message = match response.json::<ErrorResponse>().await {
        Ok(payload) => payload.error,
        Err(_) => format!("request failed with status {status}"),
    };
    if status == reqwest::StatusCode::UNAUTHORIZED {
        format!("{message} — run `gbandit login` to re-authenticate")
    } else {
        message
    }
}

fn body_snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }
    let mut snippet: String = trimmed.chars().take(200).collect();
    if trimmed.chars().count() > 200 {
        snippet.push('…');
    }
    snippet
}
