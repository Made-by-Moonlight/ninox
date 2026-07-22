use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

/// Object key of the manifest inside the remote prefix.
pub const MANIFEST_KEY: &str = "manifest.json";
/// Manifest schema version this build reads and writes.
pub const MANIFEST_FORMAT: u32 = 1;
/// File name of the local sync-state file inside a brain directory.
pub const SYNC_STATE: &str = ".sync-state.json";

/// The remote's table of contents — the single consistency anchor. Entry
/// objects are immutable (hash-suffixed keys); the manifest alone decides
/// which version of each entry is current. Design spec §2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub generation: u64,
    pub entries: BTreeMap<String, ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub sha256: String,
    pub size: u64,
    pub updated_by: String,
    pub updated_at: String,
}

impl Manifest {
    pub fn empty() -> Self {
        Self { format: MANIFEST_FORMAT, generation: 0, entries: BTreeMap::new() }
    }

    /// Parse manifest bytes, refusing unknown format versions loudly so an
    /// old ninox never mangles a newer team's remote.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(bytes).context("parse manifest.json")?;
        if m.format != MANIFEST_FORMAT {
            bail!(
                "remote brain manifest format {} is not supported by this ninox (wanted {MANIFEST_FORMAT}) — upgrade ninox",
                m.format
            );
        }
        Ok(m)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn hashes(&self) -> BTreeMap<String, String> {
        self.entries.iter().map(|(k, v)| (k.clone(), v.sha256.clone())).collect()
    }
}

/// Local record of the last agreement with the remote: the manifest
/// generation/ETag last pulled, and per-entry "base" hashes — the content
/// each local file had when it last matched the remote. Functionally a git
/// index; enables the three-way diff in `diff::plan_sync`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncState {
    pub generation: u64,
    pub manifest_etag: Option<String>,
    pub base: BTreeMap<String, String>,
    #[serde(default)]
    pub last_check_unix: u64,
}

impl SyncState {
    pub fn load(brain_path: &Path) -> Result<SyncState> {
        let p = brain_path.join(SYNC_STATE);
        if !p.exists() {
            return Ok(SyncState::default());
        }
        let text = fs::read_to_string(&p).with_context(|| format!("read {p:?}"))?;
        Ok(serde_json::from_str(&text).with_context(|| format!("parse {p:?}"))?)
    }

    pub fn save(&self, brain_path: &Path) -> Result<()> {
        fs::create_dir_all(brain_path)?;
        fs::write(brain_path.join(SYNC_STATE), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Remote object key for an entry version: `entries/{rel}@{hash8}`. The
/// hash suffix makes every version a distinct, immutable object — writers
/// can never corrupt each other's bytes (spec §2).
pub fn entry_key(rel: &str, sha256: &str) -> String {
    format!("entries/{rel}@{}", &sha256[..8.min(sha256.len())])
}

/// Hash every `.md` file under `brain_path`: rel path (forward slashes) →
/// sha256 hex. Mirrors the walk rules of `BrainIndex::rebuild` and
/// `brain_archive::export` (no symlinks, `.md` only), so exactly the files
/// that are indexable are the files that sync.
pub fn scan_local(brain_path: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !brain_path.exists() {
        return Ok(out);
    }
    for entry in WalkDir::new(brain_path).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().is_symlink()
            || !path.is_file()
            || path.extension().and_then(|e| e.to_str()) != Some("md")
        {
            continue;
        }
        let rel = path
            .strip_prefix(brain_path)
            .with_context(|| format!("relativize {path:?}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(path)?;
        out.insert(rel, sha256_hex(&bytes));
    }
    Ok(out)
}

/// `repos/ninox.md` → `repos/ninox.conflict-<user>-<YYYYMMDD-HHMMSS>.md`.
/// The conflict copy lives in the same section so it's indexed and
/// queryable like any entry (spec §3).
pub fn conflict_copy_rel(rel: &str, user: &str, now_unix: u64) -> String {
    let user = crate::slugify(user);
    let user = if user.is_empty() { "unknown".to_string() } else { user };
    let (stem, ext) = rel.rsplit_once('.').unwrap_or((rel, "md"));
    format!("{stem}.conflict-{user}-{}.{ext}", compact_timestamp(now_unix))
}

pub fn current_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "unknown".to_string())
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn rfc3339(now_unix: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(now_unix as i64)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

fn compact_timestamp(now_unix: u64) -> String {
    let fmt = time::format_description::parse_borrowed::<1>("[year][month][day]-[hour][minute][second]")
        .expect("static format description");
    time::OffsetDateTime::from_unix_timestamp(now_unix as i64)
        .ok()
        .and_then(|t| t.format(&fmt).ok())
        .unwrap_or_else(|| now_unix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn sha256_hex_is_stable() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn entry_key_embeds_hash_prefix() {
        assert_eq!(entry_key("repos/ninox.md", &sha256_hex(b"hello")), "entries/repos/ninox.md@2cf24dba");
    }

    #[test]
    fn manifest_round_trips() {
        let mut m = Manifest::empty();
        m.generation = 7;
        m.entries.insert(
            "repos/a.md".into(),
            ManifestEntry { sha256: "abc".into(), size: 3, updated_by: "ethan".into(), updated_at: "2026-07-22T10:00:00Z".into() },
        );
        let loaded = Manifest::from_bytes(&m.to_bytes().unwrap()).unwrap();
        assert_eq!(loaded.generation, 7);
        assert_eq!(loaded.entries["repos/a.md"].sha256, "abc");
        assert_eq!(loaded.hashes()["repos/a.md"], "abc");
    }

    #[test]
    fn manifest_rejects_unknown_format() {
        let bytes = br#"{"format": 99, "generation": 1, "entries": {}}"#;
        assert!(Manifest::from_bytes(bytes).is_err());
    }

    #[test]
    fn sync_state_defaults_when_absent_and_round_trips() {
        let dir = tempdir().unwrap();
        let state = SyncState::load(dir.path()).unwrap();
        assert_eq!(state.generation, 0);
        assert!(state.base.is_empty());

        let mut state = state;
        state.generation = 3;
        state.manifest_etag = Some("e3".into());
        state.base.insert("a.md".into(), "h1".into());
        state.save(dir.path()).unwrap();
        let loaded = SyncState::load(dir.path()).unwrap();
        assert_eq!(loaded.generation, 3);
        assert_eq!(loaded.manifest_etag.as_deref(), Some("e3"));
        assert_eq!(loaded.base["a.md"], "h1");
    }

    #[test]
    fn scan_local_hashes_md_files_only() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("repos")).unwrap();
        fs::write(dir.path().join("repos/a.md"), "hello").unwrap();
        fs::write(dir.path().join(".index.db"), "not markdown").unwrap();
        fs::write(dir.path().join(".sync-state.json"), "{}").unwrap();
        let scan = scan_local(dir.path()).unwrap();
        assert_eq!(scan.len(), 1);
        assert_eq!(scan["repos/a.md"], sha256_hex(b"hello"));
    }

    #[test]
    fn scan_local_of_missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(scan_local(&dir.path().join("nope")).unwrap().is_empty());
    }

    #[test]
    fn conflict_copy_rel_keeps_section_and_extension() {
        // 2026-07-22T10:45:01Z == 1784112301... use a fixed epoch: 1753181101 = 2025-07-22T10:45:01Z is
        // irrelevant — just assert shape, not the exact date digits.
        let rel = conflict_copy_rel("repos/ninox.md", "ethan", 1_753_181_101);
        assert!(rel.starts_with("repos/ninox.conflict-ethan-"), "{rel}");
        assert!(rel.ends_with(".md"), "{rel}");
        assert_eq!(rel.matches(".md").count(), 1);
    }

    #[test]
    fn rfc3339_formats_epoch() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
