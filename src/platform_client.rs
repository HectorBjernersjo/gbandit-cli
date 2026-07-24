use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::multipart::Form;
use serde::{Deserialize, Serialize};

use crate::auth_session::load_auth;
use crate::http::{http_client, parse_error, parse_json};

pub(crate) struct PlatformClient {
    http: reqwest::Client,
    token: String,
    origin: String,
}

impl PlatformClient {
    pub(crate) async fn from_saved_auth() -> Result<Self> {
        let auth = load_auth().await?;
        Ok(Self {
            http: http_client(),
            token: auth.token,
            origin: auth.platform_api_origin,
        })
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) async fn query_database(
        &self,
        environment: &str,
        project: &str,
        query: &str,
    ) -> Result<QueryResponse> {
        let response = self
            .http
            .post(format!(
                "{}/projects/{}/database/query?environment={}",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .context("failed to execute query")?;
        parse_json(response)
            .await
            .with_context(|| format!("query against project '{project}' ({environment}) failed"))
    }

    /// `Ok(None)` = the server skipped a baseline deploy (200 instead of 202)
    /// because the project already has a succeeded deploy.
    pub(crate) async fn upload_project_source<F>(
        &self,
        project: &str,
        environment: &str,
        make_form: F,
    ) -> Result<Option<SourceUploadPipeline>>
    where
        F: Fn() -> Result<Form>,
    {
        let url = format!(
            "{}/projects/{}/project/uploads?environment={}",
            self.origin, project, environment
        );
        let response = self
            .post_multipart_with_retry(&url, &make_form)
            .await
            .context("failed to upload project source")?;
        if response.status() == reqwest::StatusCode::OK {
            return Ok(None);
        }
        Ok(Some(parse_json(response).await.with_context(|| {
            format!("failed to start deploy for project '{project}' ({environment})")
        })?))
    }

    pub(crate) async fn upload_migrate_down<F>(
        &self,
        project: &str,
        make_form: F,
    ) -> Result<SourceUploadPipeline>
    where
        F: Fn() -> Result<Form>,
    {
        let url = format!(
            "{}/projects/{}/backend/migrate-down?environment=dev",
            self.origin, project
        );
        let response = self
            .post_multipart_with_retry(&url, &make_form)
            .await
            .context("failed to upload migrate-down request")?;
        parse_json(response)
            .await
            .with_context(|| format!("failed to start migrate-down for project '{project}'"))
    }

    async fn post_multipart_with_retry<F>(
        &self,
        url: &str,
        make_form: &F,
    ) -> Result<reqwest::Response>
    where
        F: Fn() -> Result<Form>,
    {
        let mut last_transport_error = None;
        for attempt in 0..3 {
            let request = self
                .http
                .post(url)
                .bearer_auth(&self.token)
                .multipart(make_form()?);
            match request.send().await {
                Ok(response) if response.status().is_server_error() && attempt < 2 => {
                    tokio::time::sleep(std::time::Duration::from_millis(250 * (attempt + 1))).await;
                }
                Ok(response) => return Ok(response),
                Err(error) if attempt < 2 => {
                    last_transport_error = Some(error);
                    tokio::time::sleep(std::time::Duration::from_millis(250 * (attempt + 1))).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(last_transport_error
            .expect("three retry attempts always retain a transport error")
            .into())
    }

    pub(crate) async fn backend_logs(&self, environment: &str, project: &str) -> Result<String> {
        let response = self
            .http
            .get(format!(
                "{}/projects/{}/backend/logs?environment={}&tail_lines=2000",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to fetch backend logs")?;
        let snapshot: BackendLogsResponse = parse_json(response).await.with_context(|| {
            format!("failed to fetch backend logs for project '{project}' ({environment})")
        })?;
        Ok(snapshot.logs)
    }

    pub(crate) async fn frontend_logs(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<Vec<FrontendLog>> {
        let response = self
            .http
            .get(format!(
                "{}/projects/{}/frontend/logs?environment={}&limit=200",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to fetch frontend logs")?;
        let snapshot: FrontendLogsListResponse = parse_json(response).await.with_context(|| {
            format!("failed to fetch frontend logs for project '{project}' ({environment})")
        })?;
        Ok(snapshot.logs)
    }

    pub(crate) async fn set_env(
        &self,
        environment: &str,
        project: &str,
        vars: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let response = self
            .http
            .put(format!(
                "{}/projects/{}/env?environment={}",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "vars": vars }))
            .send()
            .await
            .context("failed to set environment variables")?;
        let result: EnvVarsApiResponse = parse_json(response).await.with_context(|| {
            format!("failed to set env vars for project '{project}' ({environment})")
        })?;
        Ok(result.vars)
    }

    pub(crate) async fn list_env(
        &self,
        environment: &str,
        project: &str,
    ) -> Result<EnvVarsApiResponse> {
        let response = self
            .http
            .get(format!(
                "{}/projects/{}/env?environment={}",
                self.origin, project, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to list environment variables")?;
        parse_json(response).await.with_context(|| {
            format!("failed to list env vars for project '{project}' ({environment})")
        })
    }

    pub(crate) async fn delete_env(
        &self,
        environment: &str,
        project: &str,
        key: &str,
    ) -> Result<()> {
        let response = self
            .http
            .delete(format!(
                "{}/projects/{}/env/{}?environment={}",
                self.origin, project, key, environment
            ))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to delete environment variable")?;
        if !response.status().is_success() {
            let error = parse_error(response).await;
            bail!(
                "failed to delete env var {key} for project '{project}' ({environment}): {error}"
            );
        }
        Ok(())
    }

    pub(crate) async fn create_project(&self, slug: &str, title: &str) -> Result<CreatedProject> {
        let response = self
            .http
            .post(format!("{}/projects", self.origin))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "slug": slug, "title": title }))
            .send()
            .await
            .context("failed to create project")?;

        match response.status() {
            StatusCode::CREATED => parse_json(response).await,
            // parse_error already maps 401 to a `gbandit login` hint.
            _ => bail!(parse_error(response).await),
        }
    }

    pub(crate) async fn delete_project(&self, slug: &str) -> Result<ProjectDeleteOutcome> {
        let response = self
            .http
            .delete(format!("{}/projects/{slug}", self.origin))
            .bearer_auth(&self.token)
            .send()
            .await
            .context("failed to delete project")?;

        match response.status() {
            StatusCode::ACCEPTED => Ok(ProjectDeleteOutcome::Started),
            StatusCode::NOT_FOUND => bail!("project '{slug}' not found"),
            _ => bail!(parse_error(response).await),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryColumn {
    pub(crate) name: String,
    #[serde(rename = "data_type")]
    pub(crate) _data_type: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueryResponse {
    pub(crate) columns: Vec<QueryColumn>,
    pub(crate) rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SourceUploadPipeline {
    pub(crate) pipeline_run_id: i64,
}

#[derive(Debug, Deserialize)]
struct BackendLogsResponse {
    logs: String,
}

#[derive(Debug, Deserialize)]
struct FrontendLogsListResponse {
    logs: Vec<FrontendLog>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FrontendLog {
    pub(crate) level: String,
    pub(crate) message: String,
    pub(crate) source_url: Option<String>,
    pub(crate) user_name: Option<String>,
    pub(crate) user_is_anon: Option<bool>,
    pub(crate) created_at: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EnvVarsApiResponse {
    pub(crate) vars: BTreeMap<String, String>,
    pub(crate) system_vars: BTreeMap<String, String>,
}

pub(crate) enum ProjectDeleteOutcome {
    Started,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreatedProject {
    pub(crate) slug: String,
}
