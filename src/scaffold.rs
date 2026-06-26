//! Materialise a fresh project workspace from the gbandit-game template.
//!
//! Shared by `gbandit new` (local, interactive) and `gbandit scaffold`
//! (non-interactive, used by the Pi Agent entrypoint). Owning the
//! clone+substitute+gbandit.json+initial-commit flow in one place lets the
//! agent image stop shelling out to git/sed for the same job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::printer::Printer;

const DEFAULT_TEMPLATE_REPO: &str = "https://github.com/HectorBjernersjo/gbandit-game";

pub(crate) struct ScaffoldOptions<'a> {
    pub(crate) slug: &'a str,
    pub(crate) target: &'a Path,
    /// When true, run `git init -b main` and an initial commit. The Pi
    /// Agent pod sets this so the workspace PVC starts with a single
    /// linear-history commit owned entirely by this project.
    pub(crate) init_git: bool,
}

pub(crate) fn scaffold_project(printer: &Printer, opts: ScaffoldOptions<'_>) -> Result<()> {
    if !is_valid_slug(opts.slug) {
        bail!(
            "invalid slug '{}': must be 1–63 chars, start/end with [a-z0-9], contain only [a-z0-9-]",
            opts.slug
        );
    }

    fs::create_dir_all(opts.target)
        .with_context(|| format!("failed to create target dir {}", opts.target.display()))?;

    if !is_empty_dir(opts.target)? {
        bail!(
            "target directory {} is not empty — scaffold refuses to overwrite",
            opts.target.display()
        );
    }

    let repo_url = std::env::var("GBANDIT_GAME_REPO_URL")
        .unwrap_or_else(|_| DEFAULT_TEMPLATE_REPO.to_string());

    printer.progress(format!("Cloning template from {repo_url}..."));
    let tmp = tempfile::tempdir().context("failed to create temp dir for template clone")?;
    let clone_dst = tmp.path().join("clone");
    run_git(&[
        "clone",
        "--depth=1",
        &repo_url,
        clone_dst.to_str().context("clone path is not utf-8")?,
    ])?;

    // Throw away the template's shallow history so the new workspace owns
    // its own linear git history. Carrying the template's graft boundary
    // into the workspace breaks future `git push` to a user-linked remote.
    let template_git_dir = clone_dst.join(".git");
    if template_git_dir.exists() {
        fs::remove_dir_all(&template_git_dir).with_context(|| {
            format!(
                "failed to drop template .git at {}",
                template_git_dir.display()
            )
        })?;
    }

    copy_dir_contents(&clone_dst, opts.target)?;

    let slug_underscored = opts.slug.replace('-', "_");
    substitute_placeholders(opts.target, opts.slug, &slug_underscored)?;

    write_gbandit_json(opts.target, opts.slug)?;

    if opts.init_git {
        printer.progress("Initialising git repo with initial commit...");
        run_git_in(opts.target, &["init", "-b", "main"])?;
        run_git_in(opts.target, &["add", "-A"])?;
        run_git_in(
            opts.target,
            &["commit", "-m", "Initial commit from gbandit-game template"],
        )?;
    }

    Ok(())
}

fn is_valid_slug(slug: &str) -> bool {
    if slug.is_empty() || slug.len() > 63 {
        return false;
    }
    let bytes = slug.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !edge_ok(bytes[0]) || !edge_ok(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

pub(crate) fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = true;
    for ch in title.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 63 {
        out.truncate(63);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn is_empty_dir(path: &Path) -> Result<bool> {
    let mut iter = fs::read_dir(path)
        .with_context(|| format!("failed to read dir {}", path.display()))?;
    Ok(iter.next().is_none())
}

fn copy_dir_contents(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src)
        .with_context(|| format!("failed to read template dir {}", src.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::create_dir_all(&to)?;
            copy_dir_contents(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &to)?;
            #[cfg(not(unix))]
            fs::copy(&from, &to)?;
            let _ = target;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn substitute_placeholders(root: &Path, dashed: &str, underscored: &str) -> Result<()> {
    let targets = collect_text_files(root)?;
    for path in targets {
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if !looks_like_text(&bytes) {
            continue;
        }
        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !content.contains("replace-with-project") && !content.contains("replace_with_project") {
            continue;
        }
        let replaced = content
            .replace("replace-with-project", dashed)
            .replace("replace_with_project", underscored);
        fs::write(&path, replaced)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn collect_text_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            walk(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn looks_like_text(bytes: &[u8]) -> bool {
    // Cheap binary sniff: NUL byte in the first 8 KiB → binary.
    let head = &bytes[..bytes.len().min(8 * 1024)];
    !head.contains(&0)
}

fn write_gbandit_json(target: &Path, slug: &str) -> Result<()> {
    let path = target.join("gbandit.json");
    let body = serde_json::json!({
        "project": slug,
        "database": "none",
        "local_dev": { "auto_commit": true },
    });
    let pretty = serde_json::to_string_pretty(&body)?;
    fs::write(&path, format!("{pretty}\n"))
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` failed with status {}", args.join(" "), status);
    }
    Ok(())
}

fn run_git_in(cwd: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
    if !status.success() {
        bail!("`git {}` failed with status {}", args.join(" "), status);
    }
    Ok(())
}
