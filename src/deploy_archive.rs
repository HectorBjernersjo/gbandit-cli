use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use tar::Builder;
use tempfile::NamedTempFile;

fn declared_migrations(config_text: &str) -> Result<String> {
    let value = jsonc_parser::parse_to_serde_value(config_text, &Default::default())
        .map_err(|err| anyhow::anyhow!("failed to parse gbandit.jsonc: {err}"))?
        .context("gbandit.jsonc is empty")?;
    value
        .get("database")
        .and_then(|database| database.get("migrations"))
        .and_then(|migrations| migrations.as_str())
        .map(str::to_string)
        .context(
            "gbandit.jsonc does not declare database.migrations — \
             `gbandit migrate down-to` only rolls back platform-run migrations",
        )
}

/// Archive for `migrate down-to`: the declared migrations directory at the
/// archive root as `migrations/`, plus `gbandit.jsonc` at the root so the
/// executor resolves the project's engine (ADR 0014 — the workspace copy is
/// the engine source of truth). Without it the engine reads as `none` and
/// the immutability check rejects the run. The on-disk location of the
/// migrations comes from `database.migrations` in gbandit.jsonc.
pub(crate) fn build_migrate_down_archive() -> Result<NamedTempFile> {
    build_migrate_down_archive_in(Path::new("."))
}

fn build_migrate_down_archive_in(root: &Path) -> Result<NamedTempFile> {
    let config = root.join("gbandit.jsonc");
    if !config.is_file() {
        bail!("gbandit.jsonc not found in the current directory");
    }
    let config_text = fs::read_to_string(&config).context("failed to read gbandit.jsonc")?;
    let migrations_path = declared_migrations(&config_text)?;

    let migrations = root.join(&migrations_path);
    if !migrations.is_dir() {
        bail!(
            "database.migrations in gbandit.jsonc points at {}, which is not a directory — `gbandit migrate down-to` must run from the project root with the migrations dir present",
            migrations.display()
        );
    }

    let temp = NamedTempFile::new().context("failed to create temporary archive")?;
    let writer = temp
        .reopen()
        .context("failed to reopen temporary archive")?;
    let encoder =
        zstd::stream::Encoder::new(writer, 3).context("failed to initialise zstd encoder")?;
    let mut tar = Builder::new(encoder);

    let mut config_file = fs::File::open(&config)?;
    tar.append_file("gbandit.jsonc", &mut config_file)?;

    let walker = WalkBuilder::new(&migrations)
        .standard_filters(true)
        .hidden(false)
        .build();
    for entry in walker {
        let entry = entry.context("failed to walk the migrations directory")?;
        let path = entry.path();
        if path == migrations {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let relative = path
            .strip_prefix(&migrations)
            .with_context(|| format!("failed to strip archive root for {}", path.display()))?;
        let archive_path = PathBuf::from("migrations").join(relative);
        if file_type.is_dir() {
            tar.append_dir(&archive_path, path)?;
        } else if file_type.is_file() {
            let mut file = fs::File::open(path)?;
            tar.append_file(&archive_path, &mut file)?;
        }
    }

    let encoder = tar.into_inner().context("failed to finalize tar archive")?;
    let mut file = encoder.finish().context("failed to finish zstd archive")?;
    file.flush().context("failed to flush archive")?;
    Ok(temp)
}

pub(crate) fn build_component_archive(component: &str) -> Result<NamedTempFile> {
    build_component_archive_in(component, Path::new("."))
}

fn build_component_archive_in(component: &str, base: &Path) -> Result<NamedTempFile> {
    // For "project" the archive root is the cwd, with paths preserved
    // as-is (frontend/..., backend/...). For component subtrees the
    // archive must preserve the component's path so the executor's
    // extraction lands files at the expected workspace location
    // (e.g. backend/migrations/0001_init.up.sql).
    let (root, archive_prefix) = match component {
        "project" => (base.to_path_buf(), PathBuf::new()),
        other => (base.join(other), PathBuf::from(other)),
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

    // gbandit.jsonc goes first so the server's config peek stops at the first
    // entry instead of decompressing the whole archive to find it.
    let config = root.join("gbandit.jsonc");
    if config.is_file() {
        let mut config_file = fs::File::open(&config)?;
        tar.append_file(archive_prefix.join("gbandit.jsonc"), &mut config_file)?;
    }

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
        if path == root || path == config {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };

        let relative = path
            .strip_prefix(&root)
            .with_context(|| format!("failed to strip archive root for {}", path.display()))?;
        let archive_path = archive_prefix.join(relative);

        if file_type.is_dir() {
            tar.append_dir(&archive_path, path)?;
        } else if file_type.is_file() {
            let mut file = fs::File::open(path)?;
            tar.append_file(&archive_path, &mut file)?;
        }
    }

    let encoder = tar.into_inner().context("failed to finalize tar archive")?;
    let mut file = encoder.finish().context("failed to finish zstd archive")?;
    file.flush().context("failed to flush archive")?;

    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::{build_component_archive_in, build_migrate_down_archive_in};

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

        let archive = build_component_archive_in("project", dir.path()).unwrap();
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

    #[test]
    fn migrate_down_archive_places_migrations_at_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gbandit.jsonc"),
            r#"{
                // migrations live in a non-conventional directory
                "project": "p",
                "database": { "engine": "postgres", "migrations": "db/schema" },
                "backend": { "dockerfile": "server/Dockerfile", "context": "server" },
            }"#,
        )
        .unwrap();
        let migrations = dir.path().join("db/schema");
        std::fs::create_dir_all(&migrations).unwrap();
        std::fs::write(
            migrations.join("0001_init.up.sql"),
            "create table t(i int);",
        )
        .unwrap();

        let archive = build_migrate_down_archive_in(dir.path()).unwrap();
        let entries = archive_entries(&archive);
        assert!(
            entries.contains(&"gbandit.jsonc".to_string()),
            "{entries:?}"
        );
        assert!(
            entries.contains(&"migrations/0001_init.up.sql".to_string()),
            "{entries:?}"
        );
    }

    #[test]
    fn migrate_down_archive_requires_declared_migrations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gbandit.jsonc"),
            r#"{ "project": "p", "database": { "engine": "postgres" },
                 "backend": { "dockerfile": "b/Dockerfile", "context": "b" } }"#,
        )
        .unwrap();

        let err = build_migrate_down_archive_in(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not declare database.migrations"),
            "{err:#}"
        );
    }
}
