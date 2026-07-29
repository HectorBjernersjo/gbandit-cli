use std::fs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::multipart::{Form, Part};

use crate::config::{DatabaseEngine, ProjectConfig};
use crate::deploy_archive::{build_component_archive, build_migrate_down_archive};
use crate::git;
use crate::pipeline_watch::watch_deploy_pipeline;
use crate::platform_client::{PlatformClient, SourceUploadPipeline};
use crate::printer::Printer;
use crate::scaffold::title_from_slug;

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
        create: bool,
        detach: bool,
        json: bool,
    ) -> Result<()> {
        let (client, upload) = self
            .start_deploy(environment, config, message, overwrite, baseline, create, json)
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
        create: bool,
        json: bool,
    ) -> Result<(PlatformClient, Option<SourceUploadPipeline>)> {
        if overwrite {
            self.printer.progress(
                "Overwriting deploy lineage if necessary. Use this only after an intentional git history rewrite.",
            );
        }

        let project = &config.project;

        if config.database == DatabaseEngine::None && has_up_migrations() {
            bail!(
                "you have migrations in backend/migrations but database=none in gbandit.json; set database to sqlite or postgres"
            );
        }

        // Ensure the platform project exists (and its title matches
        // gbandit.json) before any local side effects like the checkpoint
        // auto-commit — answering "n" to the create prompt must leave the
        // working tree untouched.
        let client = PlatformClient::from_saved_auth().await?;
        self.ensure_project(&client, config, create, json).await?;

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

        let timing = std::env::var("GBANDIT_TIMING").is_ok();

        let archive_started = std::time::Instant::now();
        let archive = build_component_archive("project")?;
        if timing {
            eprintln!(
                "@timing phase=archive ms={}",
                archive_started.elapsed().as_millis()
            );
        }
        let archive_bytes = fs::read(archive.path())?;
        let submission_id = uuid::Uuid::new_v4().to_string();
        let has_origin = git::has_origin()?.to_string();
        let known_commits = git::known_commits()?.map(|commits| commits.join("\n"));

        let upload_started = std::time::Instant::now();
        let upload = client
            .upload_project_source(project, environment, || {
                let mut form = Form::new()
                    .text("submission_id", submission_id.clone())
                    .text("has_origin", has_origin.clone());
                if let Some(sha) = commit_sha.as_deref() {
                    form = form.text("commit_sha", sha.to_string());
                }
                if let Some(msg) = deploy_message.as_deref() {
                    form = form.text("deploy_message", msg.to_string());
                }
                if let Some(commits) = known_commits.as_deref() {
                    form = form.text("known_commits", commits.to_string());
                }
                if overwrite {
                    form = form.text("overwrite", "true");
                }
                if baseline {
                    form = form.text("baseline", "true");
                }
                Ok(form.part(
                    "bundle",
                    Part::bytes(archive_bytes.clone())
                        .file_name("project.tar.zst".to_string())
                        .mime_str("application/zstd")?,
                ))
            })
            .await?;
        if timing {
            eprintln!(
                "@timing phase=upload ms={}",
                upload_started.elapsed().as_millis()
            );
        }
        Ok((client, upload))
    }

    /// Create-on-deploy (with confirmation) plus title sync: gbandit.json is
    /// the source of truth for the title whenever it carries one.
    async fn ensure_project(
        &self,
        client: &PlatformClient,
        config: &ProjectConfig,
        create: bool,
        json: bool,
    ) -> Result<()> {
        let slug = &config.project;
        match client.get_project(slug).await? {
            Some(existing) => {
                if let Some(title) = config.title.as_deref().map(str::trim) {
                    if title != existing.title {
                        client.update_project_title(slug, title).await?;
                        self.printer
                            .progress(format!("Updated project title to \"{title}\"."));
                    }
                }
            }
            None => {
                if !create && !confirm_create(slug, json)? {
                    bail!("aborted: project '{slug}' was not created");
                }
                let title = config
                    .title
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_string)
                    .unwrap_or_else(|| title_from_slug(slug));
                self.printer.progress(format!(
                    "Creating project '{slug}' (title: \"{title}\") on the platform..."
                ));
                let created = client.create_project(slug, &title).await?;
                self.printer
                    .progress(format!("Project '{}' created.", created.slug));
            }
        }
        Ok(())
    }
}

/// Typo guard: a misspelled `project` in gbandit.json must not silently
/// become a fresh project. `--create` is the non-interactive opt-in.
fn confirm_create(slug: &str, json: bool) -> Result<bool> {
    if json || !std::io::stdin().is_terminal() {
        bail!(
            "project '{slug}' does not exist on the platform — pass --create to create it on deploy"
        );
    }
    print!("Project '{slug}' does not exist on the platform. Create it? [Y/n] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation")?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ))
}

/// True when `backend/migrations` holds at least one `*.up.sql` file — the
/// signal that the project expects a database (ADR 0014 §1.4).
fn has_up_migrations() -> bool {
    let Ok(entries) = fs::read_dir("backend/migrations") else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".up.sql"))
    })
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
    let archive = build_migrate_down_archive()?;
    let archive_bytes = fs::read(archive.path())?;
    let submission_id = uuid::Uuid::new_v4().to_string();

    printer.progress("Uploading archive...");
    let upload = client
        .upload_migrate_down(project, || {
            let mut form = Form::new()
                .text("target_migration_version", target.to_string())
                .text("submission_id", submission_id.clone());
            if let Some(msg) = message {
                form = form.text("deploy_message", msg.to_string());
            }
            Ok(form.part(
                "bundle",
                Part::bytes(archive_bytes.clone())
                    .file_name("migrations.tar.zst".to_string())
                    .mime_str("application/zstd")?,
            ))
        })
        .await?;

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
