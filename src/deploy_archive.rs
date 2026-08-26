use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use tar::Builder;
use tempfile::NamedTempFile;

pub(crate) fn build_project_archive() -> Result<NamedTempFile> {
    build_project_archive_in(Path::new("."))
}

fn build_project_archive_in(root: &Path) -> Result<NamedTempFile> {
    if !root.is_dir() {
        bail!("project directory not found: {}", root.display());
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

    // gbandit.jsonc goes first so the server's config peek stops at the first
    // entry instead of decompressing the whole archive to find it.
    let config = root.join("gbandit.jsonc");
    if config.is_file() {
        let mut config_file = fs::File::open(&config)?;
        tar.append_file("gbandit.jsonc", &mut config_file)?;
    }

    // Rely on .gitignore (and .ignore) for skipping build outputs, dependency
    // directories, and other developer-local files. `.git` is the one exception
    // — it can't be gitignored — so we hard-skip it. `hidden(false)` keeps
    // dotfiles like `.gitignore` and `.dockerignore` in the bundle since
    // they're meaningful project config.
    let walker = WalkBuilder::new(root)
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
        if path == root || path == config {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };

        let archive_path = path
            .strip_prefix(root)
            .with_context(|| format!("failed to strip archive root for {}", path.display()))?;

        if file_type.is_dir() {
            tar.append_dir(archive_path, path)?;
        } else if file_type.is_file() {
            let mut file = fs::File::open(path)?;
            tar.append_file(archive_path, &mut file)?;
        }
    }

    let encoder = tar.into_inner().context("failed to finalize tar archive")?;
    let mut file = encoder.finish().context("failed to finish zstd archive")?;
    file.flush().context("failed to flush archive")?;

    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::build_project_archive_in;

    fn archive_entries(archive: &tempfile::NamedTempFile) -> Vec<String> {
        let file = std::fs::File::open(archive.path()).unwrap();
        let decoder = zstd::stream::Decoder::new(file).unwrap();
        let mut tar = tar::Archive::new(decoder);
        tar.entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn project_archive_puts_config_first_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gbandit.jsonc"), "{ \"project\": \"p\" }").unwrap();
        std::fs::create_dir_all(dir.path().join("frontend")).unwrap();
        std::fs::write(dir.path().join("frontend/index.html"), "<html>").unwrap();

        let archive = build_project_archive_in(dir.path()).unwrap();
        let entries = archive_entries(&archive);
        assert_eq!(entries.first().map(String::as_str), Some("gbandit.jsonc"));
        assert_eq!(
            entries.iter().filter(|e| *e == "gbandit.jsonc").count(),
            1,
            "{entries:?}"
        );
        assert!(
            entries.contains(&"frontend/index.html".to_string()),
            "{entries:?}"
        );
    }
}
