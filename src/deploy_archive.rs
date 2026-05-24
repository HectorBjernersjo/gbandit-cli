use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use tar::Builder;
use tempfile::NamedTempFile;

pub(crate) fn build_component_archive(component: &str) -> Result<NamedTempFile> {
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
