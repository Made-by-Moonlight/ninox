use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

/// File name of the remote-sync marker inside a brain directory. A brain
/// with this file is remote-backed; without it, nothing about the brain's
/// behavior changes. Never synced, gitignored (see `brain::ensure_gitignore`).
pub const SYNC_TOML: &str = ".sync.toml";

/// Contents of `.sync.toml` — the brain directory self-describes its remote
/// so any process that opens it (CLI, server) discovers the remote without
/// extra plumbing. See the design spec §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncToml {
    /// `s3://bucket/prefix`
    pub remote: String,
    /// Custom endpoint for S3-compatible stores (R2, MinIO, GCS interop).
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Seconds a freshness check stays valid. 0 = check every lookup.
    #[serde(default)]
    pub cache_ttl_secs: u64,
}

impl SyncToml {
    pub fn load(brain_path: &Path) -> Result<Option<SyncToml>> {
        let p = brain_path.join(SYNC_TOML);
        if !p.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&p).with_context(|| format!("read {p:?}"))?;
        Ok(Some(toml::from_str(&text).with_context(|| format!("parse {p:?}"))?))
    }

    pub fn save(&self, brain_path: &Path) -> Result<()> {
        fs::create_dir_all(brain_path)?;
        let p = brain_path.join(SYNC_TOML);
        fs::write(&p, toml::to_string(self)?).with_context(|| format!("write {p:?}"))?;
        Ok(())
    }

    /// Split `s3://bucket/prefix` into `(bucket, prefix)`; prefix may be "".
    pub fn bucket_and_prefix(&self) -> Result<(String, String)> {
        let Some(rest) = self.remote.strip_prefix("s3://") else {
            bail!("remote {:?} is not an s3:// URL", self.remote);
        };
        match rest.split_once('/') {
            Some((bucket, prefix)) => Ok((bucket.to_string(), prefix.trim_end_matches('/').to_string())),
            None => Ok((rest.to_string(), String::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample() -> SyncToml {
        SyncToml {
            remote: "s3://team-brains/main".into(),
            endpoint: Some("https://minio.local:9000".into()),
            region: None,
            cache_ttl_secs: 30,
        }
    }

    #[test]
    fn load_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        assert!(SyncToml::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        sample().save(dir.path()).unwrap();
        let loaded = SyncToml::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded, sample());
    }

    #[test]
    fn cache_ttl_defaults_to_zero() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(SYNC_TOML), "remote = \"s3://b/p\"\n").unwrap();
        let loaded = SyncToml::load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.cache_ttl_secs, 0);
        assert!(loaded.endpoint.is_none());
    }

    #[test]
    fn bucket_and_prefix_parses_s3_url() {
        let cfg = sample();
        assert_eq!(cfg.bucket_and_prefix().unwrap(), ("team-brains".into(), "main".into()));
        let no_prefix = SyncToml { remote: "s3://bucket-only".into(), ..sample() };
        assert_eq!(no_prefix.bucket_and_prefix().unwrap(), ("bucket-only".into(), "".into()));
        let bad = SyncToml { remote: "https://not-s3".into(), ..sample() };
        assert!(bad.bucket_and_prefix().is_err());
    }
}
