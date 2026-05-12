//! The only place in the CLI that shells out to `git`.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// True when the working tree has no uncommitted changes and no untracked files.
pub fn is_clean() -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .context("failed to run `git status` — is git installed and is this a git repository?")?;
    if !output.status.success() {
        bail!(
            "`git status` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output
        .stdout
        .iter()
        .all(|b| matches!(*b, b' ' | b'\t' | b'\n' | b'\r')))
}

/// Caller must have ensured the tree is dirty.
pub fn commit_all(message: &str) -> Result<()> {
    let add = Command::new("git")
        .args(["add", "-A"])
        .output()
        .context("failed to run `git add -A`")?;
    if !add.status.success() {
        bail!(
            "`git add -A` failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }

    let commit = Command::new("git")
        .args(["commit", "-m", message])
        .output()
        .context("failed to run `git commit`")?;
    if !commit.status.success() {
        bail!(
            "`git commit` failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    Ok(())
}

/// `None` when the repo has no commits yet.
pub fn head_sha() -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("failed to run `git rev-parse HEAD`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("unknown revision") || stderr.contains("ambiguous argument") {
            return Ok(None);
        }
        bail!("`git rev-parse HEAD` failed: {}", stderr.trim());
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
}

/// All commits reachable from HEAD, newest first. `None` when this is not a
/// git repository or HEAD does not exist yet.
pub fn known_commits() -> Result<Option<Vec<String>>> {
    let output = Command::new("git")
        .args(["rev-list", "HEAD"])
        .output()
        .context("failed to run `git rev-list HEAD`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        if stderr.contains("not a git repository")
            || stderr.contains("unknown revision")
            || stderr.contains("ambiguous argument")
        {
            return Ok(None);
        }
        bail!(
            "`git rev-list HEAD` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let commits = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    Ok(Some(commits))
}

/// Outcome of the push-or-abort gate inside `gbandit deploy` (ADR 0005).
/// No origin → `NoRemote` (deploy proceeds without push); otherwise push
/// and categorise so the CLI can show a useful next-step message.
pub enum PushOutcome {
    Ok,
    /// No `origin` configured — Project not linked. Deploy proceeds.
    NoRemote,
    /// Remote moved forward; user should pull/rebase and retry.
    NonFastForward {
        detail: String,
    },
    /// Offline / DNS / firewall.
    Network {
        detail: String,
    },
    /// Deploy key removed on host, or wrong personal credentials on laptop.
    Auth {
        detail: String,
    },
}

pub fn has_origin() -> Result<bool> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .context("failed to run `git remote get-url origin`")?;
    Ok(output.status.success() && !output.stdout.is_empty())
}

/// Push HEAD to `origin/main`, categorising the outcome. Gates Pipeline Runs
/// when the project has a Linked Remote configured.
pub fn push_main() -> Result<PushOutcome> {
    if !has_origin()? {
        return Ok(PushOutcome::NoRemote);
    }
    let output = Command::new("git")
        .args(["push", "origin", "HEAD:main"])
        .output()
        .context("failed to run `git push`")?;
    if output.status.success() {
        return Ok(PushOutcome::Ok);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stderr_lower = stderr.to_lowercase();
    if stderr_lower.contains("non-fast-forward")
        || stderr_lower.contains("rejected")
        || stderr_lower.contains("fetch first")
        || stderr_lower.contains("updates were rejected")
    {
        return Ok(PushOutcome::NonFastForward {
            detail: stderr.trim().to_string(),
        });
    }
    if stderr_lower.contains("could not resolve host")
        || stderr_lower.contains("network is unreachable")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("connection timed out")
        || stderr_lower.contains("temporary failure in name resolution")
    {
        return Ok(PushOutcome::Network {
            detail: stderr.trim().to_string(),
        });
    }
    if stderr_lower.contains("permission denied")
        || stderr_lower.contains("could not read from remote repository")
        || stderr_lower.contains("authentication failed")
        || stderr_lower.contains("publickey")
        || stderr_lower.contains("403")
    {
        return Ok(PushOutcome::Auth {
            detail: stderr.trim().to_string(),
        });
    }
    bail!("`git push` failed: {}", stderr.trim())
}
