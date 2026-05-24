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
    if response.status().is_success() {
        return response
            .json::<T>()
            .await
            .context("failed to decode response JSON");
    }

    bail!(parse_error(response).await)
}

pub(crate) async fn parse_error(response: reqwest::Response) -> String {
    let status = response.status();
    match response.json::<ErrorResponse>().await {
        Ok(payload) => payload.error,
        Err(_) => format!("{status}: request failed"),
    }
}
