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

    /// True for a `https://github.com/{owner}/{repo}/wiki` remote, which
    /// `BrainSync::for_brain` routes to `GithubWikiRemoteStore` instead of
    /// `S3RemoteStore`. Requires exactly the three path segments
    /// `{owner}/{repo}/wiki` — a plain repo page that happens to be named
    /// "wiki" (`https://github.com/org/wiki`, two segments) doesn't match,
    /// so it falls through to the (clearer) "not an s3:// URL" error rather
    /// than being misrouted here and failing with a confusing "missing
    /// repo" from `parse_wiki_remote`.
    pub fn is_github_wiki(&self) -> bool {
        is_github_wiki_remote(&self.remote)
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

/// True for a `https://github.com/{owner}/{repo}/wiki` remote URL — see
/// `SyncToml::is_github_wiki`'s doc comment for why the match is this
/// specific. A free function (not just that method) so callers with only a
/// raw `remote` string — e.g. a `CatalogueRef` that hasn't necessarily been
/// materialized into a `.sync.toml` on disk yet, like the GUI's Sources
/// view deriving a source's type badge — don't need to duplicate the logic
/// or construct a throwaway `SyncToml`.
pub fn is_github_wiki_remote(remote: &str) -> bool {
    github_wiki_owner_repo(remote).is_some()
}

/// `(owner, repo)` for `https://github.com/{owner}/{repo}/wiki` or its
/// clone form `https://github.com/{owner}/{repo}.wiki.git`. Any userinfo
/// (`https://user:token@github.com/…`) is ignored: syncing always uses the
/// host's ambient git credentials.
pub fn github_wiki_owner_repo(remote: &str) -> Option<(String, String)> {
    let rest = remote.strip_prefix("https://")?;
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let parts: Vec<&str> = path.trim_end_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let (owner, repo) = match parts.as_slice() {
        [owner, repo, "wiki"] => (*owner, *repo),
        [owner, repo_git] => (*owner, repo_git.strip_suffix(".wiki.git").filter(|r| !r.is_empty())?),
        _ => return None,
    };
    Some((owner.to_string(), repo.to_string()))
}

/// Materialize `.sync.toml` for a brain whose remote is declared in the
/// app config (spec §1 "first open of a catalogue with a remote"). An
/// existing `.sync.toml` always wins; a mismatch only logs a warning.
pub fn ensure_sync_toml(config: &crate::AppConfig, brain_path: &Path) -> Result<()> {
    let Some(candidate) = config.remote_config_for(brain_path) else {
        return Ok(());
    };
    match SyncToml::load(brain_path)? {
        Some(existing) => {
            if existing != candidate {
                tracing::warn!(
                    "brain {}: .sync.toml differs from the config's remote settings; the directory's .sync.toml wins",
                    brain_path.display()
                );
            }
        }
        None => candidate.save(brain_path)?,
    }
    Ok(())
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
    fn is_github_wiki_detects_wiki_remote() {
        let s3 = sample();
        assert!(!s3.is_github_wiki());
        let wiki = SyncToml { remote: "https://github.com/org/repo/wiki".into(), ..sample() };
        assert!(wiki.is_github_wiki());
        let wiki_trailing_slash = SyncToml { remote: "https://github.com/org/repo/wiki/".into(), ..sample() };
        assert!(wiki_trailing_slash.is_github_wiki());
        let not_wiki = SyncToml { remote: "https://github.com/org/repo".into(), ..sample() };
        assert!(!not_wiki.is_github_wiki());
        // A plain repo literally named "wiki" (two path segments) must not
        // be misread as the three-segment owner/repo/wiki form.
        let repo_named_wiki = SyncToml { remote: "https://github.com/org/wiki".into(), ..sample() };
        assert!(!repo_named_wiki.is_github_wiki());
    }

    #[test]
    fn credentialed_and_clone_form_wiki_urls_are_wikis() {
        for url in [
            "https://bob:tok@github.com/acme/handbook/wiki",
            "https://tok@github.com/acme/handbook/wiki/",
            "https://github.com/acme/handbook.wiki.git",
            "https://bob:tok@github.com/acme/handbook.wiki.git",
        ] {
            assert_eq!(github_wiki_owner_repo(url), Some(("acme".into(), "handbook".into())), "{url}");
        }
        for url in [
            "https://github.com.evil.dev/acme/handbook/wiki",
            "https://github.com/acme/.wiki.git",
            "https://github.com/acme/handbook.git",
            "http://github.com/acme/handbook/wiki",
            "s3://bucket/acme/handbook/wiki",
        ] {
            assert!(!is_github_wiki_remote(url), "{url}");
        }
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

    #[test]
    fn ensure_sync_toml_materializes_from_catalogue_config() {
        let dir = tempdir().unwrap();
        let brain_path = dir.path().join("team-brain");
        let mut cfg = crate::AppConfig::default();
        cfg.brain.catalogues = vec![crate::config::CatalogueRef {
            name: "team".into(),
            path: brain_path.clone(),
            remote: Some("s3://team-brains/main".into()),
            endpoint: None,
            region: None,
            cache_ttl_secs: Some(30),
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
        }];
        ensure_sync_toml(&cfg, &brain_path).unwrap();
        let loaded = SyncToml::load(&brain_path).unwrap().unwrap();
        assert_eq!(loaded.remote, "s3://team-brains/main");
        assert_eq!(loaded.cache_ttl_secs, 30);
    }

    #[test]
    fn ensure_sync_toml_never_overwrites_existing_marker() {
        let dir = tempdir().unwrap();
        let existing = SyncToml { remote: "s3://original/x".into(), endpoint: None, region: None, cache_ttl_secs: 0 };
        existing.save(dir.path()).unwrap();
        let mut cfg = crate::AppConfig::default();
        cfg.brain.catalogues = vec![crate::config::CatalogueRef {
            name: "team".into(),
            path: dir.path().to_path_buf(),
            remote: Some("s3://different/y".into()),
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
        }];
        ensure_sync_toml(&cfg, dir.path()).unwrap();
        assert_eq!(SyncToml::load(dir.path()).unwrap().unwrap(), existing, "the directory's .sync.toml wins");
    }

    #[test]
    fn ensure_sync_toml_is_a_noop_without_remote_config() {
        let dir = tempdir().unwrap();
        let cfg = crate::AppConfig::default();
        ensure_sync_toml(&cfg, dir.path()).unwrap();
        assert!(SyncToml::load(dir.path()).unwrap().is_none());
    }
}
