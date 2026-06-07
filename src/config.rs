use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProjectConfig {
    pub(crate) project: String,
    #[serde(default)]
    local_dev: LocalDevConfig,
}

impl ProjectConfig {
    /// Local developer preference only. The Pi Agent runs without this config
    /// and keeps the default auto-commit checkpoint behaviour.
    pub(crate) fn auto_commit(&self) -> bool {
        if std::env::var("GBANDIT_AGENT").is_ok() {
            return true;
        }
        self.local_dev.auto_commit
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalDevConfig {
    /// When true (default), `deploy` auto-commits a dirty tree and pushes to
    /// the linked remote so every deploy is a synced checkpoint. False: the
    /// CLI never touches git — no commits, no push; you sync the remote
    /// yourself.
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

/// Env vars are dev overrides (set by the gbandit-dev alias); defaults point at prod.
pub(crate) fn auth_origin() -> String {
    std::env::var("GBANDIT_AUTH_ORIGIN").unwrap_or_else(|_| "https://auth.gbandit.com".into())
}

pub(crate) fn platform_api_origin() -> String {
    std::env::var("GBANDIT_PLATFORM_API_ORIGIN")
        .unwrap_or_else(|_| "https://platform.gbandit.com/api".into())
}

pub(crate) fn resolve_project(cli_project: Option<String>) -> Result<String> {
    if let Some(project) = cli_project {
        return Ok(project);
    }
    Ok(read_gbandit_json()?.project)
}

/// Like `resolve_project` but returns the full config. When `--project`
/// is passed without gbandit.json we synthesise defaults so deploys can
/// run from outside a checked-in workspace.
pub(crate) fn load_project_config(cli_project: Option<String>) -> Result<ProjectConfig> {
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
