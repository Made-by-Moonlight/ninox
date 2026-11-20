//! `RemoteStore` backed by a persistent local clone of a GitHub wiki repo.
//! See `docs/superpowers/specs/2026-10-21-github-wiki-brain-backend-design.md`.
//!
//! Auth: no token is passed to `git` explicitly. Every shelled-out `git`
//! invocation elsewhere in this codebase is purely local (worktree/commit/
//! branch/status/checkout — never a `clone`/`fetch`/`push` against a GitHub
//! remote), so there is no existing convention to reuse; this relies on the
//! ambient git credential configuration already on the host (SSH agent, a
//! stored HTTPS credential, or `gh`'s git credential helper from `gh auth
//! login`) exactly as a human running `git push` from the same machine would.

use crate::brain_sync::{
    config::SyncToml,
    manifest::sha256_hex,
    store::{GetResponse, PutOutcome, RemoteStore},
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::Mutex};

/// Bounded retries for a `put` (entry) push rejected as non-fast-forward,
/// mirroring `BrainSync::sync`'s own `MAX_ATTEMPTS` convention (mod.rs).
const MAX_PUSH_ATTEMPTS: u32 = 5;

/// Skip re-`fetch`ing the clone if the last fetch was more recent than
/// this — `pull_if_stale`/`sync` call `get` once per manifest check plus
/// once per pulled entry, and refetching the (small) wiki repo for every
/// one of those within a single sync pass is wasted network chatter.
const FETCH_DEBOUNCE: Duration = Duration::from_secs(1);

/// A held cross-process lock older than this is assumed to belong to a
/// crashed process rather than a slow one, and is reclaimed instead of
/// blocking forever.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5 * 60);
/// How long to wait for a live holder before giving up.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct GithubWikiRemoteStore {
    clone_url: String,
    clone_dir: PathBuf,
    /// Cheap same-process serialization; the cross-process lock acquired
    /// inside each trait method (see `acquire_process_lock`) is what
    /// actually protects `clone_dir` itself, since two separate `ninox`
    /// processes resolve to the identical deterministic cache path for the
    /// same catalogue (`clone_dir_for`) and would otherwise race on the
    /// same working tree with no coordination at all.
    lock: Mutex<()>,
    last_fetch: Mutex<Option<Instant>>,
    /// Test-only seam: a one-shot future awaited right before `git push`,
    /// so a test can simulate another writer landing a commit on `origin`
    /// in the exact window that makes a push rejection possible — see
    /// `write_commit_push_inner`.
    #[cfg(test)]
    race_hook: Mutex<Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>>,
}

/// Released by removing the lock directory on drop. A plain `mkdir` is
/// atomic on every platform this backend targets, so this needs no new
/// crate dependency the way a real `flock` binding would.
struct ProcessLockGuard {
    path: PathBuf,
}

impl Drop for ProcessLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

enum PushOutcome {
    Pushed,
    Rejected,
}

impl GithubWikiRemoteStore {
    /// `cfg.remote` is `https://github.com/{owner}/{repo}/wiki`.
    pub async fn from_config(cfg: &SyncToml) -> Result<Self> {
        let (owner, repo) = parse_wiki_remote(&cfg.remote)?;
        Self::for_clone_url(wiki_clone_url(&owner, &repo), clone_dir_for(&owner, &repo)).await
    }

    /// Test seam: `from_config` resolves the real GitHub clone URL and
    /// cache-dir path, then delegates here. Tests call this directly with a
    /// local bare-repo fixture's `file://` (or plain path) URL.
    pub async fn for_clone_url(clone_url: String, clone_dir: PathBuf) -> Result<Self> {
        let store = Self {
            clone_url,
            clone_dir,
            lock: Mutex::new(()),
            last_fetch: Mutex::new(None),
            #[cfg(test)]
            race_hook: Mutex::new(None),
        };
        {
            let _process_guard = store.acquire_process_lock().await?;
            store.ensure_cloned().await?;
        }
        Ok(store)
    }

    fn lock_dir(&self) -> PathBuf {
        self.clone_dir.with_file_name(format!(
            "{}.lock",
            self.clone_dir.file_name().and_then(|n| n.to_str()).unwrap_or("brain-wiki")
        ))
    }

    /// Cross-process mutual exclusion over `clone_dir`: blocks (polling)
    /// until no other process holds the lock, reclaiming it unconditionally
    /// if it's older than `LOCK_STALE_AFTER` (its holder presumably
    /// crashed), and gives up after `LOCK_WAIT_TIMEOUT` of real contention.
    async fn acquire_process_lock(&self) -> Result<ProcessLockGuard> {
        let lock_dir = self.lock_dir();
        if let Some(parent) = lock_dir.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| format!("create {parent:?}"))?;
        }
        let deadline = Instant::now() + LOCK_WAIT_TIMEOUT;
        loop {
            match tokio::fs::create_dir(&lock_dir).await {
                Ok(()) => return Ok(ProcessLockGuard { path: lock_dir }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Ok(meta) = tokio::fs::metadata(&lock_dir).await {
                        if meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > LOCK_STALE_AFTER) {
                            let _ = tokio::fs::remove_dir_all(&lock_dir).await;
                            continue;
                        }
                    }
                    if Instant::now() >= deadline {
                        bail!(
                            "timed out waiting {LOCK_WAIT_TIMEOUT:?} for the github-wiki local-clone lock at {lock_dir:?} \
                             (held by another ninox process)"
                        );
                    }
                    tokio::time::sleep(LOCK_POLL_INTERVAL).await;
                }
                Err(e) => return Err(e).with_context(|| format!("create lock dir {lock_dir:?}")),
            }
        }
    }

    #[cfg(test)]
    async fn set_race_hook(&self, hook: impl std::future::Future<Output = ()> + Send + 'static) {
        *self.race_hook.lock().await = Some(Box::pin(hook));
    }

    #[cfg(test)]
    async fn test_before_push(&self) {
        let hook = self.race_hook.lock().await.take();
        if let Some(hook) = hook {
            hook.await;
        }
    }

    async fn ensure_cloned(&self) -> Result<()> {
        if self.clone_dir.join(".git").exists() {
            return Ok(());
        }
        if let Some(parent) = self.clone_dir.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| format!("create {parent:?}"))?;
        }
        let out = Command::new("git")
            .args(["clone", "--quiet", &self.clone_url])
            .arg(&self.clone_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .context("spawn git clone")?;
        if out.status.success() {
            return Ok(());
        }
        // The wiki has never had a page created (or the fixture bare repo is
        // empty and unreachable by `clone` the normal way): initialize a
        // fresh local repo instead of failing. The first `put`/`put_if_match`
        // creates the wiki on its first push.
        tracing::warn!(
            "github wiki clone of {} failed ({}); initializing a fresh local repo instead",
            self.clone_url,
            String::from_utf8_lossy(&out.stderr).trim(),
        );
        tokio::fs::create_dir_all(&self.clone_dir).await?;
        run_git(&self.clone_dir, &["init", "--quiet", "--initial-branch", "master"]).await?;
        run_git(&self.clone_dir, &["remote", "add", "origin", &self.clone_url]).await?;
        Ok(())
    }

    /// `git symbolic-ref --short HEAD` — works even for an unborn branch
    /// (no commits yet), since it reflects the ref name, not its history.
    async fn branch(&self) -> Result<String> {
        let out = run_git(&self.clone_dir, &["symbolic-ref", "--short", "HEAD"]).await?;
        Ok(String::from_utf8(out.stdout).context("non-utf8 branch name")?.trim().to_string())
    }

    /// `-c user.name=.../-c user.email=...` overrides for whichever of
    /// those `git commit` needs — only when the ambient git config (this
    /// clone's own, or the host's global/system config) doesn't already
    /// supply it. A host actually used to author real wiki pages will have
    /// a real git identity configured already (the same one a human
    /// pushing from it would get); this only kicks in on a bare host with
    /// no identity configured at all, e.g. a CI runner, where `git commit`
    /// would otherwise fail outright with "Please tell me who you are".
    async fn commit_identity_fallback(&self) -> Vec<String> {
        let configured = |key: &'static str| {
            let dir = self.clone_dir.clone();
            async move { git_output(&dir, &["config", key]).await.is_ok_and(|o| o.status.success()) }
        };
        let mut overrides = Vec::new();
        if !configured("user.name").await {
            overrides.extend(["-c".to_string(), "user.name=ninox".to_string()]);
        }
        if !configured("user.email").await {
            overrides.extend(["-c".to_string(), "user.email=ninox@localhost".to_string()]);
        }
        overrides
    }

    async fn fetch(&self) -> Result<()> {
        self.ensure_cloned().await?;
        let mut last = self.last_fetch.lock().await;
        if let Some(t) = *last {
            if t.elapsed() < FETCH_DEBOUNCE {
                return Ok(());
            }
        }
        let branch = self.branch().await?;
        let out = git_output(&self.clone_dir, &["fetch", "--quiet", "origin", &branch]).await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
            // Two "there's nothing there yet" cases that aren't failures:
            // the remote branch doesn't exist (wiki has commits but not on
            // this branch — shouldn't normally happen, but harmless), or the
            // whole remote repo doesn't exist yet (brand-new wiki that has
            // never been pushed to, including this backend's own local
            // `git init` fallback in `ensure_cloned`).
            let remote_has_nothing_yet = stderr.contains("couldn't find remote ref")
                || stderr.contains("does not appear to be a git repository")
                || stderr.contains("repository not found")
                || stderr.contains("could not read from remote repository");
            if !remote_has_nothing_yet {
                bail!("git fetch {} {branch} failed: {stderr}", self.clone_url);
            }
        }
        *last = Some(Instant::now());
        Ok(())
    }

    /// Read `key`'s bytes from the fetched remote-tracking branch, without
    /// touching the working tree (so a concurrent write-in-progress on this
    /// same instance — serialized by `self.lock` anyway — can't interact
    /// with a read).
    async fn show_remote_file(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.fetch().await?;
        let branch = self.branch().await?;
        let spec = format!("origin/{branch}:{key}");
        let out = git_output(&self.clone_dir, &["show", &spec]).await?;
        if out.status.success() {
            return Ok(Some(out.stdout));
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("does not exist")
            || stderr.contains("unknown revision")
            || stderr.contains("bad revision")
            || stderr.contains("fatal: invalid object name")
        {
            Ok(None)
        } else {
            Err(anyhow!("git show {spec} failed: {stderr}"))
        }
    }

    /// Bring the working tree to `origin/<branch>` (or an orphan branch if
    /// the wiki has no commits yet), write `key`, and commit+push. Returns
    /// `Rejected` on a non-fast-forward push instead of erroring, so callers
    /// can decide how to react (retry vs. treat as a CAS conflict).
    async fn write_commit_push(&self, key: &str, bytes: &[u8]) -> Result<PushOutcome> {
        #[cfg(not(test))]
        let before_push = async {};
        #[cfg(test)]
        let before_push = self.test_before_push();
        self.write_commit_push_inner(key, bytes, before_push).await
    }

    /// Does the real work; `before_push` is awaited once the commit is
    /// ready locally but before `git push` runs, so a test can simulate
    /// another writer landing a commit on `origin` in that exact window —
    /// the only way a non-fast-forward rejection can genuinely happen,
    /// since every attempt starts by resetting to the latest `origin/
    /// <branch>` it can see.
    async fn write_commit_push_inner(
        &self,
        key: &str,
        bytes: &[u8],
        before_push: impl std::future::Future<Output = ()>,
    ) -> Result<PushOutcome> {
        self.fetch().await?;
        let branch = self.branch().await?;
        let has_remote_branch =
            run_git(&self.clone_dir, &["rev-parse", "--verify", "--quiet", &format!("origin/{branch}")]).await.is_ok();
        if has_remote_branch {
            run_git(&self.clone_dir, &["checkout", "-B", &branch, &format!("origin/{branch}")])
                .await
                .context("checkout tracking branch")?;
        } else {
            // Nothing pushed yet: start from an empty orphan branch so the
            // first commit initializes the wiki.
            run_git(&self.clone_dir, &["checkout", "--orphan", &branch]).await.context("checkout orphan branch")?;
            let _ = run_git(&self.clone_dir, &["rm", "-rf", "--quiet", "--ignore-unmatch", "."]).await;
        }

        let path = self.clone_dir.join(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| format!("create {parent:?}"))?;
        }
        tokio::fs::write(&path, bytes).await.with_context(|| format!("write {path:?}"))?;
        run_git(&self.clone_dir, &["add", "--", key]).await?;

        let commit = git_command(&self.clone_dir)
            .args(self.commit_identity_fallback().await)
            .args(["commit", "--quiet", "-m", &format!("brain: put {key}")])
            .output()
            .await
            .context("spawn git commit")?;
        if !commit.status.success() {
            let stderr = String::from_utf8_lossy(&commit.stderr);
            if stderr.contains("nothing to commit") {
                // Content already matches HEAD: nothing new to push.
                return Ok(PushOutcome::Pushed);
            }
            bail!("git commit failed: {stderr}");
        }

        before_push.await;

        let push = git_output(&self.clone_dir, &["push", "--quiet", "origin", &format!("HEAD:{branch}")]).await?;
        if push.status.success() {
            return Ok(PushOutcome::Pushed);
        }
        let stderr = String::from_utf8_lossy(&push.stderr);
        if stderr.contains("[rejected]") || stderr.contains("non-fast-forward") || stderr.contains("fetch first") {
            return Ok(PushOutcome::Rejected);
        }
        bail!("git push failed: {stderr}")
    }
}

fn parse_wiki_remote(remote: &str) -> Result<(String, String)> {
    super::config::github_wiki_owner_repo(remote)
        .context("not a https://github.com/{owner}/{repo}/wiki URL")
}

fn wiki_clone_url(owner: &str, repo: &str) -> String {
    format!("https://github.com/{owner}/{repo}.wiki.git")
}

fn clone_dir_for(owner: &str, repo: &str) -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".")).join("ninox").join("brain-wiki").join(format!("{owner}-{repo}"))
}

/// `git -C <dir>`, with `GIT_TERMINAL_PROMPT=0` so a host with no usable
/// credentials fails fast instead of hanging on an interactive username/
/// password prompt — the ambient-credential auth model (see the module
/// doc comment) must fail, not block, when there's nothing to find.
fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// Runs `git -C <dir> <args>` and returns its output regardless of exit
/// status, for call sites that need to inspect stderr themselves (a
/// missing ref, an empty remote, a rejected push are all expected,
/// non-error outcomes here, not failures).
async fn git_output(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    git_command(dir).args(args).output().await.with_context(|| format!("spawn git {args:?}"))
}

/// Same as `git_output`, but bails on a non-zero exit — for call sites
/// where every failure is a real error.
async fn run_git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let out = git_output(dir, args).await?;
    if !out.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out)
}

#[async_trait]
impl RemoteStore for GithubWikiRemoteStore {
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse> {
        let _guard = self.lock.lock().await;
        let _process_guard = self.acquire_process_lock().await?;
        match self.show_remote_file(key).await? {
            None => Ok(GetResponse::NotFound),
            Some(bytes) => {
                let etag = sha256_hex(&bytes);
                if if_none_match == Some(etag.as_str()) {
                    Ok(GetResponse::NotModified)
                } else {
                    Ok(GetResponse::Found { bytes, etag })
                }
            }
        }
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
        let _guard = self.lock.lock().await;
        let _process_guard = self.acquire_process_lock().await?;
        let etag = sha256_hex(&bytes);
        // Each `write_commit_push` attempt starts by resetting the working
        // tree to the latest `origin/<branch>` before recreating the commit,
        // so simply retrying it *is* "fetch + rebase": since entry keys are
        // immutable and content-addressed, redoing the same write+commit on
        // top of fresh origin state always produces the same resulting tree
        // a literal `git rebase` of the rejected commit would have, with no
        // possibility of a real conflict (spec: entries can't collide).
        for attempt in 0..MAX_PUSH_ATTEMPTS {
            match self.write_commit_push(key, &bytes).await? {
                PushOutcome::Pushed => return Ok(etag),
                PushOutcome::Rejected => {
                    tracing::warn!("github wiki push rejected for {key} (attempt {}), retrying", attempt + 1);
                    // Force the next attempt's `fetch()` to actually hit the
                    // network: `FETCH_DEBOUNCE` exists to avoid refetching
                    // once per entry within a single sync pass, but it must
                    // never suppress the one fetch this retry structurally
                    // depends on to see the commit that caused the
                    // rejection — otherwise every retry resets to the same
                    // stale state and loses the race identically.
                    *self.last_fetch.lock().await = None;
                }
            }
        }
        bail!("github wiki push for {key} kept losing the race after {MAX_PUSH_ATTEMPTS} attempts")
    }

    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome> {
        let _guard = self.lock.lock().await;
        let _process_guard = self.acquire_process_lock().await?;
        let current = self.show_remote_file(key).await?;
        let current_etag = current.as_ref().map(|b| sha256_hex(b));
        let matches = match (expected_etag, current_etag.as_deref()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(_), None) => false,
            (Some(e), Some(c)) => e == c,
        };
        if !matches {
            return Ok(PutOutcome::PreconditionFailed);
        }
        match self.write_commit_push(key, &bytes).await? {
            PushOutcome::Pushed => Ok(PutOutcome::Ok { etag: sha256_hex(&bytes) }),
            // A race landed between our pre-check and the push: defense in
            // depth, same outcome as the pre-check catching it. The engine's
            // own `sync()` CAS loop retries from a fresh read.
            PushOutcome::Rejected => Ok(PutOutcome::PreconditionFailed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn init_bare_wiki_fixture(dir: &Path) {
        tokio::fs::create_dir_all(dir).await.unwrap();
        run_git(dir, &["init", "--quiet", "--bare", "--initial-branch", "master"]).await.unwrap();
    }

    fn clone_url_for(bare_dir: &Path) -> String {
        bare_dir.to_string_lossy().into_owned()
    }

    /// Minimal, independent "another writer" push straight to `bare_dir`,
    /// used to simulate a racing writer landing a commit on `origin` from
    /// outside the store under test.
    async fn push_directly(bare_dir: &Path, key: &str, bytes: &[u8]) {
        let scratch = tempdir().unwrap();
        let clone_dir = scratch.path().join("racer");
        let bare_url = bare_dir.to_string_lossy().into_owned();
        let clone = Command::new("git").args(["clone", "--quiet", &bare_url]).arg(&clone_dir).output().await.unwrap();
        if !clone.status.success() {
            tokio::fs::create_dir_all(&clone_dir).await.unwrap();
            run_git(&clone_dir, &["init", "--quiet", "--initial-branch", "master"]).await.unwrap();
            run_git(&clone_dir, &["remote", "add", "origin", &bare_url]).await.unwrap();
        }
        let branch_out = run_git(&clone_dir, &["symbolic-ref", "--short", "HEAD"]).await.unwrap();
        let branch = String::from_utf8(branch_out.stdout).unwrap().trim().to_string();
        let path = clone_dir.join(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&path, bytes).await.unwrap();
        run_git(&clone_dir, &["add", "--", key]).await.unwrap();
        run_git(&clone_dir, &["-c", "user.email=racer@example.com", "-c", "user.name=Racer", "commit", "--quiet", "-m", "racer commit"])
            .await
            .unwrap();
        run_git(&clone_dir, &["push", "--quiet", "origin", &format!("HEAD:{branch}")]).await.unwrap();
    }

    #[tokio::test]
    async fn parse_wiki_remote_extracts_owner_and_repo() {
        assert_eq!(parse_wiki_remote("https://github.com/org/repo/wiki").unwrap(), ("org".into(), "repo".into()));
        assert_eq!(parse_wiki_remote("https://github.com/org/repo/wiki/").unwrap(), ("org".into(), "repo".into()));
        assert_eq!(parse_wiki_remote("https://u:t@github.com/org/repo.wiki.git").unwrap(), ("org".into(), "repo".into()));
        assert!(parse_wiki_remote("https://github.com/org/repo").is_err());
        assert!(parse_wiki_remote("s3://bucket/prefix").is_err());
    }

    #[test]
    fn wiki_clone_url_builds_the_dot_wiki_dot_git_url() {
        assert_eq!(wiki_clone_url("org", "repo"), "https://github.com/org/repo.wiki.git");
    }

    #[test]
    fn clone_dir_for_namespaces_by_owner_and_repo() {
        let dir = clone_dir_for("org", "repo");
        assert!(dir.ends_with("ninox/brain-wiki/org-repo"), "{dir:?}");
    }

    #[tokio::test]
    async fn from_config_clones_existing_bare_repo() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let clone_dir = tempdir().unwrap().path().join("clone");

        let store = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), clone_dir.clone()).await.unwrap();
        assert!(clone_dir.join(".git").exists());
        assert_eq!(store.branch().await.unwrap(), "master");
    }

    #[tokio::test]
    async fn from_config_initializes_when_remote_is_unreachable() {
        let clone_dir = tempdir().unwrap().path().join("clone");
        let nonexistent = tempdir().unwrap().path().join("nope.wiki.git");

        let store = GithubWikiRemoteStore::for_clone_url(nonexistent.to_string_lossy().into_owned(), clone_dir.clone())
            .await
            .unwrap();
        assert!(clone_dir.join(".git").exists());
        // get on a never-initialized repo must behave like an empty store,
        // not error.
        assert!(matches!(store.get("manifest.json", None).await.unwrap(), GetResponse::NotFound));
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let store =
            GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("clone"))
                .await
                .unwrap();
        assert!(matches!(store.get("nope.md", None).await.unwrap(), GetResponse::NotFound));
    }

    #[tokio::test]
    async fn put_then_get_round_trips_with_etag() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let store =
            GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("clone"))
                .await
                .unwrap();

        let etag = store.put("entries/a.md@deadbeef", b"# A".to_vec()).await.unwrap();
        match store.get("entries/a.md@deadbeef", None).await.unwrap() {
            GetResponse::Found { bytes, etag: e } => {
                assert_eq!(bytes, b"# A");
                assert_eq!(e, etag);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn conditional_get_returns_not_modified_on_matching_etag() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let store =
            GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("clone"))
                .await
                .unwrap();

        let etag = store.put("manifest.json", b"{}".to_vec()).await.unwrap();
        assert!(matches!(store.get("manifest.json", Some(&etag)).await.unwrap(), GetResponse::NotModified));
        assert!(matches!(store.get("manifest.json", Some("stale")).await.unwrap(), GetResponse::Found { .. }));
    }

    #[tokio::test]
    async fn put_if_match_enforces_cas() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let store =
            GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("clone"))
                .await
                .unwrap();

        let PutOutcome::Ok { etag: e1 } = store.put_if_match("manifest.json", b"v1".to_vec(), None).await.unwrap() else {
            panic!("create-only put should succeed on absent key");
        };
        assert!(matches!(
            store.put_if_match("manifest.json", b"v2".to_vec(), None).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));
        let PutOutcome::Ok { etag: e2 } =
            store.put_if_match("manifest.json", b"v2".to_vec(), Some(&e1)).await.unwrap()
        else {
            panic!("matching-etag put should succeed");
        };
        assert_ne!(e1, e2);
        assert!(matches!(
            store.put_if_match("manifest.json", b"v3".to_vec(), Some(&e1)).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));
    }

    #[tokio::test]
    async fn two_writers_racing_on_the_manifest_the_loser_gets_precondition_failed() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let alice =
            GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("alice"))
                .await
                .unwrap();
        let bob = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("bob"))
            .await
            .unwrap();

        let PutOutcome::Ok { etag } = alice.put_if_match("manifest.json", b"{\"generation\":1}".to_vec(), None).await.unwrap()
        else {
            panic!("alice's create-only put should succeed");
        };

        // Bob still thinks the manifest is absent (stale view) and races
        // Alice: he must lose, not corrupt or silently overwrite.
        assert!(matches!(
            bob.put_if_match("manifest.json", b"{\"generation\":1,\"bob\":true}".to_vec(), None).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));

        // Bob catches up, retries with the real etag, and succeeds.
        let outcome = bob.put_if_match("manifest.json", b"{\"generation\":2}".to_vec(), Some(&etag)).await.unwrap();
        assert!(matches!(outcome, PutOutcome::Ok { .. }));
    }

    #[tokio::test]
    async fn two_writers_pushing_different_entries_concurrently_both_land() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let alice =
            GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("alice"))
                .await
                .unwrap();
        let bob = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("bob"))
            .await
            .unwrap();

        // Alice pushes first, moving the remote branch forward; Bob's clone
        // is now behind when he pushes his own, distinct, immutable entry —
        // his push must be rejected and transparently retried via
        // fetch+rebase rather than erroring or dropping data.
        alice.put("entries/a.md@aaaaaaaa", b"# A".to_vec()).await.unwrap();
        bob.put("entries/b.md@bbbbbbbb", b"# B".to_vec()).await.unwrap();

        let fresh = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("fresh"))
            .await
            .unwrap();
        assert!(matches!(fresh.get("entries/a.md@aaaaaaaa", None).await.unwrap(), GetResponse::Found { .. }));
        assert!(matches!(fresh.get("entries/b.md@bbbbbbbb", None).await.unwrap(), GetResponse::Found { .. }));
    }

    /// Deterministically forces the exact race `put`'s retry loop exists
    /// for: a race hook lands another writer's commit on `origin` in the
    /// narrow window between bob's own fetch (inside `write_commit_push`)
    /// and his push, so his first push attempt is rejected as
    /// non-fast-forward. `put` must retry (fetch the racer's commit, redo
    /// the write on top of it, push again) and succeed, losing neither
    /// entry — not surface an error or silently drop bob's write.
    #[tokio::test]
    async fn put_retries_and_succeeds_after_a_push_is_rejected_by_a_racing_writer() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let bob = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("bob"))
            .await
            .unwrap();

        let bare_path = bare.path().to_path_buf();
        bob.set_race_hook(async move {
            push_directly(&bare_path, "entries/a.md@aaaaaaaa", b"# A").await;
        })
        .await;

        let etag = bob.put("entries/b.md@bbbbbbbb", b"# B".to_vec()).await.unwrap();

        let fresh = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("fresh"))
            .await
            .unwrap();
        match fresh.get("entries/b.md@bbbbbbbb", None).await.unwrap() {
            GetResponse::Found { bytes, etag: e } => {
                assert_eq!(bytes, b"# B");
                assert_eq!(e, etag);
            }
            other => panic!("expected Found, got {other:?}"),
        }
        assert!(
            matches!(fresh.get("entries/a.md@aaaaaaaa", None).await.unwrap(), GetResponse::Found { .. }),
            "the racing writer's own commit must survive the rebase-retry too"
        );
    }

    /// Two `GithubWikiRemoteStore` instances sharing the identical
    /// `clone_dir` — exactly what `clone_dir_for`'s deterministic cache
    /// path produces for two separate `ninox` processes syncing the same
    /// catalogue — must not corrupt each other's writes by racing on the
    /// same on-disk working tree. The cross-process lock (`acquire_process_
    /// lock`) is what's actually exercised here: without it, two `put`s
    /// run concurrently via the *same* shared directory would interleave
    /// `checkout`/`write`/`commit`/`push` steps non-atomically.
    #[tokio::test]
    async fn two_stores_sharing_the_same_clone_dir_do_not_corrupt_each_other() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let shared_clone_dir = tempdir().unwrap().path().join("shared-clone");

        let a = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), shared_clone_dir.clone()).await.unwrap();
        let b = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), shared_clone_dir.clone()).await.unwrap();

        let (ra, rb) = tokio::join!(a.put("entries/a.md@aaaaaaaa", b"# A".to_vec()), b.put("entries/b.md@bbbbbbbb", b"# B".to_vec()));
        ra.unwrap();
        rb.unwrap();

        let fresh = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("fresh"))
            .await
            .unwrap();
        assert!(matches!(fresh.get("entries/a.md@aaaaaaaa", None).await.unwrap(), GetResponse::Found { .. }));
        assert!(matches!(fresh.get("entries/b.md@bbbbbbbb", None).await.unwrap(), GetResponse::Found { .. }));
    }

    /// `put_if_match`'s pre-check read and its write-time `fetch()` can both
    /// land inside `FETCH_DEBOUNCE` and see stale local state if a racing
    /// writer's push happens in between. That staleness is harmless: the
    /// *actual* `git push` is checked by git for fast-forward against the
    /// real remote tip, not against our possibly-stale local tracking ref,
    /// so a conflicting race is still caught (as a push rejection →
    /// `PreconditionFailed`) even when every local read along the way was
    /// stale. This pins that down directly rather than relying on that
    /// reasoning: bob primes a fresh fetch, a racer pushes right after
    /// (within the debounce window), and bob's `put_if_match` — whose own
    /// reads never see the racer's commit — must still fail closed rather
    /// than silently overwriting it.
    #[tokio::test]
    async fn put_if_match_still_rejects_a_race_even_when_every_local_read_was_debounce_stale() {
        let bare = tempdir().unwrap();
        init_bare_wiki_fixture(bare.path()).await;
        let bob = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("bob"))
            .await
            .unwrap();

        // Primes bob's `last_fetch` and establishes "v1" as the canonical
        // manifest both locally and on the real remote.
        let PutOutcome::Ok { etag: v1_etag } =
            bob.put_if_match("manifest.json", b"v1".to_vec(), None).await.unwrap()
        else {
            panic!("create-only put should succeed on absent key");
        };

        // Immediately (well inside FETCH_DEBOUNCE) another writer pushes a
        // conflicting "v2" directly to the real remote, invisible to any of
        // bob's debounced local reads.
        push_directly(bare.path(), "manifest.json", b"v2").await;

        // bob still thinks the manifest is "v1" (that's exactly what his
        // `expected_etag` reflects) and every read he does next will be
        // debounce-stale too — but the real remote has already moved to
        // "v2", and the push itself must still catch that.
        let outcome = bob.put_if_match("manifest.json", b"v3".to_vec(), Some(&v1_etag)).await.unwrap();
        assert!(matches!(outcome, PutOutcome::PreconditionFailed), "must fail closed, not silently overwrite v2");

        // The real remote content must be exactly the racer's "v2" — never
        // bob's "v3", and still intact (not corrupted into something else).
        let fresh = GithubWikiRemoteStore::for_clone_url(clone_url_for(bare.path()), tempdir().unwrap().path().join("fresh"))
            .await
            .unwrap();
        match fresh.get("manifest.json", None).await.unwrap() {
            GetResponse::Found { bytes, .. } => assert_eq!(bytes, b"v2"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rules_file_round_trips_between_two_brains_through_the_wiki() {
        use crate::brain_sync::{BrainSync, SyncToml};
        use std::sync::Arc;

        let scratch = tempdir().unwrap();
        let bare = scratch.path().join("wiki.git");
        init_bare_wiki_fixture(&bare).await;
        let cfg = || SyncToml { remote: "wiki://x/y".into(), endpoint: None, region: None, cache_ttl_secs: 0 };
        let store = |name: &str| {
            let url = clone_url_for(&bare);
            let dir = scratch.path().join(name);
            async move { Arc::new(GithubWikiRemoteStore::for_clone_url(url, dir).await.unwrap()) }
        };

        let alice = scratch.path().join("alice-brain");
        std::fs::create_dir_all(&alice).unwrap();
        crate::brain_rules::write_brain_rules(&alice, "MLOps knowledge only").unwrap();
        std::fs::create_dir_all(alice.join("repos")).unwrap();
        std::fs::write(alice.join("repos/a.md"), "# A").unwrap();
        BrainSync::new(alice.clone(), cfg(), store("clone-alice").await).sync().await.unwrap();

        let bob = scratch.path().join("bob-brain");
        std::fs::create_dir_all(&bob).unwrap();
        BrainSync::new(bob.clone(), cfg(), store("clone-bob").await).sync().await.unwrap();

        assert_eq!(crate::brain_rules::read_brain_rules(&bob).as_deref(), Some("MLOps knowledge only"));
        assert!(bob.join("repos/a.md").exists());
        let idx = crate::BrainIndex::open(&bob).unwrap();
        assert_eq!(idx.rebuild(None).unwrap().indexed, 1, "synced _rules.md stays out of the index");
    }
}
