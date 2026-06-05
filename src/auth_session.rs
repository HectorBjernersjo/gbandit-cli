use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::config::{auth_origin, platform_api_origin};
use crate::http::{http_client, parse_error, parse_json};
use crate::printer::Printer;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StoredCredentials {
    auth_origin: String,
    platform_api_origin: String,
    session_token: String,
    session_expires_at: String,
    user_id: String,
    email: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CliLoginStartResponse {
    login_id: String,
    login_secret: String,
    authorize_url: String,
    expires_at: String,
    poll_interval_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: String,
    token_type: String,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct CliLoginPollCompleteResponse {
    session_token: String,
    session_expires_at: String,
    access_token: AccessTokenResponse,
    user_id: String,
    email: Option<String>,
    name: Option<String>,
}

pub(crate) struct CliAuth {
    pub(crate) token: String,
    pub(crate) platform_api_origin: String,
}

pub(crate) async fn login(printer: &Printer) -> Result<()> {
    let client = http_client();
    let auth_origin = auth_origin();
    let response = client
        .post(format!("{auth_origin}/api/cli/login/start"))
        .send()
        .await
        .context("failed to start browser login")?;
    let start: CliLoginStartResponse = parse_json(response).await?;

    printer.progress("Open this URL to complete login:");
    printer.progress(&start.authorize_url);
    if webbrowser::open(&start.authorize_url).is_ok() {
        printer.progress("Opened browser window.");
    }
    printer.progress("Waiting for login approval...");

    loop {
        let response = client
            .post(format!("{auth_origin}/api/cli/login/poll"))
            .json(&serde_json::json!({
                "login_id": start.login_id,
                "login_secret": start.login_secret,
            }))
            .send()
            .await
            .context("failed while polling browser login")?;

        if response.status() == StatusCode::ACCEPTED {
            tokio::time::sleep(Duration::from_secs(start.poll_interval_seconds)).await;
            continue;
        }

        let completed: CliLoginPollCompleteResponse = parse_json(response).await?;
        let credentials = StoredCredentials {
            auth_origin,
            platform_api_origin: platform_api_origin(),
            session_token: completed.session_token,
            session_expires_at: completed.session_expires_at,
            user_id: completed.user_id,
            email: completed.email,
            name: completed.name,
        };
        save_credentials(&credentials)?;
        printer.progress(format!(
            "Logged in as {}",
            credentials
                .email
                .clone()
                .or(credentials.name.clone())
                .unwrap_or(credentials.user_id.clone())
        ));
        printer.progress(format!(
            "Session expires at {}",
            credentials.session_expires_at
        ));
        printer.progress(format!(
            "Browser login request expired at {}",
            start.expires_at
        ));
        printer.progress(format!(
            "Access token expires at {} ({})",
            completed.access_token.expires_at, completed.access_token.token_type
        ));
        break;
    }

    Ok(())
}

pub(crate) async fn whoami(printer: &Printer) -> Result<()> {
    let credentials = load_credentials()?;
    let display_name = credentials
        .email
        .as_deref()
        .or(credentials.name.as_deref())
        .unwrap_or(&credentials.user_id);
    printer.progress(format!("Logged in as {display_name}"));
    if let Some(name) = &credentials.name {
        printer.progress(format!("  Name:    {name}"));
    }
    if let Some(email) = &credentials.email {
        printer.progress(format!("  Email:   {email}"));
    }
    printer.progress(format!("  User ID: {}", credentials.user_id));
    printer.progress(format!(
        "  Session expires at {}",
        credentials.session_expires_at
    ));

    match cli_access_token(&credentials).await {
        Ok(_) => printer.progress("  Session is valid."),
        Err(_) => printer
            .progress("  Session is expired or invalid. Run `gbandit login` to re-authenticate."),
    }

    Ok(())
}

pub(crate) async fn logout(printer: &Printer) -> Result<()> {
    let credentials = load_credentials()?;
    let client = http_client();
    let response = client
        .post(format!("{}/api/cli/logout", credentials.auth_origin))
        .json(&serde_json::json!({
            "session_token": credentials.session_token,
        }))
        .send()
        .await
        .context("failed to log out")?;
    if !response.status().is_success() {
        let error = parse_error(response).await;
        bail!(error);
    }
    let path = credentials_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove credentials file {}", path.display()))?;
    }
    printer.progress("Logged out.");
    Ok(())
}

async fn cli_access_token(credentials: &StoredCredentials) -> Result<String> {
    let client = http_client();
    let response = client
        .post(format!("{}/api/cli/token", credentials.auth_origin))
        .json(&serde_json::json!({
            "session_token": credentials.session_token,
            "audience": "platform-api",
        }))
        .send()
        .await
        .context("failed to mint platform access token")?;
    // A 401 here means the stored session no longer authenticates. The raw
    // body is unhelpful ("401 Unauthorized: request failed"), so say whether
    // it expired and point at `gbandit login`.
    if response.status() == StatusCode::UNAUTHORIZED {
        bail!(session_rejected_message(credentials));
    }
    let token: AccessTokenResponse = parse_json(response).await?;
    Ok(token.access_token)
}

fn session_rejected_message(credentials: &StoredCredentials) -> String {
    let expired = chrono::DateTime::parse_from_rfc3339(&credentials.session_expires_at)
        .map(|expiry| expiry.with_timezone(&chrono::Utc) <= chrono::Utc::now())
        .unwrap_or(false);
    if expired {
        format!(
            "Session expired at {}. Run `gbandit login` to re-authenticate.",
            credentials.session_expires_at
        )
    } else {
        "Session is no longer valid. Run `gbandit login` to re-authenticate.".to_string()
    }
}

/// Uses `GBANDIT_ACCESS_TOKEN` (e.g. inside an agent pod) when set;
/// otherwise loads disk credentials and mints a fresh access token.
pub(crate) async fn load_auth() -> Result<CliAuth> {
    if let Ok(token) = std::env::var("GBANDIT_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            return Ok(CliAuth {
                token,
                platform_api_origin: platform_api_origin(),
            });
        }
    }
    let credentials = load_credentials()?;
    let token = cli_access_token(&credentials).await?;
    Ok(CliAuth {
        token,
        platform_api_origin: credentials.platform_api_origin,
    })
}

fn save_credentials(credentials: &StoredCredentials) -> Result<()> {
    let path = credentials_path()?;
    let parent = path
        .parent()
        .context("credentials path must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create credentials dir {}", parent.display()))?;
    let json = serde_json::to_vec_pretty(credentials)?;
    fs::write(&path, json)
        .with_context(|| format!("failed to write credentials file {}", path.display()))?;
    Ok(())
}

fn load_credentials() -> Result<StoredCredentials> {
    // Test/CI bypass: synthesise credentials from env vars when both
    // `GBANDIT_SESSION_TOKEN` and `GBANDIT_USER_ID` are set. The auth-service
    // validates the session token against its DB on every `/api/cli/token`
    // call, so a bogus token here just produces a 401 — there's no
    // local-only auth check to spoof. Used by the e2e journey suite and
    // any future non-interactive deploy paths (CI, Pi Agent).
    if let Ok(session_token) = std::env::var("GBANDIT_SESSION_TOKEN")
        && let Ok(user_id) = std::env::var("GBANDIT_USER_ID")
    {
        return Ok(StoredCredentials {
            auth_origin: auth_origin(),
            platform_api_origin: platform_api_origin(),
            session_token,
            // Actual expiry is enforced server-side against the DB row.
            session_expires_at: "2099-01-01T00:00:00Z".to_string(),
            user_id,
            email: None,
            name: None,
        });
    }

    let path = credentials_path()?;
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read credentials file {}", path.display()))?;
    serde_json::from_slice(&bytes).context("failed to parse credentials file")
}

fn credentials_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("failed to determine config directory")?;
    // Keep dev and prod credentials in separate files so they don't clobber
    // each other when a developer runs against both.
    let filename = if auth_origin().contains("localhost") {
        "credentials-dev.json"
    } else {
        "credentials.json"
    };
    Ok(config_dir.join("gbandit").join(filename))
}
