//! Remote brain sync: team-shared brains over S3-compatible storage.
//! See docs/superpowers/specs/2026-07-22-remote-brain-design.md.

pub mod config;
pub mod diff;
pub mod manifest;
pub mod s3;
pub mod store;

pub use config::{ensure_sync_toml, SyncToml, SYNC_TOML};
pub use diff::{plan_sync, SyncPlan};
pub use manifest::{Manifest, ManifestEntry, SyncState, MANIFEST_KEY, SYNC_STATE};
pub use store::{GetResponse, InMemoryRemoteStore, PutOutcome, RemoteStore};
pub use manifest::rfc3339;

use crate::brain_sync::manifest::{conflict_copy_rel, current_user, entry_key, now_unix, scan_local, sha256_hex};
use anyhow::{bail, Result};
use std::{fs, path::PathBuf, sync::Arc};

/// What a sync actually did — printed by the CLI and used by callers to
/// decide whether the SQLite index needs a rebuild.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// False when the TTL window made this a no-op without a remote call.
    pub checked: bool,
    pub pulled: usize,
    pub pushed: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    /// Rel paths of conflict copies created this sync.
    pub conflicts: Vec<String>,
}

impl SyncReport {
    /// True when files on disk changed and the index must be rebuilt.
    pub fn changed_local(&self) -> bool {
        self.pulled + self.deleted_local + self.conflicts.len() > 0
    }
}

/// The sync engine for one remote-backed brain directory. Purely
/// file-level: it never touches `BrainIndex` — callers rebuild the index
/// when `SyncReport::changed_local()` says so.
pub struct BrainSync {
    brain_path: PathBuf,
    cfg: SyncToml,
    store: Arc<dyn RemoteStore>,
}

impl BrainSync {
    pub fn new(brain_path: PathBuf, cfg: SyncToml, store: Arc<dyn RemoteStore>) -> Self {
        Self { brain_path, cfg, store }
    }

    /// Build the sync engine for a brain directory, or `None` when the
    /// directory has no `.sync.toml` (a plain local brain).
    pub async fn for_brain(brain_path: &std::path::Path) -> Result<Option<BrainSync>> {
        let Some(cfg) = SyncToml::load(brain_path)? else {
            return Ok(None);
        };
        let store = s3::S3RemoteStore::from_config(&cfg).await?;
        Ok(Some(BrainSync::new(brain_path.to_path_buf(), cfg, Arc::new(store))))
    }

    /// The lookup-path check (spec §3 "pull_if_stale"): one conditional GET
    /// of the manifest; on change, apply only the read-safe side of the
    /// diff — files whose local content still matches base. Never pushes,
    /// never overwrites local divergence, never creates conflict copies.
    pub async fn pull_if_stale(&self) -> Result<SyncReport> {
        let mut state = SyncState::load(&self.brain_path)?;
        let now = now_unix();
        if self.cfg.cache_ttl_secs > 0
            && now.saturating_sub(state.last_check_unix) < self.cfg.cache_ttl_secs
        {
            return Ok(SyncReport::default());
        }

        let mut report = SyncReport { checked: true, ..Default::default() };
        let (manifest, etag) = match self.store.get(MANIFEST_KEY, state.manifest_etag.as_deref()).await? {
            GetResponse::NotModified => {
                state.last_check_unix = now;
                state.save(&self.brain_path)?;
                return Ok(report);
            }
            GetResponse::NotFound => (Manifest::empty(), None),
            GetResponse::Found { bytes, etag } => (Manifest::from_bytes(&bytes)?, Some(etag)),
        };

        let local = scan_local(&self.brain_path)?;
        let plan = plan_sync(&state.base, &local, &manifest.hashes());

        for rel in &plan.pulls {
            let sha = manifest.entries[rel].sha256.clone();
            self.download_entry(rel, &sha).await?;
            state.base.insert(rel.clone(), sha);
            report.pulled += 1;
        }
        for rel in &plan.delete_local {
            let _ = fs::remove_file(self.brain_path.join(rel));
            state.base.remove(rel);
            report.deleted_local += 1;
        }
        // Content already converged (e.g. same edit made on both sides):
        // silently record the agreement.
        for rel in &plan.base_updates {
            match local.get(rel) {
                Some(h) => state.base.insert(rel.clone(), h.clone()),
                None => state.base.remove(rel),
            };
        }

        state.manifest_etag = etag;
        state.generation = manifest.generation;
        state.last_check_unix = now;
        state.save(&self.brain_path)?;
        Ok(report)
    }

    /// Download one immutable entry object and move it into place with a
    /// temp-file + rename so a crash never leaves a half-written entry. The
    /// temp file name is unique per call (pid + nanosecond timestamp), not a
    /// single fixed `.sync-tmp` path per entry — two same-machine processes
    /// syncing the same brain dir concurrently (e.g. a CLI `index` and a
    /// server-side freshen) must not interleave writes to the same temp path
    /// and corrupt the entry (and then push the corruption). Same directory
    /// as `dest` so the final rename stays atomic; a non-`.md` extension so
    /// `scan_local` ignores any strays left behind by a crash.
    async fn download_entry(&self, rel: &str, sha256: &str) -> Result<()> {
        let key = entry_key(rel, sha256);
        let GetResponse::Found { bytes, .. } = self.store.get(&key, None).await? else {
            bail!("remote entry object missing: {key}");
        };
        let dest = self.brain_path.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp = dest.with_extension(format!("sync-tmp-{}-{}", std::process::id(), nanos));
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &dest)?;
        Ok(())
    }

    /// Full sync (spec §3): pull, resolve conflicts into conflict copies,
    /// push, all under a bounded manifest compare-and-swap loop. Used by
    /// `ninox brain index` and `ninox brain sync`.
    ///
    /// Guarantee on failure (CAS attempts exhausted): **no data loss**, not
    /// "no disk change". A failed attempt may already have pulled remote
    /// entries or rewritten a conflicting file into a conflict copy before
    /// its manifest CAS lost; that content is preserved on disk (nothing is
    /// silently dropped), but the working tree can differ from how it
    /// looked when `sync()` was called. Callers that need the tree
    /// untouched on error must snapshot/restore themselves.
    pub async fn sync(&self) -> Result<SyncReport> {
        let mut report = SyncReport { checked: true, ..Default::default() };
        const MAX_ATTEMPTS: u64 = 5;
        for attempt in 0..MAX_ATTEMPTS {
            let (mut manifest, etag) = match self.store.get(MANIFEST_KEY, None).await? {
                GetResponse::Found { bytes, etag } => (Manifest::from_bytes(&bytes)?, Some(etag)),
                GetResponse::NotFound => (Manifest::empty(), None),
                GetResponse::NotModified => unreachable!("no If-None-Match sent"),
            };
            let mut state = SyncState::load(&self.brain_path)?;
            let local = scan_local(&self.brain_path)?;
            let plan = plan_sync(&state.base, &local, &manifest.hashes());
            let now = now_unix();

            // Pull side: safe pulls, remote-wins resurrections, deletions.
            for rel in plan.pulls.iter().chain(plan.resurrect_pulls.iter()) {
                self.download_entry(rel, &manifest.entries[rel].sha256).await?;
                report.pulled += 1;
            }
            for rel in &plan.delete_local {
                let _ = fs::remove_file(self.brain_path.join(rel));
                report.deleted_local += 1;
            }

            // Conflicts: remote wins the canonical path; the local version
            // is preserved as a conflict copy and pushed like a new entry.
            let mut pushes = plan.pushes.clone();
            for rel in &plan.conflicts {
                let copy_rel = conflict_copy_rel(rel, &current_user(), now);
                let src = self.brain_path.join(rel);
                let copy_abs = self.brain_path.join(&copy_rel);
                if let Some(parent) = copy_abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&src, &copy_abs)?;
                self.download_entry(rel, &manifest.entries[rel].sha256).await?;
                tracing::warn!("brain sync: conflict on {rel}, local version kept as {copy_rel}");
                pushes.push(copy_rel.clone());
                report.conflicts.push(copy_rel);
            }

            if pushes.is_empty() && plan.delete_remote.is_empty() {
                // Nothing to write remotely — record the agreement and stop.
                state.base = manifest.hashes();
                state.manifest_etag = etag;
                state.generation = manifest.generation;
                state.last_check_unix = now;
                state.save(&self.brain_path)?;
                return Ok(report);
            }

            // Push side: immutable entry objects first, then the manifest CAS.
            let user = current_user();
            for rel in &pushes {
                let bytes = fs::read(self.brain_path.join(rel))?;
                let sha = sha256_hex(&bytes);
                let size = bytes.len() as u64;
                self.store.put(&entry_key(rel, &sha), bytes).await?;
                manifest.entries.insert(
                    rel.clone(),
                    ManifestEntry { sha256: sha, size, updated_by: user.clone(), updated_at: rfc3339(now) },
                );
            }
            for rel in &plan.delete_remote {
                manifest.entries.remove(rel);
            }
            manifest.generation += 1;

            match self.store.put_if_match(MANIFEST_KEY, manifest.to_bytes()?, etag.as_deref()).await? {
                PutOutcome::Ok { etag: new_etag } => {
                    report.pushed += pushes.len();
                    report.deleted_remote += plan.delete_remote.len();
                    state.base = manifest.hashes();
                    state.manifest_etag = Some(new_etag);
                    state.generation = manifest.generation;
                    state.last_check_unix = now;
                    state.save(&self.brain_path)?;
                    return Ok(report);
                }
                PutOutcome::PreconditionFailed => {
                    // Someone pushed between our read and our write. The
                    // objects we uploaded are unreferenced (immutable keys),
                    // so retrying from a fresh manifest is always safe.
                    tracing::warn!("brain sync: lost manifest CAS (attempt {}), retrying", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
                }
            }
        }
        bail!("brain push failed after {MAX_ATTEMPTS} attempts — the remote is being updated concurrently; retry with `ninox brain sync`")
    }
}

/// Offline snapshot for `ninox brain remote status` — deliberately makes
/// no network calls so status always answers instantly.
#[derive(Debug)]
pub struct RemoteStatus {
    pub remote: String,
    pub cache_ttl_secs: u64,
    pub generation: u64,
    pub last_check_unix: u64,
    pub pending_pushes: Vec<String>,
    pub conflict_files: Vec<String>,
}

pub fn remote_status(brain_path: &std::path::Path) -> Result<Option<RemoteStatus>> {
    let Some(cfg) = SyncToml::load(brain_path)? else {
        return Ok(None);
    };
    let state = SyncState::load(brain_path)?;
    let local = scan_local(brain_path)?;
    let pending_pushes: Vec<String> = local
        .iter()
        .filter(|(rel, hash)| state.base.get(*rel) != Some(hash))
        .map(|(rel, _)| rel.clone())
        .chain(state.base.keys().filter(|rel| !local.contains_key(*rel)).cloned())
        .collect();
    let conflict_files: Vec<String> =
        local.keys().filter(|rel| rel.contains(".conflict-")).cloned().collect();
    Ok(Some(RemoteStatus {
        remote: cfg.remote,
        cache_ttl_secs: cfg.cache_ttl_secs,
        generation: state.generation,
        last_check_unix: state.last_check_unix,
        pending_pushes,
        conflict_files,
    }))
}

/// The front door for every read entry point (CLI query/show, server
/// routes): open the brain, and if it's remote-backed, freshen it first
/// with a read-safe `pull_if_stale`. Any remote failure degrades to the
/// local copy with a warning — a lookup is never blocked by S3 (spec §4).
pub async fn open_synced(
    brain_path: &std::path::Path,
    embedder: Option<&dyn crate::embeddings::Embedder>,
) -> Result<crate::BrainIndex> {
    let brain = crate::BrainIndex::open(brain_path)?;
    match BrainSync::for_brain(brain_path).await {
        Ok(None) => {}
        Ok(Some(sync)) => freshen_from(&brain, &sync, embedder).await,
        Err(e) => tracing::warn!("brain: remote unavailable, serving local copy: {e}"),
    }
    Ok(brain)
}

/// Freshen an opened brain from its sync engine, degrading to the local
/// copy with a warning on any remote failure. Split from `open_synced` so
/// the pull-failure degrade path is testable with an injected store.
async fn freshen_from(brain: &crate::BrainIndex, sync: &BrainSync, embedder: Option<&dyn crate::embeddings::Embedder>) {
    match sync.pull_if_stale().await {
        Ok(report) if report.changed_local() => {
            if let Err(e) = brain.rebuild(embedder) {
                tracing::warn!("brain: index rebuild after pull failed: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("brain: remote check failed, serving local copy: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain_sync::manifest::{entry_key, sha256_hex, Manifest, ManifestEntry, SyncState, MANIFEST_KEY};
    use std::{fs, sync::Arc};
    use tempfile::tempdir;

    fn cfg(ttl: u64) -> SyncToml {
        SyncToml { remote: "s3://bucket/prefix".into(), endpoint: None, region: None, cache_ttl_secs: ttl }
    }

    #[tokio::test]
    async fn for_brain_returns_none_without_sync_toml() {
        let dir = tempdir().unwrap();
        assert!(BrainSync::for_brain(dir.path()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn for_brain_rejects_non_s3_remote() {
        let dir = tempdir().unwrap();
        SyncToml { remote: "ftp://nope".into(), endpoint: None, region: None, cache_ttl_secs: 0 }
            .save(dir.path())
            .unwrap();
        assert!(BrainSync::for_brain(dir.path()).await.is_err());
    }

    /// Seed the fake remote with a manifest holding `entries` (rel, body).
    async fn seed_remote(store: &InMemoryRemoteStore, generation: u64, entries: &[(&str, &str)]) {
        let mut manifest = Manifest::empty();
        manifest.generation = generation;
        for (rel, body) in entries {
            let sha = sha256_hex(body.as_bytes());
            store.put(&entry_key(rel, &sha), body.as_bytes().to_vec()).await.unwrap();
            manifest.entries.insert(
                rel.to_string(),
                ManifestEntry { sha256: sha, size: body.len() as u64, updated_by: "teammate".into(), updated_at: "2026-07-22T00:00:00Z".into() },
            );
        }
        store.put(MANIFEST_KEY, manifest.to_bytes().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn pull_if_stale_downloads_new_remote_entries() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        seed_remote(&store, 1, &[("repos/a.md", "# A from teammate")]).await;

        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);
        let report = sync.pull_if_stale().await.unwrap();

        assert!(report.checked);
        assert_eq!(report.pulled, 1);
        assert!(report.changed_local());
        assert_eq!(fs::read_to_string(dir.path().join("repos/a.md")).unwrap(), "# A from teammate");
        let state = SyncState::load(dir.path()).unwrap();
        assert_eq!(state.generation, 1);
        assert!(state.base.contains_key("repos/a.md"));
    }

    #[tokio::test]
    async fn pull_if_stale_not_modified_downloads_nothing() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        seed_remote(&store, 1, &[("repos/a.md", "# A")]).await;
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);
        sync.pull_if_stale().await.unwrap();

        // Second check: manifest ETag unchanged → NotModified fast path.
        let report = sync.pull_if_stale().await.unwrap();
        assert!(report.checked);
        assert_eq!(report.pulled, 0);
        assert!(!report.changed_local());
    }

    #[tokio::test]
    async fn pull_if_stale_within_ttl_skips_remote_entirely() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        seed_remote(&store, 1, &[("repos/a.md", "# A")]).await;
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(3600), store.clone());
        sync.pull_if_stale().await.unwrap();

        store.fail_all.store(true, std::sync::atomic::Ordering::SeqCst);
        // Within the TTL window the store must not even be touched — a
        // failing store proves it.
        let report = sync.pull_if_stale().await.unwrap();
        assert!(!report.checked);
    }

    #[tokio::test]
    async fn pull_if_stale_never_overwrites_locally_diverged_files() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        seed_remote(&store, 1, &[("repos/a.md", "# v1")]).await;
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store.clone());
        sync.pull_if_stale().await.unwrap();

        // Local edit diverges from base; remote also moves on.
        fs::write(dir.path().join("repos/a.md"), "# my local edit").unwrap();
        seed_remote(&store, 2, &[("repos/a.md", "# v2 from teammate")]).await;

        let report = sync.pull_if_stale().await.unwrap();
        assert_eq!(report.pulled, 0, "read path must skip diverged files");
        assert_eq!(fs::read_to_string(dir.path().join("repos/a.md")).unwrap(), "# my local edit");
    }

    #[tokio::test]
    async fn pull_if_stale_applies_remote_deletions_of_untouched_files() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        seed_remote(&store, 1, &[("repos/a.md", "# A"), ("repos/b.md", "# B")]).await;
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store.clone());
        sync.pull_if_stale().await.unwrap();

        seed_remote(&store, 2, &[("repos/b.md", "# B")]).await; // a.md gone
        let report = sync.pull_if_stale().await.unwrap();
        assert_eq!(report.deleted_local, 1);
        assert!(!dir.path().join("repos/a.md").exists());
    }

    #[tokio::test]
    async fn pull_if_stale_with_empty_remote_is_a_noop() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("local.md"), "# mine, not yet pushed").unwrap();
        let store = Arc::new(InMemoryRemoteStore::default()); // no manifest at all
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);
        let report = sync.pull_if_stale().await.unwrap();
        assert!(report.checked);
        assert_eq!(report.pulled, 0);
        assert!(dir.path().join("local.md").exists(), "read path never deletes unpushed local files");
    }

    #[tokio::test]
    async fn pull_if_stale_rejects_unknown_manifest_format_without_touching_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("local.md"), "# mine").unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        store.put(MANIFEST_KEY, br#"{"format": 99, "generation": 1, "entries": {}}"#.to_vec()).await.unwrap();
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);
        assert!(sync.pull_if_stale().await.is_err());
        assert_eq!(fs::read_to_string(dir.path().join("local.md")).unwrap(), "# mine");
    }

    #[tokio::test]
    async fn pull_if_stale_rejects_path_traversal_manifest_entry_without_writing_outside_brain_dir() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        // Hand-built manifest JSON: a manifest containing even one unsafe
        // entry must be rejected wholesale (nothing applied), same as the
        // unknown-format case above.
        let raw = br#"{"format": 1, "generation": 1, "entries": {"../evil.md": {"sha256": "abc", "size": 1, "updated_by": "attacker", "updated_at": "2026-07-22T00:00:00Z"}}}"#;
        store.put(MANIFEST_KEY, raw.to_vec()).await.unwrap();
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);

        let err = sync.pull_if_stale().await.unwrap_err().to_string();
        assert!(err.contains("unsafe path"), "{err}");

        // Nothing was written outside the brain dir, or inside it.
        let escaped = dir.path().parent().unwrap().join("evil.md");
        assert!(!escaped.exists());
        assert!(!dir.path().join("evil.md").exists());
        assert!(fs::read_dir(dir.path()).map(|mut r| r.next().is_none()).unwrap_or(true));
    }

    #[tokio::test]
    async fn sync_pushes_new_local_entries() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("repos")).unwrap();
        fs::write(dir.path().join("repos/mine.md"), "# Mine").unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store.clone());

        let report = sync.sync().await.unwrap();
        assert_eq!(report.pushed, 1);

        // A second, fresh client pulls what we pushed.
        let dir2 = tempdir().unwrap();
        let sync2 = BrainSync::new(dir2.path().to_path_buf(), cfg(0), store);
        let report2 = sync2.pull_if_stale().await.unwrap();
        assert_eq!(report2.pulled, 1);
        assert_eq!(fs::read_to_string(dir2.path().join("repos/mine.md")).unwrap(), "# Mine");
    }

    #[tokio::test]
    async fn sync_is_idempotent_when_nothing_changed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);
        sync.sync().await.unwrap();
        let report = sync.sync().await.unwrap();
        assert_eq!(report.pushed, 0);
        assert_eq!(report.pulled, 0);
        assert!(report.conflicts.is_empty());
    }

    #[tokio::test]
    async fn sync_pushes_local_deletions() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store.clone());
        sync.sync().await.unwrap();

        fs::remove_file(dir.path().join("a.md")).unwrap();
        let report = sync.sync().await.unwrap();
        assert_eq!(report.deleted_remote, 1);

        // A fresh client sees an empty brain.
        let dir2 = tempdir().unwrap();
        let sync2 = BrainSync::new(dir2.path().to_path_buf(), cfg(0), store);
        sync2.pull_if_stale().await.unwrap();
        assert!(crate::brain_sync::manifest::scan_local(dir2.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_conflicting_edits_produce_a_conflict_copy_both_sides_see() {
        let store = Arc::new(InMemoryRemoteStore::default());

        // Alice publishes v1; Bob pulls it.
        let alice = tempdir().unwrap();
        fs::write(alice.path().join("note.md"), "# v1").unwrap();
        let alice_sync = BrainSync::new(alice.path().to_path_buf(), cfg(0), store.clone());
        alice_sync.sync().await.unwrap();
        let bob = tempdir().unwrap();
        let bob_sync = BrainSync::new(bob.path().to_path_buf(), cfg(0), store.clone());
        bob_sync.pull_if_stale().await.unwrap();

        // Both edit the same entry, Alice pushes first.
        fs::write(alice.path().join("note.md"), "# alice edit").unwrap();
        fs::write(bob.path().join("note.md"), "# bob edit").unwrap();
        alice_sync.sync().await.unwrap();
        let report = bob_sync.sync().await.unwrap();

        // Bob: canonical = Alice's version, conflict copy = Bob's, pushed.
        assert_eq!(report.conflicts.len(), 1);
        let copy_rel = &report.conflicts[0];
        assert!(copy_rel.contains(".conflict-"), "{copy_rel}");
        assert_eq!(fs::read_to_string(bob.path().join("note.md")).unwrap(), "# alice edit");
        assert_eq!(fs::read_to_string(bob.path().join(copy_rel)).unwrap(), "# bob edit");

        // Alice pulls and sees the conflict copy too.
        let a_report = alice_sync.pull_if_stale().await.unwrap();
        assert_eq!(a_report.pulled, 1);
        assert_eq!(fs::read_to_string(alice.path().join(copy_rel)).unwrap(), "# bob edit");
    }

    #[tokio::test]
    async fn concurrent_pushes_from_two_clients_converge() {
        // Alice and Bob push independent new entries with stale local
        // state; nothing may be lost. (sync() re-reads the manifest at the
        // top of every attempt, so an in-process interleaved CAS loss can't
        // be forced with the fake — the retry path itself is exercised by
        // sync_gives_up_after_bounded_cas_attempts below.)
        let store = Arc::new(InMemoryRemoteStore::default());
        let alice = tempdir().unwrap();
        fs::write(alice.path().join("a.md"), "# a1").unwrap();
        let alice_sync = BrainSync::new(alice.path().to_path_buf(), cfg(0), store.clone());
        alice_sync.sync().await.unwrap();

        let bob = tempdir().unwrap();
        let bob_sync = BrainSync::new(bob.path().to_path_buf(), cfg(0), store.clone());
        bob_sync.pull_if_stale().await.unwrap();

        // Alice pushes a new entry; Bob's cached ETag is now stale when he
        // pushes his own. Both must land.
        fs::write(alice.path().join("b.md"), "# b1").unwrap();
        alice_sync.sync().await.unwrap();
        fs::write(bob.path().join("c.md"), "# c1").unwrap();
        bob_sync.sync().await.unwrap();

        let fresh = tempdir().unwrap();
        let fresh_sync = BrainSync::new(fresh.path().to_path_buf(), cfg(0), store);
        fresh_sync.pull_if_stale().await.unwrap();
        let scan = crate::brain_sync::manifest::scan_local(fresh.path()).unwrap();
        assert!(scan.contains_key("a.md") && scan.contains_key("b.md") && scan.contains_key("c.md"));
    }

    #[tokio::test]
    async fn sync_gives_up_after_bounded_cas_attempts() {
        use crate::brain_sync::store::{GetResponse, PutOutcome, RemoteStore};
        use async_trait::async_trait;

        /// A store whose manifest CAS always loses — as if a very busy
        /// teammate wins every race.
        struct AlwaysLosesCas(InMemoryRemoteStore);
        #[async_trait]
        impl RemoteStore for AlwaysLosesCas {
            async fn get(&self, key: &str, inm: Option<&str>) -> anyhow::Result<GetResponse> {
                self.0.get(key, inm).await
            }
            async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<String> {
                self.0.put(key, bytes).await
            }
            async fn put_if_match(&self, _k: &str, _b: Vec<u8>, _e: Option<&str>) -> anyhow::Result<PutOutcome> {
                Ok(PutOutcome::PreconditionFailed)
            }
        }

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), Arc::new(AlwaysLosesCas(InMemoryRemoteStore::default())));
        let err = sync.sync().await.unwrap_err().to_string();
        assert!(err.contains("ninox brain sync"), "error should tell the user how to retry: {err}");
        // Local file untouched by the failed push.
        assert_eq!(fs::read_to_string(dir.path().join("a.md")).unwrap(), "# A");
    }

    /// A store whose manifest CAS fails `PreconditionFailed` for the first
    /// `fail_count` calls — as if a teammate wins the race a couple of
    /// times — then delegates to the real in-memory store. `get`/`put`
    /// always delegate. Used to exercise a *successful* retry, unlike
    /// `AlwaysLosesCas` above which only covers permanent exhaustion.
    struct LosesCasNTimes {
        inner: InMemoryRemoteStore,
        fail_count: u64,
        calls: std::sync::atomic::AtomicU64,
    }

    #[async_trait::async_trait]
    impl RemoteStore for LosesCasNTimes {
        async fn get(&self, key: &str, inm: Option<&str>) -> anyhow::Result<GetResponse> {
            self.inner.get(key, inm).await
        }
        async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<String> {
            self.inner.put(key, bytes).await
        }
        async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> anyhow::Result<PutOutcome> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_count {
                return Ok(PutOutcome::PreconditionFailed);
            }
            self.inner.put_if_match(key, bytes, expected_etag).await
        }
    }

    #[tokio::test]
    async fn sync_recovers_after_bounded_cas_losses_lose_then_win() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("repos")).unwrap();
        fs::write(dir.path().join("repos/mine.md"), "# Mine").unwrap();
        let store = Arc::new(LosesCasNTimes {
            inner: InMemoryRemoteStore::default(),
            fail_count: 2,
            calls: std::sync::atomic::AtomicU64::new(0),
        });
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store.clone());

        let report = sync.sync().await.unwrap();
        assert_eq!(report.pushed, 1);
        assert_eq!(report.pulled, 0);
        assert!(report.conflicts.is_empty());

        // The entry really landed remotely: a fresh second client pulls it.
        let dir2 = tempdir().unwrap();
        let sync2 = BrainSync::new(dir2.path().to_path_buf(), cfg(0), store.clone());
        let report2 = sync2.pull_if_stale().await.unwrap();
        assert_eq!(report2.pulled, 1);
        assert_eq!(fs::read_to_string(dir2.path().join("repos/mine.md")).unwrap(), "# Mine");

        // No duplicate copies from the retries: exactly the one .md file,
        // no conflict copies.
        let scan = crate::brain_sync::manifest::scan_local(dir.path()).unwrap();
        assert_eq!(scan.len(), 1, "{scan:?}");
        assert!(!scan.keys().any(|k| k.contains(".conflict-")), "{scan:?}");

        // Idempotence across the retries didn't double-count: a second sync
        // reports nothing new.
        let report3 = sync.sync().await.unwrap();
        assert_eq!(report3.pushed, 0);
        assert_eq!(report3.pulled, 0);
        assert!(report3.conflicts.is_empty());
    }

    #[tokio::test]
    async fn sync_recovers_after_lost_cas_during_conflict_resolution_without_duplicating_the_copy() {
        let inner = InMemoryRemoteStore::default();
        seed_remote(&inner, 1, &[("note.md", "# v1")]).await;
        let store = Arc::new(LosesCasNTimes { inner, fail_count: 1, calls: std::sync::atomic::AtomicU64::new(0) });

        let dir = tempdir().unwrap();
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store.clone());
        sync.pull_if_stale().await.unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("note.md")).unwrap(), "# v1");

        // Local edit diverges from base...
        fs::write(dir.path().join("note.md"), "# local edit").unwrap();
        // ...while the remote independently moves to a different v2.
        seed_remote(&store.inner, 2, &[("note.md", "# v2 from teammate")]).await;

        // sync() must resolve the conflict (remote wins canonically, local
        // edit preserved as a conflict copy and pushed) while surviving
        // exactly one lost manifest CAS along the way.
        let report = sync.sync().await.unwrap();
        assert_eq!(report.conflicts.len(), 1, "{:?}", report.conflicts);
        let copy_rel = report.conflicts[0].clone();

        assert_eq!(fs::read_to_string(dir.path().join("note.md")).unwrap(), "# v2 from teammate");
        assert_eq!(fs::read_to_string(dir.path().join(&copy_rel)).unwrap(), "# local edit");

        // Exactly one conflict copy on disk — the retried attempt must not
        // have created a duplicate.
        let scan = crate::brain_sync::manifest::scan_local(dir.path()).unwrap();
        let conflict_files: Vec<_> = scan.keys().filter(|k| k.contains(".conflict-")).collect();
        assert_eq!(conflict_files.len(), 1, "{scan:?}");

        // The conflict copy was actually pushed: a fresh client pulls it.
        let dir2 = tempdir().unwrap();
        let sync2 = BrainSync::new(dir2.path().to_path_buf(), cfg(0), store);
        sync2.pull_if_stale().await.unwrap();
        assert_eq!(fs::read_to_string(dir2.path().join("note.md")).unwrap(), "# v2 from teammate");
        assert_eq!(fs::read_to_string(dir2.path().join(&copy_rel)).unwrap(), "# local edit");
    }

    #[tokio::test]
    async fn open_synced_without_sync_toml_is_plain_local_open() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        let brain = open_synced(dir.path(), None).await.unwrap();
        brain.rebuild(None).unwrap();
        assert!(brain.get("a.md").unwrap().is_some());
        assert!(!dir.path().join(crate::brain_sync::manifest::SYNC_STATE).exists(), "no sync state for local brains");
    }

    #[tokio::test]
    async fn freshen_from_survives_failing_remote_store() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "---\nname: A\n---\nLocal fact.").unwrap();
        let brain = crate::BrainIndex::open(dir.path()).unwrap();
        brain.rebuild(None).unwrap();

        let store = Arc::new(InMemoryRemoteStore::default());
        store.fail_all.store(true, std::sync::atomic::Ordering::SeqCst);
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);

        // Must not panic or error — the local brain keeps serving.
        freshen_from(&brain, &sync, None).await;
        assert!(brain.get("a.md").unwrap().is_some());
    }

    #[tokio::test]
    async fn open_synced_survives_unreachable_remote() {
        // .sync.toml points at a remote that can't be reached / built —
        // open_synced must still return a working local BrainIndex.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        SyncToml { remote: "ftp://invalid".into(), endpoint: None, region: None, cache_ttl_secs: 0 }
            .save(dir.path())
            .unwrap();
        let brain = open_synced(dir.path(), None).await.unwrap();
        brain.rebuild(None).unwrap();
        assert!(brain.get("a.md").unwrap().is_some());
    }

    #[test]
    fn remote_status_none_for_local_brain() {
        let dir = tempdir().unwrap();
        assert!(remote_status(dir.path()).unwrap().is_none());
    }

    #[tokio::test]
    async fn remote_status_reports_pending_and_conflicts() {
        let dir = tempdir().unwrap();
        let store = Arc::new(InMemoryRemoteStore::default());
        seed_remote(&store, 1, &[("a.md", "# A")]).await;
        cfg(0).save(dir.path()).unwrap();
        let sync = BrainSync::new(dir.path().to_path_buf(), cfg(0), store);
        sync.pull_if_stale().await.unwrap();

        fs::write(dir.path().join("a.md"), "# A locally edited").unwrap();
        fs::write(dir.path().join("b.conflict-ethan-20260722-100000.md"), "# leftover").unwrap();

        let status = remote_status(dir.path()).unwrap().unwrap();
        assert_eq!(status.remote, "s3://bucket/prefix");
        assert_eq!(status.generation, 1);
        assert!(status.pending_pushes.contains(&"a.md".to_string()));
        assert_eq!(status.conflict_files, vec!["b.conflict-ethan-20260722-100000.md"]);
    }
}
