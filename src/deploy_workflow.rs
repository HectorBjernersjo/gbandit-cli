use std::fs;
use std::path::PathBuf;

use anyhow::{Result, bail};
use reqwest::multipart::{Form, Part};

use crate::config::ProjectConfig;
use crate::deploy_archive::build_component_archive;
use crate::git;
use crate::pipeline_watch::watch_deploy_pipeline;
use crate::platform_client::{PlatformClient, SourceUploadPipeline};
use crate::printer::Printer;

pub(crate) struct DeployWorkflow<'a> {
    printer: &'a Printer,
}

impl<'a> DeployWorkflow<'a> {
    pub(crate) fn new(printer: &'a Printer) -> Self {
        Self { printer }
    }

    pub(crate) async fn deploy(
        &self,
        environment: &str,
        config: &ProjectConfig,
        message: Option<&str>,
        overwrite: bool,
        baseline: bool,
        detach: bool,
        json: bool,
    ) -> Result<()> {
        let (client, upload) = self
            .start_deploy(environment, config, message, overwrite, baseline)
            .await?;

        let Some(upload) = upload else {
            if json {
                println!("{}", serde_json::json!({ "status": "baseline_skipped" }));
            } else {
                self.printer.progress(
                    "Baseline deploy skipped — the project already has a succeeded deploy.",
                );
            }
            return Ok(());
        };

        if json {
            println!("{}", serde_json::to_string(&upload)?);
        } else if detach {
            self.printer.progress(format!(
                "Started deploy {environment} for project {} as Pipeline Run #{}.",
                config.project, upload.pipeline_run_id
            ));
        }

        if detach {
            return Ok(());
        }

        self.printer.progress(format!(
            "Deploying {environment} for project {}...",
            config.project
        ));
        watch_deploy_pipeline(
            self.printer,
            client.http(),
            client.origin(),
            client.token(),
            upload.pipeline_run_id,
        )
        .await
    }

    async fn start_deploy(
        &self,
        environment: &str,
        config: &ProjectConfig,
        message: Option<&str>,
        overwrite: bool,
        baseline: bool,
    ) -> Result<(PlatformClient, Option<SourceUploadPipeline>)> {
        if overwrite {
            self.printer.progress(
                "Overwriting deploy lineage if necessary. Use this only after an intentional git history rewrite.",
            );
        }

        let project = &config.project;
        let (commit_sha, deploy_message) =
            prepare_checkpoint(self.printer, config.auto_commit(), environment, message)?;

        // Push gate (ADR 0005): if `origin` is configured, push must succeed
        // before the Pipeline Run is triggered. Dirty local-dev deploys have no
        // checkpoint commit, so there is nothing correct to push. With
        // auto_commit=false the user syncs the linked remote themselves; the
        // checkpoint becomes restorable once its commit reaches the remote
        // (unreachable commits are filtered as orphans, PRD 0005).
        if commit_sha.is_some() {
            if config.auto_commit() {
                push_or_abort(self.printer)?;
            } else if git::has_origin()? {
                self.printer.progress(
                    "Skipping push to linked remote (auto_commit=false). The checkpoint becomes restorable once you push.",
                );
            }
        }

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
        if baseline {
            form = form.text("baseline", "true");
        }

        let client = PlatformClient::from_saved_auth().await?;
        let upload = client
            .upload_project_source(project, environment, form)
            .await?;
        Ok((client, upload))
    }
}

pub(crate) async fn migrate_down_to(
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
    let client = PlatformClient::from_saved_auth().await?;
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

    printer.progress("Uploading archive...");
    let upload = client.upload_migrate_down(project, form).await?;

    printer.progress(format!(
        "Migrating dev tenant DB for project {project} down to version {target}..."
    ));
    watch_deploy_pipeline(
        printer,
        client.http(),
        client.origin(),
        client.token(),
        upload.pipeline_run_id,
    )
    .await
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
/// - auto_commit=false, clean: return HEAD (still lands as a checkpoint,
///   but the deploy never pushes — restorable once the user pushes).
fn prepare_checkpoint(
    printer: &Printer,
    auto_commit: bool,
    environment: &str,
    message: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let deploy_message = message.map(str::to_string);
    let canned = || format!("gbandit deploy {environment}");
    let commit_message = message.map(str::to_string).unwrap_or_else(canned);

    if !git::in_repo()? {
        if !auto_commit {
            printer.progress("Skipping commit_sha: not a git repository.");
            return Ok((None, deploy_message));
        }
        bail!(
            "deploy checkpoints require a git repository and this directory is not one — \
             run `git init`, or set local_dev.auto_commit to false in gbandit.json \
             to deploy without checkpoints"
        );
    }

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
