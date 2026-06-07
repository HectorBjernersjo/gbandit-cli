use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::auth_session;
use crate::cli::{Command, EnvAction, LogTarget, MigrateAction, ProjectAction};
use crate::config::{load_project_config, resolve_project};
use crate::deploy_workflow::{DeployWorkflow, migrate_down_to};
use crate::new_command;
use crate::platform_client::{PlatformClient, ProjectDeleteOutcome};
use crate::printer::Printer;
use crate::query_table::QueryTable;
use crate::release_installer::ReleaseInstaller;
use crate::scaffold::{self, ScaffoldOptions};

pub(crate) async fn run(command: Command, printer: &Printer) -> Result<()> {
    match command {
        Command::Login => auth_session::login(printer).await,
        Command::Whoami => auth_session::whoami(printer).await,
        Command::Update { tag } => {
            ReleaseInstaller::github()
                .install(printer, tag.as_deref())
                .await
        }
        Command::Sql {
            environment,
            project,
            query,
        } => {
            let project = resolve_project(project)?;
            let client = PlatformClient::from_saved_auth().await?;
            let result = client
                .query_database(environment.as_str(), &project, &query)
                .await?;
            QueryTable::new(&result).print();
            Ok(())
        }
        Command::Deploy {
            environment,
            project,
            message,
            overwrite,
            baseline,
            detach,
            json,
        } => {
            let config = load_project_config(project)?;
            DeployWorkflow::new(printer)
                .deploy(
                    environment.as_str(),
                    &config,
                    message.as_deref(),
                    overwrite,
                    baseline,
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
            logs(printer, environment.as_str(), component, &project).await
        }
        Command::Env { action } => match action {
            EnvAction::Set {
                pairs,
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_set(printer, environment.as_str(), &project, &pairs).await
            }
            EnvAction::List {
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_list(printer, environment.as_str(), &project).await
            }
            EnvAction::Delete {
                key,
                environment,
                project,
            } => {
                let project = resolve_project(project)?;
                env_delete(printer, environment.as_str(), &project, &key).await
            }
        },
        Command::Migrate { action } => match action {
            MigrateAction::DownTo {
                target,
                project,
                message,
            } => {
                let project = resolve_project(project)?;
                migrate_down_to(printer, &project, target, message.as_deref()).await
            }
        },
        Command::Project { action } => match action {
            ProjectAction::Delete { slug, yes } => project_delete(printer, &slug, yes).await,
        },
        Command::New { name, title } => new_command::run(printer, name, title).await,
        Command::Scaffold {
            project,
            target,
            git_init,
        } => {
            let target = std::path::PathBuf::from(&target);
            scaffold::scaffold_project(
                printer,
                ScaffoldOptions {
                    slug: &project,
                    target: &target,
                    init_git: git_init,
                },
            )
        }
        Command::Logout => auth_session::logout(printer).await,
    }
}

async fn logs(
    printer: &Printer,
    environment: &str,
    component: LogTarget,
    project: &str,
) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    match component {
        LogTarget::Backend => {
            let logs = client.backend_logs(environment, project).await?;
            if !logs.is_empty() {
                print!("{logs}");
            }
            Ok(())
        }
        LogTarget::Frontend => {
            let logs = client.frontend_logs(environment, project).await?;
            if logs.is_empty() {
                printer.progress("No frontend logs recorded.");
                return Ok(());
            }

            // Response is newest-first; print oldest-first.
            for entry in logs.iter().rev() {
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
    }
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

    let client = PlatformClient::from_saved_auth().await?;
    let vars = client.set_env(environment, project, vars).await?;
    for (key, value) in &vars {
        printer.progress(format!("{key}={value}"));
    }
    Ok(())
}

async fn env_list(printer: &Printer, environment: &str, project: &str) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    let vars = client.list_env(environment, project).await?;
    if vars.vars.is_empty() && vars.system_vars.is_empty() {
        printer.progress("No environment variables set.");
    } else {
        for (key, value) in &vars.vars {
            printer.progress(format!("{key}={value}"));
        }
        for (key, value) in &vars.system_vars {
            printer.progress(format!("{key}={value} [system]"));
        }
    }
    Ok(())
}

async fn env_delete(printer: &Printer, environment: &str, project: &str, key: &str) -> Result<()> {
    let client = PlatformClient::from_saved_auth().await?;
    client.delete_env(environment, project, key).await?;
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

    let client = PlatformClient::from_saved_auth().await?;
    match client.delete_project(slug).await? {
        ProjectDeleteOutcome::Started => {
            // ADR 0007: deletion is async. The project shows up as `deleting`
            // in the UI / list endpoint until the background reconciler frees
            // the slug.
            printer.progress(format!(
                "Deletion started for project '{slug}'. The slug stays reserved \
                 until namespace, database, and PVCs are fully torn down."
            ));
            Ok(())
        }
    }
}
