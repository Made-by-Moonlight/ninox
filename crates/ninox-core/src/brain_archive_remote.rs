//! Non-local destinations/sources for `ninox brain export`/`import`.
//!
//! `brain_archive` only knows how to build/extract a `.tar.gz` on the local
//! filesystem. This module adds a thin dispatch layer on top: parse the
//! destination/source string into an [`ArchiveLocation`], and for anything
//! that isn't a local path, stage the archive through a local temp file and
//! move it the rest of the way with S3 (plain object get/put) or a GitHub
//! wiki (the `<repo>.wiki.git` remote, pushed/pulled with `git`). This is
//! unrelated to `brain_sync`'s continuous manifest/CAS sync of individual
//! markdown files — this is a one-shot archive hop.

use crate::brain_archive::{export, import, ExportStats, ImportStats};
use crate::github;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::{tempdir, NamedTempFile};

/// File name the archive is stored under inside the wiki repo.
const WIKI_ARCHIVE_FILE_NAME: &str = "brain.tar.gz";

/// Where a `ninox brain export`/`import` destination or source points.
///
/// * A local path — existing behavior, unchanged.
/// * `s3://<bucket>/<key>` — a single object in S3.
/// * `wiki:<owner>/<repo>` — the GitHub wiki for `owner/repo` (the
///   `<repo>.wiki.git` remote), since a wiki has no natural URI form of its
///   own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveLocation {
    Local(PathBuf),
    S3 { bucket: String, key: String },
    Wiki { owner: String, repo: String },
}

impl ArchiveLocation {
    pub fn parse(raw: &Path) -> Result<Self> {
        let s = raw.to_string_lossy();
        if let Some(rest) = s.strip_prefix("s3://") {
            let Some((bucket, key)) = rest.split_once('/') else {
                bail!("s3 location {s:?} must include a key, e.g. s3://bucket/key");
            };
            if bucket.is_empty() || key.is_empty() {
                bail!("s3 location {s:?} must include both a bucket and a key, e.g. s3://bucket/key");
            }
            return Ok(Self::S3 { bucket: bucket.to_string(), key: key.to_string() });
        }
        if let Some(rest) = s.strip_prefix("wiki:") {
            let Some((owner, repo)) = rest.split_once('/') else {
                bail!("wiki location {s:?} must be wiki:<owner>/<repo>");
            };
            if owner.is_empty() || repo.is_empty() {
                bail!("wiki location {s:?} must be wiki:<owner>/<repo>");
            }
            return Ok(Self::Wiki { owner: owner.to_string(), repo: repo.to_string() });
        }
        Ok(Self::Local(raw.to_path_buf()))
    }
}

// ---------------------------------------------------------------------------
// S3 — a plain single-object get/put, behind a trait so tests never touch
// real AWS (same approach as `brain_sync::store::RemoteStore`, minus the
// conditional-request machinery this doesn't need).
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ArchiveBlobStore: Send + Sync {
    async fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>>;
    async fn put(&self, bucket: &str, key: &str, bytes: Vec<u8>) -> Result<()>;
}

/// The real S3 object store, built from the default AWS credential chain —
/// same construction pattern as `brain_sync::s3::S3RemoteStore`.
pub struct S3ArchiveStore {
    client: aws_sdk_s3::Client,
}

impl S3ArchiveStore {
    pub async fn new() -> Result<Self> {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest()).load().await;
        Ok(Self { client: aws_sdk_s3::Client::new(&sdk_config) })
    }
}

#[async_trait]
impl ArchiveBlobStore for S3ArchiveStore {
    async fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>> {
        let out = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("get s3://{bucket}/{key}"))?;
        Ok(out.body.collect().await.context("read object body")?.into_bytes().to_vec())
    }

    async fn put(&self, bucket: &str, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(bytes.into())
            .send()
            .await
            .with_context(|| format!("put s3://{bucket}/{key}"))?;
        Ok(())
    }
}

/// Build the archive locally, then upload it. Generic over [`ArchiveBlobStore`]
/// so it's testable with a fake store, no network required.
pub async fn export_to_blob_store(
    brain_path: &Path,
    store: &dyn ArchiveBlobStore,
    bucket: &str,
    key: &str,
) -> Result<ExportStats> {
    let tmp = NamedTempFile::new().context("create temp file for archive")?;
    let stats = export(brain_path, tmp.path())?;
    let bytes = fs::read(tmp.path()).context("read temp archive")?;
    store.put(bucket, key, bytes).await?;
    Ok(stats)
}

/// Download the archive, then run the existing local `import` on it.
pub async fn import_from_blob_store(
    store: &dyn ArchiveBlobStore,
    bucket: &str,
    key: &str,
    target_brain_path: &Path,
    force: bool,
) -> Result<ImportStats> {
    let bytes = store.get(bucket, key).await?;
    let tmp = NamedTempFile::new().context("create temp file for archive")?;
    fs::write(tmp.path(), &bytes).context("write downloaded archive")?;
    import(tmp.path(), target_brain_path, force)
}

// ---------------------------------------------------------------------------
// GitHub wiki — there's no API for wikis, so push/pull by shelling out to
// `git` against the `<repo>.wiki.git` remote. The remote URL (with the token
// embedded) is kept separate from the git plumbing below it, so tests can
// exercise the plumbing against a local bare repo standing in for the
// remote, with no network or GitHub token involved.
// ---------------------------------------------------------------------------

/// A `<repo>.wiki.git` HTTPS remote URL with the token embedded for git's
/// basic-auth-over-HTTPS convention. Never log this value — it carries a
/// live credential.
pub fn wiki_remote_url(owner: &str, repo: &str, token: &str) -> String {
    format!("https://x-access-token:{token}@github.com/{owner}/{repo}.wiki.git")
}

/// Run `git` in `dir`, returning its stderr (with `redact`, if given, blanked
/// out — git echoes a failing remote URL verbatim in some errors, and that
/// URL may carry a credential) as the error on a non-zero exit.
fn run_git(dir: &Path, args: &[&str], redact: Option<&str>) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("spawn git {args:?}"))?;
    if !output.status.success() {
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let mut args_display = format!("{args:?}");
        if let Some(secret) = redact {
            if !secret.is_empty() {
                stderr = stderr.replace(secret, "***");
                args_display = args_display.replace(secret, "***");
            }
        }
        bail!("git {args_display} failed: {}", stderr.trim());
    }
    Ok(())
}

/// Like [`run_git`], but returns trimmed stdout on success instead of `()`.
fn run_git_capture(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("spawn git {args:?}"))?;
    if !output.status.success() {
        bail!("git {args:?} failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Clone `remote_url`, write `archive_file` into it as `archive_name`, and
/// push.
///
/// Unlike a normal repo, a wiki has no git repo to clone at all until its
/// first page has been created through the GitHub web UI — there's no
/// git-only way to bootstrap one (pushing to a nonexistent wiki remote fails
/// the same way the clone does), so a clone failure here is surfaced as an
/// actionable error rather than papered over.
fn push_archive_to_wiki_repo(
    remote_url: &str,
    redact: &str,
    archive_file: &Path,
    archive_name: &str,
    commit_message: &str,
) -> Result<()> {
    let work_dir = tempdir().context("create temp dir for wiki clone")?;
    let repo_dir = work_dir.path().join("wiki");

    let clone_output = Command::new("git")
        .args(["clone", "--quiet", remote_url, &repo_dir.to_string_lossy()])
        .output()
        .context("spawn git clone")?;
    if !clone_output.status.success() {
        let mut stderr = String::from_utf8_lossy(&clone_output.stderr).into_owned();
        if !redact.is_empty() {
            stderr = stderr.replace(redact, "***");
        }
        bail!(
            "failed to clone wiki repo (has its first page been created via the GitHub web UI yet? a wiki has no git repo until then): {}",
            stderr.trim()
        );
    }

    // Local-only identity — never touches global git config.
    run_git(&repo_dir, &["config", "user.email", "ninox@localhost"], None)?;
    run_git(&repo_dir, &["config", "user.name", "ninox"], None)?;

    fs::copy(archive_file, repo_dir.join(archive_name))
        .with_context(|| format!("copy archive into wiki checkout as {archive_name}"))?;
    run_git(&repo_dir, &["add", archive_name], None)?;

    let nothing_staged = Command::new("git")
        .arg("-C")
        .arg(&repo_dir)
        .args(["diff", "--cached", "--quiet"])
        .status()
        .context("spawn git diff")?
        .success();
    if !nothing_staged {
        run_git(&repo_dir, &["commit", "--quiet", "-m", commit_message], None)?;
    }

    // Whatever branch HEAD already is (set by the clone, or by `init` above)
    // — never assumed to be "master"/"main", only defaulted to "master" if
    // it can't be read at all.
    let branch = run_git_capture(&repo_dir, &["symbolic-ref", "--short", "HEAD"])
        .unwrap_or_else(|_| "master".to_string());
    run_git(&repo_dir, &["push", "--quiet", "origin", &format!("HEAD:refs/heads/{branch}")], Some(redact))?;
    Ok(())
}

/// Clone `remote_url` and return the path to `archive_name` inside it.
fn pull_archive_from_wiki_repo(
    remote_url: &str,
    redact: &str,
    archive_name: &str,
    dest_dir: &Path,
) -> Result<PathBuf> {
    let repo_dir = dest_dir.join("wiki");
    let output = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", remote_url, &repo_dir.to_string_lossy()])
        .output()
        .context("spawn git clone")?;
    if !output.status.success() {
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !redact.is_empty() {
            stderr = stderr.replace(redact, "***");
        }
        bail!("failed to clone wiki repo: {}", stderr.trim());
    }
    let archive_path = repo_dir.join(archive_name);
    if !archive_path.is_file() {
        bail!(
            "wiki repo has no {archive_name} — has `ninox brain export wiki:<owner>/<repo>` ever been run for it?"
        );
    }
    Ok(archive_path)
}

/// Build the archive locally, then push it into the wiki repo at
/// `remote_url`. `redact` (the token, if any is embedded in `remote_url`) is
/// scrubbed from any error message.
pub fn export_to_wiki_remote(brain_path: &Path, remote_url: &str, redact: &str) -> Result<ExportStats> {
    let tmp = NamedTempFile::new().context("create temp file for archive")?;
    let stats = export(brain_path, tmp.path())?;
    push_archive_to_wiki_repo(remote_url, redact, tmp.path(), WIKI_ARCHIVE_FILE_NAME, "Update brain export")?;
    Ok(stats)
}

/// Pull the archive from the wiki repo at `remote_url`, then run the
/// existing local `import` on it.
pub fn import_from_wiki_remote(
    remote_url: &str,
    redact: &str,
    target_brain_path: &Path,
    force: bool,
) -> Result<ImportStats> {
    let work_dir = tempdir().context("create temp dir for wiki clone")?;
    let archive_path = pull_archive_from_wiki_repo(remote_url, redact, WIKI_ARCHIVE_FILE_NAME, work_dir.path())?;
    import(&archive_path, target_brain_path, force)
}

// ---------------------------------------------------------------------------
// Top-level dispatch — what `ninox-app`'s CLI calls.
// ---------------------------------------------------------------------------

/// Export `brain_path` to `destination` — a local path, `s3://bucket/key`,
/// or `wiki:owner/repo`. `github_token` is the config-provided token, if
/// any (falls back through `github::resolve_token`).
pub async fn export_to(
    brain_path: &Path,
    destination: &Path,
    github_token: Option<String>,
) -> Result<ExportStats> {
    match ArchiveLocation::parse(destination)? {
        ArchiveLocation::Local(path) => export(brain_path, &path),
        ArchiveLocation::S3 { bucket, key } => {
            let store = S3ArchiveStore::new().await?;
            export_to_blob_store(brain_path, &store, &bucket, &key).await
        }
        ArchiveLocation::Wiki { owner, repo } => {
            let token = github::resolve_token(github_token).context(
                "no GitHub token available for a wiki destination — set github_token in ninox's config, GITHUB_TOKEN, or run `gh auth login`",
            )?;
            let remote_url = wiki_remote_url(&owner, &repo, &token);
            export_to_wiki_remote(brain_path, &remote_url, &token)
        }
    }
}

/// Import into `target_brain_path` from `source` — a local path,
/// `s3://bucket/key`, or `wiki:owner/repo`.
pub async fn import_from(
    source: &Path,
    target_brain_path: &Path,
    force: bool,
    github_token: Option<String>,
) -> Result<ImportStats> {
    match ArchiveLocation::parse(source)? {
        ArchiveLocation::Local(path) => import(&path, target_brain_path, force),
        ArchiveLocation::S3 { bucket, key } => {
            let store = S3ArchiveStore::new().await?;
            import_from_blob_store(&store, &bucket, &key, target_brain_path, force).await
        }
        ArchiveLocation::Wiki { owner, repo } => {
            let token = github::resolve_token(github_token).context(
                "no GitHub token available for a wiki source — set github_token in ninox's config, GITHUB_TOKEN, or run `gh auth login`",
            )?;
            let remote_url = wiki_remote_url(&owner, &repo, &token);
            import_from_wiki_remote(&remote_url, &token, target_brain_path, force)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // -----------------------------------------------------------------
    // ArchiveLocation::parse
    // -----------------------------------------------------------------

    #[test]
    fn parse_local_path_when_no_scheme_matches() {
        let loc = ArchiveLocation::parse(Path::new("brain.tar.gz")).unwrap();
        assert_eq!(loc, ArchiveLocation::Local(PathBuf::from("brain.tar.gz")));

        let loc = ArchiveLocation::parse(Path::new("/abs/path/brain.tar.gz")).unwrap();
        assert_eq!(loc, ArchiveLocation::Local(PathBuf::from("/abs/path/brain.tar.gz")));
    }

    #[test]
    fn parse_s3_uri() {
        let loc = ArchiveLocation::parse(Path::new("s3://my-bucket/path/to/brain.tar.gz")).unwrap();
        assert_eq!(
            loc,
            ArchiveLocation::S3 { bucket: "my-bucket".to_string(), key: "path/to/brain.tar.gz".to_string() }
        );
    }

    #[test]
    fn parse_s3_uri_without_key_is_an_error() {
        let err = ArchiveLocation::parse(Path::new("s3://my-bucket")).unwrap_err();
        assert!(err.to_string().contains("must include a key"), "{err}");
    }

    #[test]
    fn parse_wiki_uri() {
        let loc = ArchiveLocation::parse(Path::new("wiki:Made-by-Moonlight/ninox")).unwrap();
        assert_eq!(
            loc,
            ArchiveLocation::Wiki { owner: "Made-by-Moonlight".to_string(), repo: "ninox".to_string() }
        );
    }

    #[test]
    fn parse_wiki_uri_without_repo_is_an_error() {
        let err = ArchiveLocation::parse(Path::new("wiki:just-an-owner")).unwrap_err();
        assert!(err.to_string().contains("wiki:<owner>/<repo>"), "{err}");
    }

    #[test]
    fn wiki_remote_url_embeds_token_as_basic_auth() {
        let url = wiki_remote_url("acme", "widgets", "ghs_secrettoken");
        assert_eq!(url, "https://x-access-token:ghs_secrettoken@github.com/acme/widgets.wiki.git");
    }

    // -----------------------------------------------------------------
    // S3 path — exercised against an in-memory fake, no network.
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct FakeBlobStore {
        objects: Mutex<std::collections::HashMap<(String, String), Vec<u8>>>,
    }

    #[async_trait]
    impl ArchiveBlobStore for FakeBlobStore {
        async fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>> {
            self.objects
                .lock()
                .unwrap()
                .get(&(bucket.to_string(), key.to_string()))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no such object s3://{bucket}/{key}"))
        }

        async fn put(&self, bucket: &str, key: &str, bytes: Vec<u8>) -> Result<()> {
            self.objects.lock().unwrap().insert((bucket.to_string(), key.to_string()), bytes);
            Ok(())
        }
    }

    #[tokio::test]
    async fn export_then_import_round_trips_through_blob_store() {
        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("repos")).unwrap();
        fs::write(source_dir.path().join("repos/ninox.md"), "---\nname: ninox\n---\nBody.").unwrap();

        let store = FakeBlobStore::default();
        let export_stats = export_to_blob_store(source_dir.path(), &store, "my-bucket", "brains/main.tar.gz")
            .await
            .unwrap();
        assert_eq!(export_stats.files, 1);

        let target_dir = tempdir().unwrap();
        let import_stats =
            import_from_blob_store(&store, "my-bucket", "brains/main.tar.gz", target_dir.path(), false)
                .await
                .unwrap();
        assert_eq!(import_stats.imported, 1);
        assert!(target_dir.path().join("repos/ninox.md").exists());
    }

    #[tokio::test]
    async fn import_from_blob_store_surfaces_missing_object() {
        let store = FakeBlobStore::default();
        let target_dir = tempdir().unwrap();
        let err = import_from_blob_store(&store, "my-bucket", "nope.tar.gz", target_dir.path(), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such object"), "{err}");
    }

    // -----------------------------------------------------------------
    // Wiki path — exercised against a local bare git repo standing in for
    // the `<repo>.wiki.git` remote, no network or GitHub token involved.
    // -----------------------------------------------------------------

    fn init_bare_repo(dir: &Path) {
        let status = Command::new("git").args(["init", "--quiet", "--bare"]).arg(dir).status().unwrap();
        assert!(status.success());
    }

    #[test]
    fn export_then_import_round_trips_through_a_wiki_remote() {
        let bare = tempdir().unwrap();
        init_bare_repo(bare.path());
        let remote_url = bare.path().to_string_lossy().into_owned();

        let source_dir = tempdir().unwrap();
        fs::create_dir_all(source_dir.path().join("repos")).unwrap();
        fs::write(source_dir.path().join("repos/ninox.md"), "---\nname: ninox\n---\nBody.").unwrap();

        let export_stats = export_to_wiki_remote(source_dir.path(), &remote_url, "").unwrap();
        assert_eq!(export_stats.files, 1);

        let target_dir = tempdir().unwrap();
        let import_stats = import_from_wiki_remote(&remote_url, "", target_dir.path(), false).unwrap();
        assert_eq!(import_stats.imported, 1);
        assert!(target_dir.path().join("repos/ninox.md").exists());
    }

    #[test]
    fn export_to_wiki_remote_errors_when_remote_does_not_exist() {
        // A path that doesn't exist at all stands in for "the wiki has
        // never had a page created via the GitHub web UI, so there's no
        // git repo to push to yet" — there's no git-only way to bootstrap
        // one, so this must fail with a clear, actionable error.
        let missing = tempdir().unwrap();
        let never_created = missing.path().join("does-not-exist.git");

        let source_dir = tempdir().unwrap();
        fs::write(source_dir.path().join("note.md"), "# Note").unwrap();

        let err = export_to_wiki_remote(source_dir.path(), &never_created.to_string_lossy(), "").unwrap_err();
        assert!(err.to_string().contains("failed to clone wiki repo"), "{err}");
    }

    #[test]
    fn export_to_wiki_remote_is_idempotent_when_content_is_unchanged() {
        let bare = tempdir().unwrap();
        init_bare_repo(bare.path());
        let remote_url = bare.path().to_string_lossy().into_owned();

        let source_dir = tempdir().unwrap();
        fs::write(source_dir.path().join("note.md"), "# Note").unwrap();

        export_to_wiki_remote(source_dir.path(), &remote_url, "").unwrap();
        // A second export of the same brain content must not fail (no
        // staged changes to commit the second time around).
        export_to_wiki_remote(source_dir.path(), &remote_url, "").unwrap();
    }

    #[test]
    fn import_from_wiki_remote_errors_when_archive_was_never_pushed() {
        let bare = tempdir().unwrap();
        init_bare_repo(bare.path());
        let remote_url = bare.path().to_string_lossy().into_owned();

        let target_dir = tempdir().unwrap();
        let err = import_from_wiki_remote(&remote_url, "", target_dir.path(), false).unwrap_err();
        assert!(err.to_string().contains("has no brain.tar.gz"), "{err}");
    }

    #[test]
    fn import_from_wiki_remote_errors_when_remote_does_not_exist() {
        let missing = tempdir().unwrap();
        let never_created = missing.path().join("does-not-exist.git");
        let target_dir = tempdir().unwrap();
        let err = import_from_wiki_remote(&never_created.to_string_lossy(), "", target_dir.path(), false)
            .unwrap_err();
        assert!(err.to_string().contains("failed to clone"), "{err}");
    }

    #[test]
    fn run_git_redacts_the_secret_from_a_failing_commands_error() {
        let dir = tempdir().unwrap();
        let secret = "super-secret-token";
        // `git clone` of a nonexistent local path fails with an error that
        // echoes the path back verbatim — standing in for a failing
        // clone/push against a GitHub URL echoing back an embedded token.
        let bogus_source = dir.path().join(format!("does-not-exist-{secret}"));
        let err = run_git(
            dir.path(),
            &["clone", "--quiet", &bogus_source.to_string_lossy(), "dest"],
            Some(secret),
        )
        .unwrap_err();
        assert!(!err.to_string().contains(secret), "token leaked into error: {err}");
    }
}
