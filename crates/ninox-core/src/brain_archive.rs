//! Export/import a brain as a portable `.tar.gz` archive.
//!
//! Packages only the Markdown source files (the human-readable source of
//! truth, see `docs/BRAIN.md`) — never `.index.db`, which is derived and
//! machine/version-specific. A plain gzipped tarball, not a bespoke format,
//! so a teammate without `ninox` can `tar xzf` it by hand if they want.

use anyhow::{bail, Context, Result};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use std::{
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

/// Result of [`export`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportStats {
    /// Markdown files packaged into the archive.
    pub files: usize,
}

/// Result of [`import`].
#[derive(Debug, Clone, Default)]
pub struct ImportStats {
    /// Entries extracted into the target brain.
    pub imported: usize,
    /// Relative paths that already existed in the target brain and were
    /// left untouched because `force` was not set.
    pub skipped: Vec<PathBuf>,
}

/// Package every Markdown file under `brain_path` into a gzipped tarball at
/// `output_path`, preserving relative paths (e.g. `repos/ninox.md`).
pub fn export(brain_path: &Path, output_path: &Path) -> Result<ExportStats> {
    let out_file = fs::File::create(output_path)
        .with_context(|| format!("create archive {output_path:?}"))?;
    let mut builder = tar::Builder::new(GzEncoder::new(out_file, Compression::default()));

    let mut files = 0usize;
    for entry in WalkDir::new(brain_path).follow_links(false).sort_by_file_name() {
        let entry = entry.with_context(|| format!("walk brain dir {brain_path:?}"))?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let rel = path
            .strip_prefix(brain_path)
            .with_context(|| format!("relativize {path:?} against {brain_path:?}"))?;
        builder
            .append_path_with_name(path, rel)
            .with_context(|| format!("add {rel:?} to archive"))?;
        files += 1;
    }

    let enc = builder.into_inner().context("finish tar stream")?;
    enc.finish().context("finish gzip stream")?;
    Ok(ExportStats { files })
}

/// Extract a `.tar.gz` produced by [`export`] into `target_brain_path`.
///
/// An entry whose relative path already exists under `target_brain_path` is
/// left untouched (and recorded in [`ImportStats::skipped`]) unless `force`
/// is set, so importing never silently overwrites a teammate's existing
/// notes. Rebuilding the SQLite index afterwards is the caller's
/// responsibility (mirrors `ninox brain index`).
pub fn import(archive_path: &Path, target_brain_path: &Path, force: bool) -> Result<ImportStats> {
    fs::create_dir_all(target_brain_path)
        .with_context(|| format!("create brain dir {target_brain_path:?}"))?;

    let in_file =
        fs::File::open(archive_path).with_context(|| format!("open archive {archive_path:?}"))?;
    let mut archive = tar::Archive::new(GzDecoder::new(in_file));

    let mut stats = ImportStats::default();
    for entry in archive.entries().context("read archive entries")? {
        let mut entry = entry.context("read archive entry")?;
        let rel_path = entry.path().context("read entry path")?.to_path_buf();

        if rel_path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("archive entry {rel_path:?} escapes the brain directory");
        }

        let dest = target_brain_path.join(&rel_path);
        if dest.exists() && !force {
            stats.skipped.push(rel_path);
            continue;
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {parent:?}"))?;
        }
        entry.unpack(&dest).with_context(|| format!("extract {rel_path:?}"))?;
        stats.imported += 1;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn export_packages_markdown_and_excludes_index_db() {
        let brain_dir = tempdir().unwrap();
        fs::create_dir_all(brain_dir.path().join("repos")).unwrap();
        fs::write(brain_dir.path().join("repos/ninox.md"), "---\nname: ninox\n---\nBody.").unwrap();
        fs::create_dir_all(brain_dir.path().join("symbols")).unwrap();
        fs::write(brain_dir.path().join("symbols/thing.md"), "# Thing").unwrap();
        // Simulate a derived index sitting alongside the source files.
        fs::write(brain_dir.path().join(".index.db"), b"not markdown").unwrap();
        fs::write(brain_dir.path().join(".gitignore"), ".index.db\n").unwrap();

        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        let stats = export(brain_dir.path(), &archive_path).unwrap();
        assert_eq!(stats.files, 2);
        assert!(archive_path.exists());

        let file = fs::File::open(&archive_path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"repos/ninox.md".to_string()));
        assert!(names.contains(&"symbols/thing.md".to_string()));
        assert!(
            !names.iter().any(|n| n.contains(".index.db")),
            "archive must not contain the derived index: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == ".gitignore"),
            "archive should only contain markdown source: {names:?}"
        );
    }

    #[test]
    fn import_extracts_entries_and_rebuild_makes_them_queryable() {
        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("repos")).unwrap();
        fs::write(
            source_dir.path().join("repos/ninox.md"),
            "---\nname: ninox\ntags:\n- rust\n---\nEntry point is main.rs.",
        )
        .unwrap();

        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        export(source_dir.path(), &archive_path).unwrap();

        let target_dir = tempdir().unwrap();
        let stats = import(&archive_path, target_dir.path(), false).unwrap();
        assert_eq!(stats.imported, 1);
        assert!(stats.skipped.is_empty());
        assert!(target_dir.path().join("repos/ninox.md").exists());

        let brain = crate::brain::BrainIndex::open(target_dir.path()).unwrap();
        brain.rebuild(None).unwrap();
        let results = brain
            .query("main.rs", None, crate::brain::QueryFilters::default())
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "repos/ninox.md");
    }

    #[test]
    fn import_skips_conflicting_files_by_default() {
        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("repos")).unwrap();
        fs::write(source_dir.path().join("repos/ninox.md"), "incoming version").unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        export(source_dir.path(), &archive_path).unwrap();

        let target_dir = tempdir().unwrap();
        fs::create_dir_all(target_dir.path().join("repos")).unwrap();
        fs::write(target_dir.path().join("repos/ninox.md"), "teammate's existing notes").unwrap();

        let stats = import(&archive_path, target_dir.path(), false).unwrap();
        assert_eq!(stats.imported, 0);
        assert_eq!(stats.skipped, vec![PathBuf::from("repos/ninox.md")]);
        assert_eq!(
            fs::read_to_string(target_dir.path().join("repos/ninox.md")).unwrap(),
            "teammate's existing notes",
            "conflicting file must not be overwritten without --force"
        );
    }

    #[test]
    fn import_with_force_overwrites_conflicting_files() {
        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("repos")).unwrap();
        fs::write(source_dir.path().join("repos/ninox.md"), "incoming version").unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        export(source_dir.path(), &archive_path).unwrap();

        let target_dir = tempdir().unwrap();
        fs::create_dir_all(target_dir.path().join("repos")).unwrap();
        fs::write(target_dir.path().join("repos/ninox.md"), "teammate's existing notes").unwrap();

        let stats = import(&archive_path, target_dir.path(), true).unwrap();
        assert_eq!(stats.imported, 1);
        assert!(stats.skipped.is_empty());
        assert_eq!(
            fs::read_to_string(target_dir.path().join("repos/ninox.md")).unwrap(),
            "incoming version"
        );
    }

    #[test]
    fn import_creates_target_brain_dir_if_missing() {
        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("concepts")).unwrap();
        fs::write(source_dir.path().join("concepts/x.md"), "content").unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        export(source_dir.path(), &archive_path).unwrap();

        let target_dir = tempdir().unwrap();
        let target_brain_path = target_dir.path().join("nested/brain");
        assert!(!target_brain_path.exists());

        let stats = import(&archive_path, &target_brain_path, false).unwrap();
        assert_eq!(stats.imported, 1);
        assert!(target_brain_path.join("concepts/x.md").exists());
    }
}
