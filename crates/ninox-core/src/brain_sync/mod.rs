//! Remote brain sync: team-shared brains over S3-compatible storage.
//! See docs/superpowers/specs/2026-07-22-remote-brain-design.md.

pub mod config;
pub mod diff;
pub mod manifest;
pub mod store;

pub use config::{SyncToml, SYNC_TOML};
pub use diff::{plan_sync, SyncPlan};
pub use manifest::{Manifest, ManifestEntry, SyncState, MANIFEST_KEY, SYNC_STATE};
pub use store::{GetResponse, InMemoryRemoteStore, PutOutcome, RemoteStore};

use crate::brain_sync::manifest::{entry_key, now_unix, scan_local};
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
    /// temp-file + rename so a crash never leaves a half-written entry.
    async fn download_entry(&self, rel: &str, sha256: &str) -> Result<()> {
        let key = entry_key(rel, sha256);
        let GetResponse::Found { bytes, .. } = self.store.get(&key, None).await? else {
            bail!("remote entry object missing: {key}");
        };
        let dest = self.brain_path.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension("md.sync-tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &dest)?;
        Ok(())
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
}
