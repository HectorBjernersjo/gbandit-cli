//! Interactive project creation: prompts for title + slug, calls the
//! platform API to create the project, then scaffolds the template into
//! ./<slug>/ (or the current dir when the user passed ".").

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::platform_client::PlatformClient;
use crate::printer::Printer;
use crate::scaffold::{self, ScaffoldOptions};

pub(crate) async fn run(
    printer: &Printer,
    name: Option<String>,
    title: Option<String>,
) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();

    // `.` is the in-place sentinel: scaffold into the current dir and prompt
    // separately for the slug. Any other value is both the slug and the
    // target dir name (matching `cargo new <name>`).
    let in_place = name.as_deref() == Some(".");

    let title = match title {
        Some(t) => t.trim().to_string(),
        None => prompt_required(&mut stdin, "Project title", validate_title_input)?,
    };

    let slug = if in_place {
        let suggested = scaffold::slugify(&title);
        prompt_with_default(
            &mut stdin,
            "Project slug (your game will be at your-slug.gbandit.com)",
            &suggested,
            validate_slug_input,
        )?
    } else {
        match name {
            Some(n) => {
                validate_slug_input(&n).map_err(|msg| anyhow::anyhow!("invalid name: {msg}"))?;
                n
            }
            None => {
                let suggested = scaffold::slugify(&title);
                prompt_with_default(&mut stdin, "Project slug", &suggested, validate_slug_input)?
            }
        }
    };

    let target_path: PathBuf = if in_place {
        Path::new(".").to_path_buf()
    } else {
        Path::new(&slug).to_path_buf()
    };

    printer.progress(format!(
        "Creating project '{slug}' (title: \"{title}\") on the platform..."
    ));
    let client = PlatformClient::from_saved_auth().await?;
    let created = client.create_project(&slug, &title).await?;

    printer.progress(format!(
        "Project created. Scaffolding into {} ...",
        target_path.display()
    ));
    scaffold::scaffold_project(
        printer,
        ScaffoldOptions {
            slug: &created.slug,
            target: &target_path,
            init_git: true,
        },
    )?;

    if in_place {
        printer.progress("Done. Run `gbandit deploy` to ship.".to_string());
    } else {
        printer.progress(format!(
            "Done. cd {} && gbandit deploy",
            target_path.display()
        ));
    }
    Ok(())
}

fn prompt_required<R, F>(stdin: &mut R, label: &str, validate: F) -> Result<String>
where
    R: BufRead,
    F: Fn(&str) -> Result<(), String>,
{
    loop {
        print!("{label}: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("failed to read from stdin")?;
        let trimmed = line.trim().to_string();
        match validate(&trimmed) {
            Ok(()) => return Ok(trimmed),
            Err(msg) => eprintln!("  {msg}"),
        }
    }
}

fn prompt_with_default<R, F>(
    stdin: &mut R,
    label: &str,
    default: &str,
    validate: F,
) -> Result<String>
where
    R: BufRead,
    F: Fn(&str) -> Result<(), String>,
{
    loop {
        if default.is_empty() {
            print!("{label}: ");
        } else {
            print!("{label} [{default}]: ");
        }
        std::io::stdout().flush().ok();
        let mut line = String::new();
        stdin
            .read_line(&mut line)
            .context("failed to read from stdin")?;
        let trimmed = line.trim();
        let value = if trimmed.is_empty() {
            default.to_string()
        } else {
            trimmed.to_string()
        };
        match validate(&value) {
            Ok(()) => return Ok(value),
            Err(msg) => eprintln!("  {msg}"),
        }
    }
}

fn validate_title_input(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("title cannot be empty".to_string());
    }
    if value.chars().count() > 80 {
        return Err("title must be 80 characters or fewer".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err("title may not contain control characters".to_string());
    }
    Ok(())
}

fn validate_slug_input(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 63 {
        return Err("slug must be 1–63 characters".to_string());
    }
    let bytes = value.as_bytes();
    let edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !edge_ok(bytes[0]) || !edge_ok(bytes[bytes.len() - 1]) {
        return Err("slug must start and end with [a-z0-9]".to_string());
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("slug may only contain [a-z0-9-]".to_string());
    }
    for prefix in ["dev-", "stage-", "prod-"] {
        if value.starts_with(prefix) {
            return Err(format!(
                "slug may not start with reserved prefix '{prefix}'"
            ));
        }
    }
    Ok(())
}
