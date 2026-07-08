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
    /// Relative paths that failed to extract (e.g. a `--force` overwrite of
    /// a file by a same-named directory, or vice versa) along with the
    /// error. Recorded rather than aborting so one bad entry doesn't lose
    /// the rest of an otherwise-good import.
    pub failed: Vec<(PathBuf, String)>,
}

/// Package every Markdown file under `brain_path` into a gzipped tarball at
/// `output_path`, preserving relative paths (e.g. `repos/ninox.md`).
///
/// Creates `brain_path` if it doesn't exist yet (mirrors `BrainIndex::open`),
/// so exporting a brain that has never been indexed produces an empty
/// archive instead of a raw "no such file or directory" error.
pub fn export(brain_path: &Path, output_path: &Path) -> Result<ExportStats> {
    fs::create_dir_all(brain_path).with_context(|| format!("create brain dir {brain_path:?}"))?;

    let out_file = fs::File::create(output_path)
        .with_context(|| format!("create archive {output_path:?}"))?;
    let mut builder = tar::Builder::new(GzEncoder::new(out_file, Compression::default()));
    // Never dereference a symlink into the archive as a regular file's
    // content — matches `WalkDir::follow_links(false)` below in intent, and
    // means `import` can safely refuse every symlink entry it sees without
    // ever rejecting a legitimate export.
    builder.follow_symlinks(false);

    let mut files = 0usize;
    for entry in WalkDir::new(brain_path).follow_links(false).sort_by_file_name() {
        let entry = entry.with_context(|| format!("walk brain dir {brain_path:?}"))?;
        let path = entry.path();
        if entry.file_type().is_symlink()
            || !path.is_file()
            || path.extension().and_then(|e| e.to_str()) != Some("md")
        {
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

/// Reject an entry whose path or type could let extraction escape
/// `target_brain_path` — required because `Entry::unpack`, unlike
/// `Entry::unpack_in`, does not guard against this itself. An absolute path
/// or a `..` component could point anywhere on disk once joined onto the
/// target dir, and a symlink/hard-link entry could redirect a *later*
/// same-named entry through it.
fn check_entry_is_safe(rel_path: &Path, entry_type: tar::EntryType) -> Result<()> {
    if rel_path.is_absolute()
        || rel_path.components().any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("archive entry {rel_path:?} has an unsafe path");
    }
    if entry_type.is_symlink() || entry_type.is_hard_link() {
        bail!("archive entry {rel_path:?} is a symlink/hard link, refusing to extract it");
    }
    Ok(())
}

fn open_archive(archive_path: &Path) -> Result<tar::Archive<GzDecoder<fs::File>>> {
    let in_file =
        fs::File::open(archive_path).with_context(|| format!("open archive {archive_path:?}"))?;
    Ok(tar::Archive::new(GzDecoder::new(in_file)))
}

/// Extract a `.tar.gz` produced by [`export`] into `target_brain_path`.
///
/// An entry whose relative path already exists under `target_brain_path` is
/// left untouched (and recorded in [`ImportStats::skipped`]) unless `force`
/// is set, so importing never silently overwrites a teammate's existing
/// notes. Rebuilding the SQLite index afterwards is the caller's
/// responsibility (mirrors `ninox brain index`).
///
/// Runs in two passes: the first validates every entry via
/// [`check_entry_is_safe`] without writing anything, so an archive
/// containing even one unsafe entry (absolute path, `..` traversal, symlink,
/// hard link) is rejected wholesale instead of leaving a half-imported,
/// unreported brain on disk from the entries that came before it. The
/// second pass does the actual extraction; a per-entry failure there (e.g.
/// a `--force` overwrite of a file by a same-named directory) is recorded in
/// [`ImportStats::failed`] rather than aborting, so one bad entry still
/// doesn't lose the rest of an otherwise-good import.
pub fn import(archive_path: &Path, target_brain_path: &Path, force: bool) -> Result<ImportStats> {
    fs::create_dir_all(target_brain_path)
        .with_context(|| format!("create brain dir {target_brain_path:?}"))?;

    let mut validate_archive = open_archive(archive_path)?;
    for entry in validate_archive.entries().context("read archive entries")? {
        let entry = entry.context("read archive entry")?;
        let rel_path = entry.path().context("read entry path")?.to_path_buf();
        check_entry_is_safe(&rel_path, entry.header().entry_type())?;
    }

    let mut archive = open_archive(archive_path)?;
    let mut stats = ImportStats::default();
    for entry in archive.entries().context("read archive entries")? {
        let mut entry = entry.context("read archive entry")?;
        let rel_path = entry.path().context("read entry path")?.to_path_buf();
        let entry_type = entry.header().entry_type();
        check_entry_is_safe(&rel_path, entry_type)?;

        if entry_type.is_dir() {
            // Directories are implicit in the relative file paths we
            // extract; nothing to do for an explicit directory entry.
            continue;
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
        // A single bad entry (e.g. `--force` onto a same-named directory)
        // is recorded rather than aborting the whole import via `?`, so the
        // rest of an otherwise-good archive still gets imported.
        match entry.unpack(&dest) {
            Ok(_) => stats.imported += 1,
            Err(err) => stats.failed.push((rel_path, err.to_string())),
        }
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

    // -----------------------------------------------------------------
    // Adversarial / malformed archives — `export` never produces any of
    // these, so they're hand-built with the low-level `tar` API directly.
    // -----------------------------------------------------------------

    type RawBuilder = tar::Builder<GzEncoder<fs::File>>;

    // Appends an entry by writing the name field's raw bytes directly
    // rather than through `Header::set_path`, which itself refuses absolute
    // paths and `..` components — bypassing it here simulates a
    // hand-crafted malicious archive, which the tar *format* doesn't
    // forbid even though this crate's own writer does.
    fn append_raw_entry(
        builder: &mut RawBuilder,
        entry_path: &str,
        entry_type: tar::EntryType,
        content: &[u8],
        link_target: Option<&str>,
    ) {
        let mut header = tar::Header::new_gnu();
        {
            let name_bytes = entry_path.as_bytes();
            assert!(name_bytes.len() < 100, "test path too long for the name field");
            header.as_mut_bytes()[..name_bytes.len()].copy_from_slice(name_bytes);
        }
        header.set_entry_type(entry_type);
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        if let Some(target) = link_target {
            header.set_link_name(target).unwrap();
        }
        header.set_cksum();
        builder.append(&header, content).unwrap();
    }

    fn write_single_entry_archive(
        archive_path: &Path,
        entry_path: &str,
        entry_type: tar::EntryType,
        content: &[u8],
        link_target: Option<&str>,
    ) {
        let file = fs::File::create(archive_path).unwrap();
        let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_raw_entry(&mut builder, entry_path, entry_type, content, link_target);
        let enc = builder.into_inner().unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn import_rejects_absolute_path_entries() {
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("evil.tar.gz");
        write_single_entry_archive(
            &archive_path,
            "/etc/cron.d/pwn",
            tar::EntryType::Regular,
            b"malicious",
            None,
        );

        let target_dir = tempdir().unwrap();
        let err = import(&archive_path, target_dir.path(), false).unwrap_err();
        assert!(
            err.to_string().contains("unsafe path"),
            "expected an unsafe-path error, got: {err}"
        );
    }

    #[test]
    fn import_rejects_parent_dir_escape_entries() {
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("evil.tar.gz");
        write_single_entry_archive(
            &archive_path,
            "../../evil.md",
            tar::EntryType::Regular,
            b"malicious",
            None,
        );

        let target_dir = tempdir().unwrap();
        let brain_dir = target_dir.path().join("brain");
        let err = import(&archive_path, &brain_dir, false).unwrap_err();
        assert!(
            err.to_string().contains("unsafe path"),
            "expected an unsafe-path error, got: {err}"
        );
        assert!(
            !target_dir.path().join("evil.md").exists(),
            "the entry must never be written outside the target brain dir"
        );
    }

    #[test]
    fn import_rejects_symlink_entries() {
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("evil.tar.gz");
        write_single_entry_archive(
            &archive_path,
            "repos/escape",
            tar::EntryType::Symlink,
            b"",
            Some("/tmp"),
        );

        let target_dir = tempdir().unwrap();
        let err = import(&archive_path, target_dir.path(), false).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected a symlink-rejection error, got: {err}"
        );
        assert!(
            !target_dir.path().join("repos/escape").exists(),
            "no symlink should ever be created by import"
        );
    }

    #[test]
    fn import_skips_explicit_directory_entries_without_error() {
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("dirs.tar.gz");
        write_single_entry_archive(&archive_path, "repos/", tar::EntryType::Directory, b"", None);

        let target_dir = tempdir().unwrap();
        let stats = import(&archive_path, target_dir.path(), false).unwrap();
        assert_eq!(stats.imported, 0);
        assert!(stats.skipped.is_empty());
        assert!(stats.failed.is_empty());
    }

    #[test]
    fn import_rejects_corrupt_archive_without_panicking() {
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("not-a-real-archive.tar.gz");
        fs::write(&archive_path, b"this is not gzip data at all").unwrap();

        let target_dir = tempdir().unwrap();
        let result = import(&archive_path, target_dir.path(), false);
        assert!(result.is_err(), "a corrupt archive must error, not panic");
    }

    #[test]
    fn import_records_a_failed_entry_without_losing_the_rest_of_the_import() {
        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("repos")).unwrap();
        fs::write(source_dir.path().join("repos/a.md"), "A content").unwrap();
        fs::write(source_dir.path().join("repos/b.md"), "B content").unwrap();
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        export(source_dir.path(), &archive_path).unwrap();

        let target_dir = tempdir().unwrap();
        // Force a type mismatch: the target already has a *directory* where
        // the archive wants to write a *file*, so unpacking that one entry
        // will fail even with `force` set.
        fs::create_dir_all(target_dir.path().join("repos/a.md")).unwrap();

        let stats = import(&archive_path, target_dir.path(), true).unwrap();
        assert_eq!(stats.imported, 1, "repos/b.md should still import: {stats:?}");
        assert_eq!(stats.failed.len(), 1);
        assert_eq!(stats.failed[0].0, PathBuf::from("repos/a.md"));
        assert!(target_dir.path().join("repos/b.md").exists());
    }

    #[test]
    fn import_writes_nothing_when_any_entry_fails_the_safety_check() {
        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("mixed.tar.gz");
        {
            let file = fs::File::create(&archive_path).unwrap();
            let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));
            // A perfectly safe entry ordered *before* the unsafe one, so a
            // naive per-entry bail would already have written it to disk.
            append_raw_entry(&mut builder, "repos/a.md", tar::EntryType::Regular, b"safe content", None);
            append_raw_entry(
                &mut builder,
                "/etc/cron.d/pwn",
                tar::EntryType::Regular,
                b"malicious",
                None,
            );
            let enc = builder.into_inner().unwrap();
            enc.finish().unwrap();
        }

        let target_dir = tempdir().unwrap();
        let err = import(&archive_path, target_dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "unexpected error: {err}");
        assert!(
            !target_dir.path().join("repos/a.md").exists(),
            "no entry should be written when the archive contains any unsafe entry, even one ordered earlier"
        );
    }

    #[test]
    fn export_creates_brain_dir_if_missing_and_produces_empty_archive() {
        let parent_dir = tempdir().unwrap();
        let brain_path = parent_dir.path().join("never-indexed-brain");
        assert!(!brain_path.exists());

        let archive_dir = tempdir().unwrap();
        let archive_path = archive_dir.path().join("brain.tar.gz");
        let stats = export(&brain_path, &archive_path).unwrap();

        assert_eq!(stats.files, 0);
        assert!(
            brain_path.exists(),
            "export should create the brain dir on demand, like BrainIndex::open does"
        );
        assert!(archive_path.exists());
    }
}
