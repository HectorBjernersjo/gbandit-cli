mod git;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use clap::{Parser, Subcommand, ValueEnum};
use ignore::WalkBuilder;
use reqwest::StatusCode;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use tar::Builder;
use tempfile::NamedTempFile;

const BUILD_VERSION: &str = env!("GBANDIT_BUILD_VERSION");

#[derive(Parser)]
#[command(name = "gbandit", version = BUILD_VERSION)]
struct Cli {
    /// Show subprocess output and per-stage status events.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Prefix every line with a timestamp.
    #[arg(short, long, global = true)]
    timestamps: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy)]
struct Printer {
    verbose: bool,
    timestamps: bool,
    json: bool,
}

impl Printer {
    /// CLI-side progress line. Always shown; timestamp follows the flag.
    fn progress(&self, msg: impl AsRef<str>) {
        let line = if self.timestamps {
            format!("{} {}", local_timestamp(), msg.as_ref())
        } else {
            msg.as_ref().to_string()
        };
        if self.json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    /// Info-level Log event (headline): always shown.
    fn info(&self, stage: &str, created_at: Option<&str>, text: &str) {
        for line in text.lines() {
            self.print_event_line(stage, created_at, line, self.verbose);
        }
    }

    /// Debug-level Log event: shown only with `--verbose`.
    fn debug(&self, stage: &str, created_at: Option<&str>, text: &str) {
        if !self.verbose {
            return;
        }
        for line in text.lines() {
            self.print_event_line(stage, created_at, line, true);
        }
    }

    /// Status transition. Hidden in default mode; failed stages are surfaced
    /// separately with their buffered log.
    fn status(&self, stage: &str, created_at: Option<&str>, kind: &str, detail: Option<&str>) {
        if !self.verbose {
            return;
        }
        let line = match detail {
            Some(d) if !d.is_empty() => format!("{kind}: {d}"),
            _ => kind.to_string(),
        };
        self.print_event_line(stage, created_at, &line, true);
    }

    fn print_event_line(
        &self,
        stage: &str,
        created_at: Option<&str>,
        line: &str,
        with_label: bool,
    ) {
        let prefix_ts = if self.timestamps {
            created_at
                .and_then(format_event_timestamp)
                .unwrap_or_else(local_timestamp)
        } else {
            String::new()
        };
        let label = if with_label {
            format!("[{stage}] ")
        } else {
            String::new()
        };
        let rendered = if prefix_ts.is_empty() {
            format!("{label}{line}")
        } else {
            format!("{prefix_ts} {label}{line}")
        };
        if self.json {
            eprintln!("{rendered}");
        } else {
            println!("{rendered}");
        }
    }
}

/// `HH:MM:SS.cc` from the local wall clock.
fn local_timestamp() -> String {
    let now = chrono::Local::now();
    let cs = now.timestamp_subsec_millis() / 10;
    format!("{}.{cs:02}", now.format("%H:%M:%S"))
}

/// RFC3339 → `HH:MM:SS.cc` in local time. `None` on parse failure.
fn format_event_timestamp(created_at: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(created_at).ok()?;
    let local = parsed.with_timezone(&chrono::Local);
    let cs = local.timestamp_subsec_millis() / 10;
    Some(format!("{}.{cs:02}", local.format("%H:%M:%S")))
}

/// Reqwest client preconfigured with `Gbandit-Client` so backends can
/// route per-version behaviour (e.g. archive format support).
fn http_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    let value = format!("gbandit-cli/{BUILD_VERSION}");
    let header =
        reqwest::header::HeaderValue::from_str(&value).expect("build version must be ASCII");
    headers.insert(
        reqwest::header::HeaderName::from_static("gbandit-client"),
        header,
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("reqwest client must build with static headers")
}

#[derive(Subcommand)]
enum Command {
    Login,
    Whoami,
    Sql {
        query: String,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    Deploy {
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
        /// Becomes the git commit message and the checkpoint label.
        #[arg(short, long)]
        message: Option<String>,
        /// Overwrite the latest deployed code lineage when this checkout does
        /// not contain it. Use after intentional history rewrites.
        #[arg(long)]
        overwrite: bool,
        /// Return after creating the Pipeline Run instead of waiting for completion.
        #[arg(long)]
        detach: bool,
        /// Emit stable machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    Logs {
        component: LogTarget,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    Env {
        #[command(subcommand)]
        action: EnvAction,
    },
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },
    Logout,
}

#[derive(Subcommand)]
enum ProjectAction {
    /// Delete a project entirely (ADR 0004). No undo. Owner-only.
    Delete {
        /// Project slug to delete.
        slug: String,
        /// Skip the type-the-slug interactive confirm. Use only in scripts.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum MigrateAction {
    /// Roll the dev tenant DB to a specific migration version. Dev-only —
    /// prod is forward-only by ADR 0002.
    DownTo {
        /// Target migration version. Anything below the project's Migration
        /// Floor (max version ever applied to prod) is rejected.
        target: i64,
        #[arg(long)]
        project: Option<String>,
        #[arg(short, long)]
        message: Option<String>,
    },
}

#[derive(Subcommand)]
enum EnvAction {
    Set {
        /// KEY=VALUE pairs to set
        #[arg(required = true)]
        pairs: Vec<String>,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    List {
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
    Delete {
        key: String,
        #[arg(short, long, default_value_t = Environment::Dev)]
        environment: Environment,
        #[arg(long)]
        project: Option<String>,
    },
}

/// Mirrors `tenant_routing::Environment`; duplicated to avoid the dep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
enum Environment {
    Dev,
    Prod,
}

impl Environment {
    fn as_str(self) -> &'static str {
        match self {
            Environment::Dev => "dev",
            Environment::Prod => "prod",
        }
    }
}

impl std::fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Deserialize)]
struct ProjectConfig {
    project: String,
    #[serde(default)]
    local_dev: LocalDevConfig,
}

impl ProjectConfig {
    /// Local developer preference only. The Pi Agent runs without this config
    /// and keeps the default auto-commit checkpoint behaviour.
    fn auto_commit(&self) -> bool {
        if std::env::var("GBANDIT_AGENT").is_ok() {
            return true;
        }
        self.local_dev.auto_commit
    }
}

#[derive(Debug, Deserialize)]
struct LocalDevConfig {
    /// When true (default), `deploy` auto-commits a dirty tree so every
    /// deploy is a checkpoint. False: never touches git history.
    #[serde(default = "default_auto_commit")]
    auto_commit: bool,
}

impl Default for LocalDevConfig {
    fn default() -> Self {
        Self {
            auto_commit: default_auto_commit(),
        }
    }
}

fn default_auto_commit() -> bool {
    true
}

#[derive(Clone, Debug, ValueEnum)]
enum LogTarget {
    Backend,
    Frontend,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Component {
    Frontend,
    Backend,
    Project,
    BackendMigrate,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredCredentials {
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

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct SourceUploadPipeline {
    pipeline_run_id: i64,
}

#[derive(Debug, Deserialize, Clone)]
struct PipelineChildStatus {
    /// `None` when the stage hasn't reported yet. Used on snapshot/reconnect
    /// to dump already-failed stages without waiting for a missed event.
    #[serde(default)]
    status: Option<String>,
    log_output: String,
}

#[derive(Debug, Deserialize, Clone)]
struct PipelineRun {
    component: Component,
    status: String,
    error_summary: Option<String>,
    frontend_build: PipelineChildStatus,
    frontend_publish: PipelineChildStatus,
    backend_migrate: PipelineChildStatus,
    backend_build: PipelineChildStatus,
    backend_deploy: PipelineChildStatus,
}

#[derive(Debug, Deserialize)]
struct PipelineStreamDelta {
    #[serde(default)]
    created_at: Option<String>,
    event: StreamEvent,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    stage: String,
    kind: String,
    /// `info` (headline, always shown) or `debug` (verbose-only). ADR 0003.
    #[serde(default = "default_log_level")]
    level: String,
    #[serde(default)]
    detail: Option<String>,
}

fn default_log_level() -> String {
    "debug".to_string()
}

#[derive(Debug, Deserialize)]
struct BackendLogsResponse {
    logs: String,
}

#[derive(Debug, Deserialize)]
struct QueryColumn {
    name: String,
    #[serde(rename = "data_type")]
    _data_type: String,
}

#[derive(Debug, Deserialize)]
struct QueryResponse {
    columns: Vec<QueryColumn>,
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct EnvVarsApiResponse {
    vars: BTreeMap<String, String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let printer = Printer {
        verbose: cli.verbose,
        timestamps: cli.timestamps,
        json: matches!(&cli.command, Command::Deploy { json: true, .. }),
    };
    match cli.command {
        Command::Login => login(&printer).await,
        Command::Whoami => whoami(&printer).await,
        Command::Sql {
            environment,
            project,
            query,
        } => {
            let project = resolve_project(project)?;
            sql(environment.as_str(), &project, &query).await
        }
        Command::Deploy {
            environment,
            project,
            message,
            overwrite,
            detach,
            json,
        } => {
            let config = load_project_config(project)?;
            deploy(
                &printer,
                environment.as_str(),
                &config,
                message.as_deref(),
                overwrite,
                detach,
                json,
            )
            .await
        }
        Command::Logs {
            environment,
            component,
            project,
        } => {
            let project = resolve_project(project)?;
            logs(&printer, environment.as_str(), component, &project).await
        }
        Command::Env { action } => match action {
            EnvAction::Set {
                pairs,
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_set(&printer, environment.as_str(), &project, &pairs).await
            }
            EnvAction::List {
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_list(&printer, environment.as_str(), &project).await
            }
            EnvAction::Delete {
                key,
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_delete(&printer, environment.as_str(), &project, &key).await
            }
        },
        Command::Migrate { action } => match action {
            MigrateAction::DownTo {
                target,
                project,
                message,
            } => {
                let project = resolve_project(project)?;
                migrate_down_to(&printer, &project, target, message.as_deref()).await
            }
        },
        Command::Project { action } => match action {
            ProjectAction::Delete { slug, yes } => project_delete(&printer, &slug, yes).await,
        },
        Command::Logout => logout(&printer).await,
    }
}

async fn login(printer: &Printer) -> Result<()> {
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

async fn whoami(printer: &Printer) -> Result<()> {
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

async fn sql(environment: &str, project: &str, query: &str) -> Result<()> {
    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .post(format!(
            "{}/projects/{}/database/query?environment={}",
            auth.platform_api_origin, project, environment
        ))
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({ "query": query }))
        .send()
        .await
        .context("failed to execute query")?;
    let result: QueryResponse = parse_json(response).await?;
    print_query_result(&result.columns, &result.rows);
    Ok(())
}

fn print_query_result(columns: &[QueryColumn], rows: &[Vec<serde_json::Value>]) {
    if columns.is_empty() {
        println!("Query executed successfully. No rows returned.");
        return;
    }

    let mut widths: Vec<usize> = columns.iter().map(|c| c.name.len()).collect();
    for row in rows {
        for (i, val) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(format_value(val).len());
            }
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:width$}", c.name, width = widths[i]))
        .collect();
    println!(" {} ", header.join(" | "));

    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("-{}-", sep.join("-+-"));

    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let width = widths.get(i).copied().unwrap_or(0);
                format!("{:width$}", format_value(v), width = width)
            })
            .collect();
        println!(" {} ", cells.join(" | "));
    }

    println!("({} rows)", rows.len());
}

fn format_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

async fn migrate_down_to(
    printer: &Printer,
    project: &str,
    target: i64,
    message: Option<&str>,
) -> Result<()> {
    if target < 0 {
        bail!("target migration version must be >= 0");
    }
    if !PathBuf::from("backend/migrations").is_dir() {
        bail!(
            "no backend/migrations/ directory in current workspace — `gbandit migrate down-to` must run from the project root with the migrations dir present"
        );
    }

    printer.progress("Minting access token...");
    let auth = load_auth().await?;
    printer.progress("Creating migrations archive...");
    let archive = build_component_archive("backend/migrations")?;
    let mut form = Form::new()
        .part(
            "bundle",
            Part::bytes(fs::read(archive.path())?)
                .file_name("migrations.tar.zst".to_string())
                .mime_str("application/zstd")?,
        )
        .text("target_migration_version", target.to_string());
    if let Some(msg) = message {
        form = form.text("deploy_message", msg.to_string());
    }

    let client = http_client();
    printer.progress("Uploading archive...");
    let response = client
        .post(format!(
            "{}/projects/{}/backend/migrate-down?environment=dev",
            auth.platform_api_origin, project
        ))
        .bearer_auth(&auth.token)
        .multipart(form)
        .send()
        .await
        .context("failed to upload migrate-down request")?;
    let upload: SourceUploadPipeline = parse_json(response).await?;

    printer.progress(format!(
        "Migrating dev tenant DB for project {project} down to version {target}..."
    ));
    watch_deploy_pipeline(
        printer,
        &client,
        &auth.platform_api_origin,
        &auth.token,
        upload.pipeline_run_id,
    )
    .await
}

async fn deploy(
    printer: &Printer,
    environment: &str,
    config: &ProjectConfig,
    message: Option<&str>,
    overwrite: bool,
    detach: bool,
    json: bool,
) -> Result<()> {
    let (client, auth, upload) =
        start_deploy(printer, environment, config, message, overwrite).await?;

    if json {
        println!("{}", serde_json::to_string(&upload)?);
    } else if detach {
        printer.progress(format!(
            "Started deploy {environment} for project {} as Pipeline Run #{}.",
            config.project, upload.pipeline_run_id
        ));
    }

    if detach {
        return Ok(());
    }

    printer.progress(format!(
        "Deploying {environment} for project {}...",
        config.project
    ));
    watch_deploy_pipeline(
        printer,
        &client,
        &auth.platform_api_origin,
        &auth.token,
        upload.pipeline_run_id,
    )
    .await
}

async fn start_deploy(
    printer: &Printer,
    environment: &str,
    config: &ProjectConfig,
    message: Option<&str>,
    overwrite: bool,
) -> Result<(reqwest::Client, CliAuth, SourceUploadPipeline)> {
    if overwrite {
        printer.progress(
            "Overwriting deploy lineage if necessary. Use this only after an intentional git history rewrite.",
        );
    }

    let project = &config.project;
    let (commit_sha, deploy_message) =
        prepare_checkpoint(printer, config.auto_commit(), environment, message)?;

    // Push gate (ADR 0005): if `origin` is configured, push must succeed
    // before the Pipeline Run is triggered. Dirty local-dev deploys have no
    // checkpoint commit, so there is nothing correct to push.
    if commit_sha.is_some() {
        push_or_abort(printer)?;
    }

    let auth = load_auth().await?;
    let archive = build_component_archive("project")?;
    let mut form = Form::new().part(
        "bundle",
        Part::bytes(fs::read(archive.path())?)
            .file_name("project.tar.zst".to_string())
            .mime_str("application/zstd")?,
    );
    if let Some(sha) = commit_sha.as_deref() {
        form = form.text("commit_sha", sha.to_string());
    }
    if let Some(msg) = deploy_message.as_deref() {
        form = form.text("deploy_message", msg.to_string());
    }
    form = form.text("has_origin", git::has_origin()?.to_string());
    if let Some(commits) = git::known_commits()? {
        form = form.text("known_commits", commits.join("\n"));
    }
    if overwrite {
        form = form.text("overwrite", "true");
    }

    let client = http_client();
    let response = client
        .post(format!(
            "{}/projects/{}/project/uploads?environment={}",
            auth.platform_api_origin, project, environment
        ))
        .bearer_auth(&auth.token)
        .multipart(form)
        .send()
        .await
        .context("failed to upload project source")?;
    let upload: SourceUploadPipeline = parse_json(response).await?;
    Ok((client, auth, upload))
}

/// Push gate (ADR 0005). Aborts the deploy if push fails.
fn push_or_abort(printer: &Printer) -> Result<()> {
    if !git::has_origin()? {
        return Ok(());
    }
    printer.progress("Pushing to linked remote...");
    match git::push_main()? {
        git::PushOutcome::Ok => {
            printer.progress("Push succeeded.");
            Ok(())
        }
        git::PushOutcome::NoRemote => Ok(()),
        git::PushOutcome::NonFastForward { detail } => bail!(
            "push rejected (non-fast-forward) — pull from the linked remote and retry. \
             If you're in Pi, ask it to use the pull_remote skill. From a laptop, run `git pull --rebase`. \
             Aborting deploy.\n\n{detail}"
        ),
        git::PushOutcome::Network { detail } => bail!(
            "push failed — network unreachable. Aborting deploy so the Checkpoint stays in sync with the remote.\n\n{detail}"
        ),
        git::PushOutcome::Auth { detail } => bail!(
            "push failed — authentication rejected. \
             Looks like the Deploy Key isn't installed any more (or your laptop's git credentials are wrong). \
             Reconnect from the Settings page or fix your local credentials, then retry. \
             Aborting deploy.\n\n{detail}"
        ),
    }
}

/// Returns `(commit_sha, deploy_message)` for the platform.
/// - auto_commit=true, dirty: commit then return HEAD.
/// - auto_commit=true, clean: return HEAD (no empty commit).
/// - auto_commit=false, dirty: `commit_sha = None` (deploy, not checkpoint).
/// - auto_commit=false, clean: return HEAD (still lands as a checkpoint).
fn prepare_checkpoint(
    printer: &Printer,
    auto_commit: bool,
    environment: &str,
    message: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let deploy_message = message.map(str::to_string);
    let canned = || format!("gbandit deploy {environment}");
    let commit_message = message.map(str::to_string).unwrap_or_else(canned);

    let clean = match git::is_clean() {
        Ok(value) => value,
        Err(err) => {
            if !auto_commit {
                printer.progress(format!("Skipping commit_sha: {err}"));
                return Ok((None, deploy_message));
            }
            return Err(err);
        }
    };

    if auto_commit && !clean {
        printer.progress("Auto-committing working tree...");
        git::commit_all(&commit_message)?;
    }

    let sha = git::head_sha()?;
    if !auto_commit && !clean {
        printer.progress(
            "Deploying uncommitted local changes. Linked remote will not be pushed; the Gbandit Agent will not see these changes unless you commit/push/sync them.",
        );
        return Ok((None, deploy_message));
    }
    Ok((sha, deploy_message))
}

async fn logs(
    printer: &Printer,
    environment: &str,
    component: LogTarget,
    project: &str,
) -> Result<()> {
    match component {
        LogTarget::Backend => backend_logs(environment, project).await,
        LogTarget::Frontend => frontend_logs(printer, environment, project).await,
    }
}

async fn backend_logs(environment: &str, project: &str) -> Result<()> {
    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .get(format!(
            "{}/projects/{}/backend/logs?environment={}&tail_lines=2000",
            auth.platform_api_origin, project, environment
        ))
        .bearer_auth(&auth.token)
        .send()
        .await
        .context("failed to fetch backend logs")?;
    let snapshot: BackendLogsResponse = parse_json(response).await?;
    if !snapshot.logs.is_empty() {
        print!("{}", snapshot.logs);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct FrontendLogsListResponse {
    logs: Vec<FrontendLog>,
}

#[derive(Debug, Deserialize)]
struct FrontendLog {
    level: String,
    message: String,
    source_url: Option<String>,
    user_name: Option<String>,
    user_is_anon: Option<bool>,
    created_at: String,
}

async fn frontend_logs(printer: &Printer, environment: &str, project: &str) -> Result<()> {
    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .get(format!(
            "{}/projects/{}/frontend/logs?environment={}&limit=200",
            auth.platform_api_origin, project, environment
        ))
        .bearer_auth(&auth.token)
        .send()
        .await
        .context("failed to fetch frontend logs")?;
    let snapshot: FrontendLogsListResponse = parse_json(response).await?;

    if snapshot.logs.is_empty() {
        printer.progress("No frontend logs recorded.");
        return Ok(());
    }

    // Response is newest-first; print oldest-first.
    for entry in snapshot.logs.iter().rev() {
        let time = entry.created_at.get(11..19).unwrap_or(&entry.created_at);
        let level = entry.level.to_uppercase();
        let user = entry
            .user_name
            .as_deref()
            .unwrap_or_else(|| match entry.user_is_anon {
                Some(true) => "anon",
                _ => "-",
            });
        let path = entry.source_url.as_deref().map(url_path).unwrap_or("/");
        println!(
            "{time} {level:<5}  [{user} @ {path}] {msg}",
            msg = entry.message,
        );
    }
    Ok(())
}

/// Strip scheme + host from a URL.
fn url_path(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    match after_scheme.find('/') {
        Some(idx) => &after_scheme[idx..],
        None => "/",
    }
}

async fn env_set(
    printer: &Printer,
    environment: &str,
    project: &str,
    pairs: &[String],
) -> Result<()> {
    let mut vars = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("invalid KEY=VALUE pair: {pair}"))?;
        vars.insert(key.to_string(), value.to_string());
    }

    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .put(format!(
            "{}/projects/{}/env?environment={}",
            auth.platform_api_origin, project, environment
        ))
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({ "vars": vars }))
        .send()
        .await
        .context("failed to set environment variables")?;
    let result: EnvVarsApiResponse = parse_json(response).await?;
    for (key, value) in &result.vars {
        printer.progress(format!("{key}={value}"));
    }
    Ok(())
}

async fn env_list(printer: &Printer, environment: &str, project: &str) -> Result<()> {
    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .get(format!(
            "{}/projects/{}/env?environment={}",
            auth.platform_api_origin, project, environment
        ))
        .bearer_auth(&auth.token)
        .send()
        .await
        .context("failed to list environment variables")?;
    let result: EnvVarsApiResponse = parse_json(response).await?;
    if result.vars.is_empty() {
        printer.progress("No environment variables set.");
    } else {
        for (key, value) in &result.vars {
            printer.progress(format!("{key}={value}"));
        }
    }
    Ok(())
}

async fn env_delete(printer: &Printer, environment: &str, project: &str, key: &str) -> Result<()> {
    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .delete(format!(
            "{}/projects/{}/env/{}?environment={}",
            auth.platform_api_origin, project, key, environment
        ))
        .bearer_auth(&auth.token)
        .send()
        .await
        .context("failed to delete environment variable")?;
    if !response.status().is_success() {
        bail!(parse_error(response).await);
    }
    printer.progress(format!("Deleted {key}"));
    Ok(())
}

async fn project_delete(printer: &Printer, slug: &str, skip_prompt: bool) -> Result<()> {
    if !skip_prompt {
        // Friction at the presentation layer (ADR 0004 §7).
        printer.progress(format!(
            "About to permanently delete project '{slug}'. This destroys the dev"
        ));
        printer.progress("and prod databases, all deploys, all uploaded assets, and");
        printer.progress("the GitHub link. There is no undo and no Restore path.");
        print!("Type the slug to confirm: ");
        std::io::stdout().flush().ok();
        let mut typed = String::new();
        std::io::stdin()
            .read_line(&mut typed)
            .context("failed to read confirmation")?;
        if typed.trim() != slug {
            bail!("aborted: typed value did not match slug");
        }
    }

    let auth = load_auth().await?;
    let client = http_client();
    let response = client
        .delete(format!("{}/projects/{slug}", auth.platform_api_origin))
        .bearer_auth(&auth.token)
        .send()
        .await
        .context("failed to delete project")?;

    match response.status() {
        StatusCode::ACCEPTED => {
            // ADR 0007: deletion is async. The project shows up as `deleting`
            // in the UI / list endpoint until the background reconciler frees
            // the slug.
            printer.progress(format!(
                "Deletion started for project '{slug}'. The slug stays reserved \
                 until namespace, database, and PVCs are fully torn down."
            ));
            Ok(())
        }
        StatusCode::FORBIDDEN => bail!("forbidden: only owners can delete a project"),
        StatusCode::NOT_FOUND => bail!("project '{slug}' not found"),
        _ => bail!(parse_error(response).await),
    }
}

async fn logout(printer: &Printer) -> Result<()> {
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
    let token: AccessTokenResponse = parse_json(response).await?;
    Ok(token.access_token)
}

struct CliAuth {
    token: String,
    platform_api_origin: String,
}

/// Uses `GBANDIT_ACCESS_TOKEN` (e.g. inside an agent pod) when set;
/// otherwise loads disk credentials and mints a fresh access token.
async fn load_auth() -> Result<CliAuth> {
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

/// Per-stage Debug-line buffer, dumped if the stage fails.
type StageLogBuffer = HashMap<String, Vec<(Option<String>, String)>>;

async fn watch_deploy_pipeline(
    printer: &Printer,
    client: &reqwest::Client,
    platform_api_origin: &str,
    token: &str,
    pipeline_run_id: i64,
) -> Result<()> {
    let mut response = client
        .get(format!(
            "{platform_api_origin}/pipelines/{pipeline_run_id}/stream"
        ))
        .bearer_auth(token)
        .header("accept", "text/event-stream")
        .send()
        .await
        .context("failed to open deploy event stream")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("stream request failed ({status}): {body}");
    }

    let mut buf = String::new();
    let mut buffers: StageLogBuffer = HashMap::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read event stream chunk")?
    {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buf.find("\n\n") {
            let raw_event = buf[..end].to_string();
            buf.drain(..end + 2);
            if let Some(outcome) = handle_sse_event(printer, &mut buffers, &raw_event)? {
                return outcome;
            }
        }
    }
    bail!("deploy event stream ended unexpectedly")
}

/// `Some(result)` when the deploy reaches a terminal state.
fn handle_sse_event(
    printer: &Printer,
    buffers: &mut StageLogBuffer,
    raw: &str,
) -> Result<Option<Result<()>>> {
    let mut event_type = String::new();
    let mut data_lines: Vec<&str> = Vec::new();
    for line in raw.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("event:") {
            event_type = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("data:") {
            data_lines.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    let data = data_lines.join("\n");

    match event_type.as_str() {
        "snapshot" => {
            let snap: PipelineRun =
                serde_json::from_str(&data).context("failed to decode snapshot")?;
            // Reconnect catch-up: seed buffers from the snapshot's
            // log_output, and dump any stage already in `failed` since
            // no Failed event will arrive.
            for (label, child) in pipeline_stages(&snap) {
                if !child.log_output.is_empty() {
                    buffers.insert(label.to_string(), vec![(None, child.log_output.clone())]);
                    if printer.verbose {
                        printer.debug(label, None, &child.log_output);
                    }
                }
                if child.status.as_deref() == Some("failed") {
                    dump_failed_stage(printer, buffers, label, None);
                }
            }
            printer.status("pipeline", None, "status", Some(&snap.status));
            if is_terminal_status(&snap.status) {
                return Ok(Some(finish_terminal(
                    printer,
                    &snap.status,
                    snap.error_summary,
                )));
            }
        }
        "" | "pipeline_event" => {
            let delta: PipelineStreamDelta = match serde_json::from_str(&data) {
                Ok(d) => d,
                Err(_) => return Ok(None),
            };
            let stage = delta.event.stage.clone();
            let created_at = delta.created_at.as_deref();
            match delta.event.kind.as_str() {
                "log" => {
                    let text = delta.event.detail.as_deref().unwrap_or("");
                    let is_info = delta.event.level == "info";
                    if is_info {
                        printer.info(&stage, created_at, text);
                    } else {
                        // Buffer for potential failure dump; stream in verbose.
                        buffers
                            .entry(stage.clone())
                            .or_default()
                            .push((delta.created_at.clone(), text.to_string()));
                        printer.debug(&stage, created_at, text);
                    }
                }
                "started" | "succeeded" => {
                    if stage != "pipeline" {
                        printer.status(&stage, created_at, delta.event.kind.as_str(), None);
                    }
                }
                "skipped" => {
                    printer.status(&stage, created_at, "skipped", delta.event.detail.as_deref());
                }
                "failed" => {
                    let reason = delta.event.detail.as_deref().unwrap_or("");
                    if stage != "pipeline" {
                        // Always surface failures, even in default mode.
                        printer.print_event_line(
                            &stage,
                            created_at,
                            &format!("failed: {reason}"),
                            true,
                        );
                        dump_failed_stage(printer, buffers, &stage, created_at);
                    }
                }
                "cancelled" => {
                    let reason = delta.event.detail.as_deref().unwrap_or("");
                    printer.print_event_line(
                        &stage,
                        created_at,
                        &format!("cancelled: {reason}"),
                        true,
                    );
                }
                _ => {}
            }

            if stage == "pipeline" {
                match delta.event.kind.as_str() {
                    "succeeded" => {
                        printer.progress("Deploy succeeded.");
                        return Ok(Some(Ok(())));
                    }
                    "failed" => {
                        let summary = delta
                            .event
                            .detail
                            .clone()
                            .unwrap_or_else(|| "deploy failed".into());
                        return Ok(Some(Err(anyhow::anyhow!("{}", summary))));
                    }
                    "cancelled" => {
                        let reason = delta
                            .event
                            .detail
                            .clone()
                            .unwrap_or_else(|| "cancelled".into());
                        return Ok(Some(Err(anyhow::anyhow!("deploy cancelled: {}", reason))));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

/// Print the buffered Debug log for a failed stage, with `--- log ---`
/// markers so the failure boundary stands out.
fn dump_failed_stage(
    printer: &Printer,
    buffers: &mut StageLogBuffer,
    stage: &str,
    created_at: Option<&str>,
) {
    let entries = buffers.remove(stage).unwrap_or_default();
    if entries.is_empty() && printer.verbose {
        return;
    }
    println!("--- {stage} log ---");
    for (entry_ts, text) in &entries {
        for line in text.lines() {
            printer.print_event_line(stage, entry_ts.as_deref(), line, false);
        }
    }
    println!("--- end {stage} log ---");
    let _ = created_at;
}

fn finish_terminal(printer: &Printer, status: &str, error_summary: Option<String>) -> Result<()> {
    if status == "succeeded" {
        printer.progress("Deploy succeeded.");
        Ok(())
    } else {
        bail!(
            "{}",
            error_summary.unwrap_or_else(|| format!("deploy {status}"))
        )
    }
}

fn pipeline_stages(pipeline: &PipelineRun) -> Vec<(&'static str, &PipelineChildStatus)> {
    let frontend = [
        ("frontend_build", &pipeline.frontend_build),
        ("frontend_publish", &pipeline.frontend_publish),
    ];
    let backend = [
        ("backend_migrate", &pipeline.backend_migrate),
        ("backend_build", &pipeline.backend_build),
        ("backend_deploy", &pipeline.backend_deploy),
    ];
    match pipeline.component {
        Component::Frontend => frontend.into_iter().collect(),
        Component::Backend => backend.into_iter().collect(),
        Component::Project => frontend.into_iter().chain(backend).collect(),
        Component::BackendMigrate => vec![("backend_migrate", &pipeline.backend_migrate)],
    }
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "skipped" | "cancelled")
}

fn build_component_archive(component: &str) -> Result<NamedTempFile> {
    let root = match component {
        "project" => PathBuf::from("."),
        other => PathBuf::from(other),
    };
    if !root.is_dir() {
        bail!("component directory not found: {}", root.display());
    }

    let temp = NamedTempFile::new().context("failed to create temporary archive")?;
    let writer = temp
        .reopen()
        .context("failed to reopen temporary archive")?;
    // zstd level 3 is the default and a strict win over gzip default (6):
    // smaller output and several times faster on the projects we bundle.
    let encoder =
        zstd::stream::Encoder::new(writer, 3).context("failed to initialise zstd encoder")?;
    let mut tar = Builder::new(encoder);

    // Rely on .gitignore (and .ignore) for skipping build outputs, dependency
    // directories, and other developer-local files. `.git` is the one exception
    // — it can't be gitignored — so we hard-skip it. `hidden(false)` keeps
    // dotfiles like `.gitignore` and `.dockerignore` in the bundle since
    // they're meaningful project config.
    let walker = WalkBuilder::new(&root)
        .standard_filters(true)
        .hidden(false)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| name != ".git")
                .unwrap_or(true)
        })
        .build();

    for entry in walker {
        let entry = entry.context("failed to walk project directory")?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };

        let relative = path
            .strip_prefix(&root)
            .with_context(|| format!("failed to strip archive root for {}", path.display()))?;

        if file_type.is_dir() {
            tar.append_dir(relative, path)?;
        } else if file_type.is_file() {
            let mut file = fs::File::open(path)?;
            tar.append_file(relative, &mut file)?;
        }
    }

    let encoder = tar.into_inner().context("failed to finalize tar archive")?;
    let mut file = encoder.finish().context("failed to finish zstd archive")?;
    file.flush().context("failed to flush archive")?;

    Ok(temp)
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

fn auth_origin() -> String {
    std::env::var("GBANDIT_AUTH_ORIGIN").expect("GBANDIT_AUTH_ORIGIN must be set")
}

fn platform_api_origin() -> String {
    std::env::var("GBANDIT_PLATFORM_API_ORIGIN").expect("GBANDIT_PLATFORM_API_ORIGIN must be set")
}

fn resolve_project(cli_project: Option<String>) -> Result<String> {
    if let Some(project) = cli_project {
        return Ok(project);
    }
    Ok(read_gbandit_json()?.project)
}

/// Like `resolve_project` but returns the full config. When `--project`
/// is passed without gbandit.json we synthesise defaults so deploys can
/// run from outside a checked-in workspace.
fn load_project_config(cli_project: Option<String>) -> Result<ProjectConfig> {
    match cli_project {
        Some(project) => match read_gbandit_json() {
            Ok(mut cfg) => {
                cfg.project = project;
                Ok(cfg)
            }
            Err(_) => Ok(ProjectConfig {
                project,
                local_dev: LocalDevConfig::default(),
            }),
        },
        None => read_gbandit_json(),
    }
}

fn read_gbandit_json() -> Result<ProjectConfig> {
    let path = PathBuf::from("gbandit.json");
    let bytes = fs::read(&path)
        .with_context(|| "no --project flag and no gbandit.json found in the current directory")?;
    serde_json::from_slice(&bytes).context("failed to parse gbandit.json")
}

async fn parse_json<T: for<'de> Deserialize<'de>>(response: reqwest::Response) -> Result<T> {
    if response.status().is_success() {
        return response
            .json::<T>()
            .await
            .context("failed to decode response JSON");
    }

    bail!(parse_error(response).await)
}

async fn parse_error(response: reqwest::Response) -> String {
    let status = response.status();
    match response.json::<ErrorResponse>().await {
        Ok(payload) => payload.error,
        Err(_) => format!("{status}: request failed"),
    }
}
