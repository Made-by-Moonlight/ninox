# Remote Brains Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Team-shared brains: canonical copy in S3-compatible storage, mirrored locally for fast lookups, freshness-checked on every lookup, writable by everyone with conflict copies on concurrent edits.

**Architecture:** A `manifest.json` in S3 is the consistency anchor; each markdown entry is an immutable, hash-suffixed object. A local `.sync-state.json` records per-entry base hashes (like a git index) enabling a three-way diff (base/local/remote). Lookups do one conditional GET of the manifest; `ninox brain index` does full pull+push with compare-and-swap manifest updates. Spec: `docs/superpowers/specs/2026-07-22-remote-brain-design.md`.

**Tech Stack:** Rust workspace (ninox-core / ninox-server / ninox-app), `aws-sdk-s3` + `aws-config` behind a `RemoteStore` trait, `sha2` for content hashes, `time` for timestamps, existing `rusqlite`/FTS index untouched.

## Global Constraints

- Only `.md` files ever leave the machine. `.index.db`, `.sync.toml`, `.sync-state.json` are never uploaded.
- A brain directory without `.sync.toml` must behave byte-for-byte as today: zero network calls, zero new files (except `.gitignore` gaining two lines).
- A query must NEVER fail or block because the remote is unreachable — warn once, serve local.
- The GPUI app layer (`ninox-app` UI code in `app.rs`/`components/`) must not gain network calls (existing "no HTTP client in app" rule). All sync runs in ninox-core, invoked from CLI subcommands and ninox-server routes only.
- Manifest `format` is `1`. An unknown format fails sync loudly without touching local files.
- New dependencies go through `[workspace.dependencies]` in the root `Cargo.toml`.
- Conventional commits, no co-authors.
- Run tests with `cargo test -p ninox-core <filter>` (or `-p ninox-server`). Full check before finishing a task: `cargo test -p <crate>`.

## File Structure

```
crates/ninox-core/src/brain_sync/mod.rs       # BrainSync engine, SyncReport, open_synced, remote_status
crates/ninox-core/src/brain_sync/config.rs    # SyncToml (.sync.toml) load/save, ensure_sync_toml
crates/ninox-core/src/brain_sync/manifest.rs  # Manifest, ManifestEntry, SyncState, sha256, entry keys, scan_local
crates/ninox-core/src/brain_sync/store.rs     # RemoteStore trait, GetResponse, PutOutcome, InMemoryRemoteStore
crates/ninox-core/src/brain_sync/diff.rs      # plan_sync three-way diff, SyncPlan
crates/ninox-core/src/brain_sync/s3.rs        # S3RemoteStore (aws-sdk-s3), BrainSync::for_brain factory
crates/ninox-core/src/config.rs               # BrainConfig/CatalogueRef remote fields, remote_config_for
crates/ninox-core/src/brain.rs                # ensure_gitignore extension, BrainIndex::path()
crates/ninox-core/src/lib.rs                  # pub mod brain_sync
crates/ninox-app/src/main.rs                  # CLI wiring: sync on index/query/show, brain sync/remote subcommands
crates/ninox-server/src/routes/brain.rs       # pull_if_stale before query/get, full sync on POST /index
crates/ninox-server/src/server.rs             # construct BrainSync at startup
docs/BRAIN.md                                 # remote brain documentation
.claude/skills/brain/SKILL.md                 # note that sync is automatic
```

---

### Task 1: SyncToml config file + remote fields on BrainConfig/CatalogueRef

**Files:**
- Create: `crates/ninox-core/src/brain_sync/mod.rs` (module skeleton)
- Create: `crates/ninox-core/src/brain_sync/config.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod brain_sync;` after `pub mod brain_archive;`)
- Modify: `crates/ninox-core/src/config.rs` (fields on `BrainConfig` + `CatalogueRef`, `AppConfig::remote_config_for`)

**Interfaces:**
- Produces: `brain_sync::config::SyncToml { remote: String, endpoint: Option<String>, region: Option<String>, cache_ttl_secs: u64 }` with `load(brain_path: &Path) -> Result<Option<SyncToml>>`, `save(&self, brain_path: &Path) -> Result<()>`, `bucket_and_prefix(&self) -> Result<(String, String)>`; const `SYNC_TOML: &str = ".sync.toml"`.
- Produces: `AppConfig::remote_config_for(&self, brain_path: &Path) -> Option<SyncToml>`.
- Produces: optional fields `remote`, `endpoint`, `region`, `cache_ttl_secs` on both `BrainConfig` and `CatalogueRef`.

- [ ] **Step 1: Write the failing tests**

Create `crates/ninox-core/src/brain_sync/mod.rs`:

```rust
//! Remote brain sync: team-shared brains over S3-compatible storage.
//! See docs/superpowers/specs/2026-07-22-remote-brain-design.md.

pub mod config;

pub use config::{SyncToml, SYNC_TOML};
```

Create `crates/ninox-core/src/brain_sync/config.rs` with ONLY the tests module for now:

```rust
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

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
```

Add to `crates/ninox-core/src/config.rs` tests module:

```rust
#[test]
fn remote_config_for_matches_catalogue_by_path() {
    let mut cfg = AppConfig::default();
    cfg.brain.catalogues = vec![CatalogueRef {
        name: "team".into(),
        path: PathBuf::from("/tmp/team-brain"),
        remote: Some("s3://team-brains/main".into()),
        endpoint: None,
        region: Some("eu-west-1".into()),
        cache_ttl_secs: Some(60),
    }];
    let sync = cfg.remote_config_for(Path::new("/tmp/team-brain")).unwrap();
    assert_eq!(sync.remote, "s3://team-brains/main");
    assert_eq!(sync.region.as_deref(), Some("eu-west-1"));
    assert_eq!(sync.cache_ttl_secs, 60);
    assert!(cfg.remote_config_for(Path::new("/tmp/other")).is_none());
}

#[test]
fn remote_config_for_matches_default_brain() {
    let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let mut cfg = AppConfig::default();
    cfg.brain.remote = Some("s3://team-brains/default".into());
    let path = cfg.resolved_brain_path();
    let sync = cfg.remote_config_for(&path).unwrap();
    assert_eq!(sync.remote, "s3://team-brains/default");
    assert_eq!(sync.cache_ttl_secs, 0);
}

#[test]
fn catalogue_without_remote_yields_none() {
    let mut cfg = AppConfig::default();
    cfg.brain.catalogues = vec![CatalogueRef {
        name: "local".into(),
        path: PathBuf::from("/tmp/local-brain"),
        remote: None,
        endpoint: None,
        region: None,
        cache_ttl_secs: None,
    }];
    assert!(cfg.remote_config_for(Path::new("/tmp/local-brain")).is_none());
}

#[test]
fn catalogue_ref_remote_fields_default_to_none_in_toml() {
    let toml_src = "port = 8080\nfont_size = 13.0\n\n[[brain.catalogues]]\nname = \"docs\"\npath = \"/tmp/docs\"\n";
    let cfg: AppConfig = toml::from_str(toml_src).unwrap();
    assert!(cfg.brain.catalogues[0].remote.is_none());
    assert!(cfg.brain.catalogues[0].cache_ttl_secs.is_none());
}
```

Note: existing tests construct `CatalogueRef { name, path }` — they will need the new fields. Update the two existing constructions in `config.rs` tests (`catalogue_options_appends_configured_catalogues_and_skips_duplicate_default`) with `remote: None, endpoint: None, region: None, cache_ttl_secs: None`, and check for other `CatalogueRef {` literals across the workspace (`grep -rn "CatalogueRef {" crates/`) — add the fields there too (there are constructions in `ninox-app` tests).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core brain_sync:: remote_config_for 2>&1 | head -30`
Expected: compile errors — `SyncToml` not defined, `remote` field missing on `CatalogueRef`.

- [ ] **Step 3: Implement**

In `crates/ninox-core/src/lib.rs`, after `pub mod brain_archive;`:

```rust
pub mod brain_sync;
```

Fill `crates/ninox-core/src/brain_sync/config.rs` above the tests module:

```rust
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
```

In `crates/ninox-core/src/config.rs`, extend the two structs (flat fields, matching the spec):

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    pub path: Option<PathBuf>,
    /// Remote backing for the default brain: `s3://bucket/prefix`.
    pub remote: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub cache_ttl_secs: Option<u64>,
    #[serde(default)]
    pub catalogues: Vec<CatalogueRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogueRef {
    pub name: String,
    pub path: PathBuf,
    /// Optional remote backing: `s3://bucket/prefix`. On first open the
    /// local dir is materialized with a `.sync.toml` built from these
    /// fields (see `brain_sync::config::ensure_sync_toml`).
    pub remote: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub cache_ttl_secs: Option<u64>,
}
```

Add to `impl AppConfig` (uses the crate-internal type, no cycle — same crate):

```rust
    /// The remote-sync settings configured for `brain_path`, if any: the
    /// default brain's `[brain]` remote fields when `brain_path` is the
    /// resolved default, else a `[[brain.catalogues]]` entry whose `path`
    /// matches exactly. Returns the `.sync.toml` payload to materialize.
    pub fn remote_config_for(&self, brain_path: &std::path::Path) -> Option<crate::brain_sync::SyncToml> {
        let build = |remote: &Option<String>, endpoint: &Option<String>, region: &Option<String>, ttl: &Option<u64>| {
            remote.as_ref().map(|r| crate::brain_sync::SyncToml {
                remote: r.clone(),
                endpoint: endpoint.clone(),
                region: region.clone(),
                cache_ttl_secs: ttl.unwrap_or(0),
            })
        };
        if self.brain.remote.is_some() && brain_path == self.resolved_brain_path() {
            return build(&self.brain.remote, &self.brain.endpoint, &self.brain.region, &self.brain.cache_ttl_secs);
        }
        self.brain
            .catalogues
            .iter()
            .find(|c| c.path == brain_path)
            .and_then(|c| build(&c.remote, &c.endpoint, &c.region, &c.cache_ttl_secs))
    }
```

Fix every `CatalogueRef { ... }` literal found by `grep -rn "CatalogueRef {" crates/` to include the four new fields as `None`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync::config remote_config_for catalogue_ref_remote && cargo test -p ninox-app 2>&1 | tail -5`
Expected: PASS (including previously-existing config and app tests).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src crates/ninox-app/src
git commit -m "feat(brain): add .sync.toml config and remote fields on brain catalogues"
```

---

### Task 2: Manifest, SyncState, hashing, entry keys, local scan

**Files:**
- Create: `crates/ninox-core/src/brain_sync/manifest.rs`
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (add `pub mod manifest;` + re-exports)
- Modify: root `Cargo.toml` (`[workspace.dependencies]`: `sha2 = "0.10"`, `time = { version = "0.3", features = ["formatting"] }`)
- Modify: `crates/ninox-core/Cargo.toml` (`sha2 = { workspace = true }`, `time = { workspace = true }`)

**Interfaces:**
- Produces: `Manifest { format: u32, generation: u64, entries: BTreeMap<String, ManifestEntry> }` with `empty()`, `from_bytes(&[u8]) -> Result<Manifest>` (fails on unknown format), `to_bytes(&self) -> Result<Vec<u8>>`, `hashes(&self) -> BTreeMap<String, String>`.
- Produces: `ManifestEntry { sha256: String, size: u64, updated_by: String, updated_at: String }`.
- Produces: `SyncState { generation: u64, manifest_etag: Option<String>, base: BTreeMap<String, String>, last_check_unix: u64 }` with `load(brain_path) -> Result<SyncState>` (default when absent) and `save(&self, brain_path) -> Result<()>`.
- Produces: `sha256_hex(bytes: &[u8]) -> String`, `entry_key(rel: &str, sha256: &str) -> String` (`entries/{rel}@{first 8 hex}`), `scan_local(brain_path: &Path) -> Result<BTreeMap<String, String>>` (rel path → sha256, `.md` only), `conflict_copy_rel(rel: &str, user: &str, now_unix: u64) -> String`, `current_user() -> String`, `now_unix() -> u64`, `rfc3339(now_unix: u64) -> String`.
- Consumes: nothing from other tasks.
- Constants: `MANIFEST_KEY: &str = "manifest.json"`, `MANIFEST_FORMAT: u32 = 1`, `SYNC_STATE: &str = ".sync-state.json"`.

- [ ] **Step 1: Add dependencies**

Root `Cargo.toml` `[workspace.dependencies]` (after `semver`):

```toml
sha2        = "0.10"
time        = { version = "0.3", features = ["formatting"] }
```

`crates/ninox-core/Cargo.toml` `[dependencies]`:

```toml
sha2       = { workspace = true }
time       = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/ninox-core/src/brain_sync/manifest.rs` with the tests module:

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ninox-core brain_sync::manifest 2>&1 | head -20`
Expected: compile errors — functions/types not defined.

- [ ] **Step 4: Implement**

Above the tests module in `manifest.rs`:

```rust
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
    let fmt = time::format_description::parse("[year][month][day]-[hour][minute][second]")
        .expect("static format description");
    time::OffsetDateTime::from_unix_timestamp(now_unix as i64)
        .ok()
        .and_then(|t| t.format(&fmt).ok())
        .unwrap_or_else(|| now_unix.to_string())
}
```

In `brain_sync/mod.rs` add:

```rust
pub mod manifest;

pub use manifest::{Manifest, ManifestEntry, SyncState, MANIFEST_KEY, SYNC_STATE};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync::manifest`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ninox-core
git commit -m "feat(brain): add sync manifest, sync-state, and content hashing"
```

---

### Task 3: RemoteStore trait + in-memory fake with conditional semantics

**Files:**
- Create: `crates/ninox-core/src/brain_sync/store.rs`
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (add `pub mod store;` + re-exports)

**Interfaces:**
- Produces:

```rust
#[async_trait::async_trait]
pub trait RemoteStore: Send + Sync {
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse>;
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String>; // -> etag
    /// expected_etag None = create-only (If-None-Match: *).
    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome>;
}
pub enum GetResponse { Found { bytes: Vec<u8>, etag: String }, NotModified, NotFound }
pub enum PutOutcome { Ok { etag: String }, PreconditionFailed }
pub struct InMemoryRemoteStore { /* Mutex<BTreeMap<String,(Vec<u8>,String)>>, AtomicU64 etag counter, AtomicBool fail_all */ }
```

- Consumes: nothing from other tasks.

- [ ] **Step 1: Write the failing tests**

Create `crates/ninox-core/src/brain_sync/store.rs` with the tests module:

```rust
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let store = InMemoryRemoteStore::default();
        assert!(matches!(store.get("nope", None).await.unwrap(), GetResponse::NotFound));
    }

    #[tokio::test]
    async fn put_then_get_round_trips_with_etag() {
        let store = InMemoryRemoteStore::default();
        let etag = store.put("k", b"v".to_vec()).await.unwrap();
        match store.get("k", None).await.unwrap() {
            GetResponse::Found { bytes, etag: e } => {
                assert_eq!(bytes, b"v");
                assert_eq!(e, etag);
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn conditional_get_returns_not_modified_on_matching_etag() {
        let store = InMemoryRemoteStore::default();
        let etag = store.put("k", b"v".to_vec()).await.unwrap();
        assert!(matches!(store.get("k", Some(&etag)).await.unwrap(), GetResponse::NotModified));
        assert!(matches!(store.get("k", Some("stale")).await.unwrap(), GetResponse::Found { .. }));
    }

    #[tokio::test]
    async fn put_if_match_enforces_cas() {
        let store = InMemoryRemoteStore::default();
        // create-only succeeds when absent, fails when present
        let PutOutcome::Ok { etag: e1 } = store.put_if_match("k", b"v1".to_vec(), None).await.unwrap() else {
            panic!("create-only put should succeed on absent key");
        };
        assert!(matches!(
            store.put_if_match("k", b"v2".to_vec(), None).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));
        // matching etag succeeds, stale etag fails
        let PutOutcome::Ok { etag: e2 } = store.put_if_match("k", b"v2".to_vec(), Some(&e1)).await.unwrap() else {
            panic!("matching-etag put should succeed");
        };
        assert_ne!(e1, e2);
        assert!(matches!(
            store.put_if_match("k", b"v3".to_vec(), Some(&e1)).await.unwrap(),
            PutOutcome::PreconditionFailed
        ));
    }

    #[tokio::test]
    async fn fail_all_makes_every_call_error() {
        let store = InMemoryRemoteStore::default();
        store.fail_all.store(true, Ordering::SeqCst);
        assert!(store.get("k", None).await.is_err());
        assert!(store.put("k", vec![]).await.is_err());
        assert!(store.put_if_match("k", vec![], None).await.is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core brain_sync::store 2>&1 | head -20`
Expected: compile errors — types not defined.

- [ ] **Step 3: Implement**

Above the tests module:

```rust
#[derive(Debug)]
pub enum GetResponse {
    Found { bytes: Vec<u8>, etag: String },
    /// Conditional get: the caller's ETag still matches.
    NotModified,
    NotFound,
}

#[derive(Debug)]
pub enum PutOutcome {
    Ok { etag: String },
    /// The compare-and-swap lost: the object changed since `expected_etag`
    /// (or already exists, for create-only puts).
    PreconditionFailed,
}

/// Minimal conditional-request surface of an S3-compatible object store —
/// exactly what the sync engine needs and nothing more. Implemented by
/// `s3::S3RemoteStore` for real use and `InMemoryRemoteStore` for tests.
#[async_trait]
pub trait RemoteStore: Send + Sync {
    /// `if_none_match: Some(etag)` turns this into a conditional get that
    /// answers `NotModified` when the object still has that ETag.
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse>;
    /// Unconditional write; returns the new ETag. Used only for immutable,
    /// hash-keyed entry objects where overwrites are byte-identical.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String>;
    /// Compare-and-swap write. `expected_etag: None` means create-only
    /// (`If-None-Match: *`). Used only for the manifest.
    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome>;
}

/// Test double with real conditional-request semantics. `fail_all` makes
/// every call error, for offline-fallback tests.
#[derive(Default)]
pub struct InMemoryRemoteStore {
    objects: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
    etag_counter: AtomicU64,
    pub fail_all: AtomicBool,
}

impl InMemoryRemoteStore {
    fn next_etag(&self) -> String {
        format!("e{}", self.etag_counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn check_up(&self) -> Result<()> {
        if self.fail_all.load(Ordering::SeqCst) {
            bail!("simulated remote failure");
        }
        Ok(())
    }
}

#[async_trait]
impl RemoteStore for InMemoryRemoteStore {
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse> {
        self.check_up()?;
        let objects = self.objects.lock().unwrap();
        match objects.get(key) {
            None => Ok(GetResponse::NotFound),
            Some((bytes, etag)) => {
                if if_none_match == Some(etag.as_str()) {
                    Ok(GetResponse::NotModified)
                } else {
                    Ok(GetResponse::Found { bytes: bytes.clone(), etag: etag.clone() })
                }
            }
        }
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
        self.check_up()?;
        let etag = self.next_etag();
        self.objects.lock().unwrap().insert(key.to_string(), (bytes, etag.clone()));
        Ok(etag)
    }

    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome> {
        self.check_up()?;
        let mut objects = self.objects.lock().unwrap();
        let current = objects.get(key).map(|(_, e)| e.as_str());
        let ok = match (expected_etag, current) {
            (None, None) => true,                       // create-only, absent
            (None, Some(_)) => false,                   // create-only, exists
            (Some(_), None) => false,                   // expected something, gone
            (Some(e), Some(c)) => e == c,
        };
        if !ok {
            return Ok(PutOutcome::PreconditionFailed);
        }
        let etag = self.next_etag();
        objects.insert(key.to_string(), (bytes, etag.clone()));
        Ok(PutOutcome::Ok { etag })
    }
}
```

In `brain_sync/mod.rs` add:

```rust
pub mod store;

pub use store::{GetResponse, InMemoryRemoteStore, PutOutcome, RemoteStore};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync::store`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core
git commit -m "feat(brain): add RemoteStore trait with conditional semantics and in-memory fake"
```

---

### Task 4: Three-way diff (`plan_sync`)

**Files:**
- Create: `crates/ninox-core/src/brain_sync/diff.rs`
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (add `pub mod diff;` + re-exports)

**Interfaces:**
- Produces:

```rust
pub struct SyncPlan {
    pub pulls: Vec<String>,           // local == base, remote changed → download
    pub resurrect_pulls: Vec<String>, // local deleted + remote edited → remote edit wins (full sync only)
    pub pushes: Vec<String>,          // local changed, remote == base (also: remote deleted + local edited)
    pub delete_local: Vec<String>,    // local == base, remote deleted
    pub delete_remote: Vec<String>,   // local deleted, remote == base
    pub conflicts: Vec<String>,       // both changed to different content
    pub base_updates: Vec<String>,    // content already equal, base stale
}
pub fn plan_sync(base, local, remote: &BTreeMap<String, String>) -> SyncPlan
```

- Consumes: nothing (pure function over hash maps; hashes from Task 2's `scan_local` / `Manifest::hashes`).

- [ ] **Step 1: Write the failing tests**

Create `crates/ninox-core/src/brain_sync/diff.rs` with the tests module. The tests cover the entire matrix from the spec §3 table:

```rust
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn all_equal_is_a_noop() {
        let m = map(&[("a.md", "h1")]);
        assert_eq!(plan_sync(&m, &m, &m), SyncPlan::default());
    }

    #[test]
    fn remote_changed_local_untouched_pulls() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.pulls, vec!["a.md"]);
        assert_eq!(plan.pushes, Vec::<String>::new());
    }

    #[test]
    fn local_changed_remote_untouched_pushes() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.pushes, vec!["a.md"]);
    }

    #[test]
    fn both_changed_same_content_updates_base_only() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.base_updates, vec!["a.md"]);
        assert!(plan.pulls.is_empty() && plan.pushes.is_empty() && plan.conflicts.is_empty());
    }

    #[test]
    fn both_changed_differently_conflicts() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[("a.md", "h3")]));
        assert_eq!(plan.conflicts, vec!["a.md"]);
    }

    #[test]
    fn local_delete_remote_untouched_deletes_remote() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.delete_remote, vec!["a.md"]);
    }

    #[test]
    fn remote_delete_local_untouched_deletes_local() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h1")]), &map(&[]));
        assert_eq!(plan.delete_local, vec!["a.md"]);
    }

    #[test]
    fn local_delete_vs_remote_edit_resurrects() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.resurrect_pulls, vec!["a.md"]);
        assert!(plan.pulls.is_empty(), "resurrection must not run on the read path");
    }

    #[test]
    fn remote_delete_vs_local_edit_pushes_the_edit() {
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]), &map(&[]));
        assert_eq!(plan.pushes, vec!["a.md"]);
    }

    #[test]
    fn brand_new_local_pushes() {
        let plan = plan_sync(&map(&[]), &map(&[("a.md", "h1")]), &map(&[]));
        assert_eq!(plan.pushes, vec!["a.md"]);
    }

    #[test]
    fn brand_new_remote_pulls() {
        let plan = plan_sync(&map(&[]), &map(&[]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.pulls, vec!["a.md"]);
    }

    #[test]
    fn new_on_both_sides_same_content_updates_base() {
        let plan = plan_sync(&map(&[]), &map(&[("a.md", "h1")]), &map(&[("a.md", "h1")]));
        assert_eq!(plan.base_updates, vec!["a.md"]);
    }

    #[test]
    fn new_on_both_sides_different_content_conflicts() {
        let plan = plan_sync(&map(&[]), &map(&[("a.md", "h1")]), &map(&[("a.md", "h2")]));
        assert_eq!(plan.conflicts, vec!["a.md"]);
    }

    #[test]
    fn deleted_everywhere_updates_base() {
        // In base, gone from both local and remote: only the stale base
        // record remains to clean up.
        let plan = plan_sync(&map(&[("a.md", "h1")]), &map(&[]), &map(&[]));
        assert_eq!(plan.base_updates, vec!["a.md"]);
    }

    #[test]
    fn independent_paths_get_independent_actions() {
        let plan = plan_sync(
            &map(&[("pull.md", "h1"), ("push.md", "h1")]),
            &map(&[("pull.md", "h1"), ("push.md", "h2")]),
            &map(&[("pull.md", "h9"), ("push.md", "h1")]),
        );
        assert_eq!(plan.pulls, vec!["pull.md"]);
        assert_eq!(plan.pushes, vec!["push.md"]);
    }

    /// The spec's scale guarantee: the manifest diff must stay fast at the
    /// brain sizes ninox already tests for (500 entries) — generous ceiling
    /// to catch a catastrophic regression, not to pin performance.
    #[test]
    fn plan_sync_scales_to_500_paths_within_ceiling() {
        let mut base = BTreeMap::new();
        let mut local = BTreeMap::new();
        let mut remote = BTreeMap::new();
        for i in 0..500 {
            base.insert(format!("notes/note{i}.md"), format!("h{i}"));
            // A third each: unchanged, locally edited, remotely edited.
            local.insert(format!("notes/note{i}.md"), if i % 3 == 1 { format!("l{i}") } else { format!("h{i}") });
            remote.insert(format!("notes/note{i}.md"), if i % 3 == 2 { format!("r{i}") } else { format!("h{i}") });
        }
        let start = std::time::Instant::now();
        let plan = plan_sync(&base, &local, &remote);
        let elapsed = start.elapsed();
        assert_eq!(plan.pushes.len() + plan.pulls.len(), 333);
        assert!(elapsed.as_millis() < 500, "diff of 500 paths took too long: {elapsed:?}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core brain_sync::diff 2>&1 | head -20`
Expected: compile errors — `plan_sync`/`SyncPlan` not defined.

- [ ] **Step 3: Implement**

Above the tests module:

```rust
/// The per-path actions a sync must take, computed by [`plan_sync`].
/// `pulls`/`delete_local` are safe on the read path (`pull_if_stale`):
/// they only touch files whose local content still matches base.
/// `resurrect_pulls`/`conflicts` overwrite local divergence and run only
/// in a full `sync()` (spec §3).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub pulls: Vec<String>,
    pub resurrect_pulls: Vec<String>,
    pub pushes: Vec<String>,
    pub delete_local: Vec<String>,
    pub delete_remote: Vec<String>,
    pub conflicts: Vec<String>,
    pub base_updates: Vec<String>,
}

/// Three-way diff of content hashes: `base` (last agreement with the
/// remote), `local` (files on disk), `remote` (manifest). Pure function —
/// all I/O happens in the engine that applies the plan.
pub fn plan_sync(
    base: &BTreeMap<String, String>,
    local: &BTreeMap<String, String>,
    remote: &BTreeMap<String, String>,
) -> SyncPlan {
    let mut plan = SyncPlan::default();
    let paths: BTreeSet<&String> = base.keys().chain(local.keys()).chain(remote.keys()).collect();
    for path in paths {
        let b = base.get(path);
        let l = local.get(path);
        let r = remote.get(path);
        if l == r {
            // Content agrees (or both absent); only the base may be stale.
            if b != l {
                plan.base_updates.push(path.clone());
            }
        } else if l == b {
            // Local untouched since last sync; remote moved.
            match r {
                Some(_) => plan.pulls.push(path.clone()),
                None => plan.delete_local.push(path.clone()),
            }
        } else if r == b {
            // Remote untouched since last sync; local moved.
            match l {
                Some(_) => plan.pushes.push(path.clone()),
                None => plan.delete_remote.push(path.clone()),
            }
        } else {
            // Both sides diverged from base.
            match (l, r) {
                (None, Some(_)) => plan.resurrect_pulls.push(path.clone()),
                (Some(_), None) => plan.pushes.push(path.clone()), // edit beats delete
                (Some(_), Some(_)) => plan.conflicts.push(path.clone()),
                (None, None) => unreachable!("l == r handled above"),
            }
        }
    }
    plan
}
```

In `brain_sync/mod.rs` add:

```rust
pub mod diff;

pub use diff::{plan_sync, SyncPlan};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync::diff`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core
git commit -m "feat(brain): add three-way sync diff covering the full spec matrix"
```

---

### Task 5: BrainSync engine — `pull_if_stale` + gitignore extension

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (engine struct, `SyncReport`, `pull_if_stale`, tests)
- Modify: `crates/ninox-core/src/brain.rs` (`ensure_gitignore` covers `.sync.toml`/`.sync-state.json`)

**Interfaces:**
- Consumes: `SyncToml` (T1), manifest/state/hash helpers (T2), `RemoteStore`/`InMemoryRemoteStore` (T3), `plan_sync` (T4).
- Produces:

```rust
pub struct BrainSync { /* brain_path: PathBuf, cfg: SyncToml, store: Arc<dyn RemoteStore> */ }
impl BrainSync {
    pub fn new(brain_path: PathBuf, cfg: SyncToml, store: Arc<dyn RemoteStore>) -> Self;
    pub async fn pull_if_stale(&self) -> Result<SyncReport>;
}
#[derive(Debug, Default)]
pub struct SyncReport {
    pub checked: bool,
    pub pulled: usize,
    pub pushed: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub conflicts: Vec<String>, // conflict-copy rel paths
}
impl SyncReport { pub fn changed_local(&self) -> bool; }
```

- [ ] **Step 1: Write the failing tests**

Append to `crates/ninox-core/src/brain_sync/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain_sync::manifest::{entry_key, now_unix, sha256_hex, Manifest, ManifestEntry, SyncState, MANIFEST_KEY};
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
```

Also add to `crates/ninox-core/src/brain.rs` tests:

```rust
    #[test]
    fn gitignore_covers_sync_files() {
        let (_brain, dir) = make_brain();
        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".index.db"));
        assert!(content.contains(".sync.toml"));
        assert!(content.contains(".sync-state.json"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core brain_sync::tests gitignore_covers 2>&1 | head -20`
Expected: compile errors (`BrainSync` undefined); `gitignore_covers_sync_files` FAILS on missing lines.

- [ ] **Step 3: Implement**

In `crates/ninox-core/src/brain.rs`, replace `ensure_gitignore` with a multi-entry version:

```rust
/// Ensure the brain's derived/local-only files are in its `.gitignore`:
/// the SQLite index plus the remote-sync marker and state files (see
/// `brain_sync`), none of which should ever be committed or synced.
fn ensure_gitignore(brain_path: &Path) -> Result<()> {
    let gi = brain_path.join(".gitignore");
    let wanted = [".index.db", ".sync.toml", ".sync-state.json"];
    let mut content = if gi.exists() { fs::read_to_string(&gi)? } else { String::new() };
    let mut changed = !gi.exists();
    for entry in wanted {
        if !content.lines().any(|l| l.trim() == entry) {
            content.push_str(entry);
            content.push('\n');
            changed = true;
        }
    }
    if changed {
        fs::write(&gi, content)?;
    }
    Ok(())
}
```

(Note the existing `open_creates_schema` test asserts `.index.db` is present — it still passes.)

In `crates/ninox-core/src/brain_sync/mod.rs`, above the tests module:

```rust
use crate::brain_sync::{
    diff::plan_sync,
    manifest::{entry_key, now_unix, scan_local, Manifest, SyncState, MANIFEST_KEY},
    store::{GetResponse, RemoteStore},
};
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
```

Adjust the top-of-file exports block so the final `mod.rs` header reads:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync gitignore`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core
git commit -m "feat(brain): add BrainSync engine with read-safe pull_if_stale"
```

---

### Task 6: Full `sync()` — push, CAS retry, conflict copies

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/mod.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5.
- Produces: `BrainSync::sync(&self) -> Result<SyncReport>` — full pull + push + conflict handling with bounded CAS retry.

- [ ] **Step 1: Write the failing tests**

Append inside the existing `mod tests` in `brain_sync/mod.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core brain_sync::tests::sync_ 2>&1 | head -20`
Expected: compile error — `sync` method not defined.

- [ ] **Step 3: Implement**

Add to `impl BrainSync` in `brain_sync/mod.rs` (below `pull_if_stale`), plus the imports `conflict_copy_rel, current_user, rfc3339, sha256_hex, ManifestEntry` from `manifest` and `PutOutcome` from `store`:

```rust
    /// Full sync (spec §3): pull, resolve conflicts into conflict copies,
    /// push, all under a bounded manifest compare-and-swap loop. Used by
    /// `ninox brain index` and `ninox brain sync`.
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync`
Expected: all PASS (pull_if_stale tests from Task 5 must still pass).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core
git commit -m "feat(brain): full sync with CAS retry and conflict copies"
```

---

### Task 7: S3RemoteStore + `BrainSync::for_brain` factory

**Files:**
- Create: `crates/ninox-core/src/brain_sync/s3.rs`
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (add `pub mod s3;`, `for_brain`)
- Modify: root `Cargo.toml` + `crates/ninox-core/Cargo.toml` (aws deps)

**Interfaces:**
- Consumes: `SyncToml` (T1), `RemoteStore` (T3).
- Produces: `S3RemoteStore::from_config(cfg: &SyncToml) -> Result<S3RemoteStore>` (async), `BrainSync::for_brain(brain_path: &Path) -> Result<Option<BrainSync>>` (async; `None` when no `.sync.toml`).

Real-S3 behavior can't be unit-tested; the store maps SDK responses to the trait and everything above it is tested against the fake. Keep this file thin and boring.

- [ ] **Step 1: Add dependencies**

Root `Cargo.toml` `[workspace.dependencies]`:

```toml
aws-config  = { version = "1", default-features = false, features = ["behavior-version-latest", "rt-tokio", "rustls", "sso"] }
aws-sdk-s3  = { version = "1", default-features = false, features = ["behavior-version-latest", "rt-tokio", "rustls"] }
```

`crates/ninox-core/Cargo.toml`:

```toml
aws-config = { workspace = true }
aws-sdk-s3 = { workspace = true }
```

Run `cargo check -p ninox-core` — if the feature names don't resolve on the current SDK version, drop `default-features = false` and use plain `aws-config = "1"` / `aws-sdk-s3 = "1"` (defaults include rustls).

- [ ] **Step 2: Write the failing test**

`for_brain` is testable without S3 (the `None` path and the invalid-URL path). Append to `brain_sync/mod.rs` tests:

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ninox-core for_brain 2>&1 | head -10`
Expected: compile error — `for_brain` not defined.

- [ ] **Step 4: Implement**

Create `crates/ninox-core/src/brain_sync/s3.rs`:

```rust
//! The real S3-compatible `RemoteStore`. Maps SDK responses onto the
//! trait's conditional semantics; every code path above this file is
//! tested against `InMemoryRemoteStore` instead. Auth is the standard AWS
//! credential chain (env vars, shared profiles, SSO) via `aws-config`.

use crate::brain_sync::{
    config::SyncToml,
    store::{GetResponse, PutOutcome, RemoteStore},
};
use anyhow::{Context, Result};
use async_trait::async_trait;

pub struct S3RemoteStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3RemoteStore {
    pub async fn from_config(cfg: &SyncToml) -> Result<Self> {
        let (bucket, prefix) = cfg.bucket_and_prefix()?;
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(region) = &cfg.region {
            loader = loader.region(aws_config::Region::new(region.clone()));
        }
        let sdk_config = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&sdk_config);
        if let Some(endpoint) = &cfg.endpoint {
            // S3-compatible stores (R2, MinIO) generally need path-style
            // addressing; harmless for AWS when an endpoint is set.
            builder = builder.endpoint_url(endpoint.clone()).force_path_style(true);
        }
        Ok(Self { client: aws_sdk_s3::Client::from_conf(builder.build()), bucket, prefix })
    }

    fn full_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }
}

/// True when an SDK error is an HTTP-level status we handle as a
/// conditional-request outcome (304 / 412) rather than a failure.
fn raw_status<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>) -> Option<u16>
where
    R: std::fmt::Debug,
    aws_sdk_s3::error::SdkError<E, R>: aws_smithy_runtime_api::client::result::CreateUnhandledError,
{
    use aws_smithy_runtime_api::http::Response;
    match err {
        aws_sdk_s3::error::SdkError::ServiceError(se) => Some(se.raw().status().as_u16()),
        aws_sdk_s3::error::SdkError::ResponseError(re) => Some(re.raw().status().as_u16()),
        _ => None,
    }
}

#[async_trait]
impl RemoteStore for S3RemoteStore {
    async fn get(&self, key: &str, if_none_match: Option<&str>) -> Result<GetResponse> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(self.full_key(key));
        if let Some(etag) = if_none_match {
            req = req.if_none_match(etag);
        }
        match req.send().await {
            Ok(out) => {
                let etag = out.e_tag().unwrap_or_default().to_string();
                let bytes = out.body.collect().await.context("read object body")?.into_bytes().to_vec();
                Ok(GetResponse::Found { bytes, etag })
            }
            Err(err) => {
                if matches!(raw_status(&err), Some(304)) {
                    return Ok(GetResponse::NotModified);
                }
                if let aws_sdk_s3::error::SdkError::ServiceError(se) = &err {
                    if se.err().is_no_such_key() {
                        return Ok(GetResponse::NotFound);
                    }
                    if matches!(raw_status(&err), Some(404)) {
                        return Ok(GetResponse::NotFound);
                    }
                }
                Err(err).with_context(|| format!("get s3://{}/{}", self.bucket, self.full_key(key)))
            }
        }
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<String> {
        let out = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .body(bytes.into())
            .send()
            .await
            .with_context(|| format!("put s3://{}/{}", self.bucket, self.full_key(key)))?;
        Ok(out.e_tag().unwrap_or_default().to_string())
    }

    async fn put_if_match(&self, key: &str, bytes: Vec<u8>, expected_etag: Option<&str>) -> Result<PutOutcome> {
        let mut req = self.client.put_object().bucket(&self.bucket).key(self.full_key(key)).body(bytes.into());
        match expected_etag {
            Some(etag) => req = req.if_match(etag),
            None => req = req.if_none_match("*"),
        }
        match req.send().await {
            Ok(out) => Ok(PutOutcome::Ok { etag: out.e_tag().unwrap_or_default().to_string() }),
            Err(err) if matches!(raw_status(&err), Some(412) | Some(409)) => Ok(PutOutcome::PreconditionFailed),
            Err(err) => Err(err).with_context(|| format!("put s3://{}/{}", self.bucket, self.full_key(key))),
        }
    }
}
```

(The exact `raw_status` helper signature may need adjusting to the SDK version — the requirement is: 304 on GET → `NotModified`, `NoSuchKey`/404 on GET → `NotFound`, 412 (or 409, which MinIO uses for concurrent writes) on conditional PUT → `PreconditionFailed`. If the generic bounds fight you, write two concrete helpers, one per operation error type.)

In `brain_sync/mod.rs`: add `pub mod s3;` to the module list and this to `impl BrainSync`:

```rust
    /// Build the sync engine for a brain directory, or `None` when the
    /// directory has no `.sync.toml` (a plain local brain).
    pub async fn for_brain(brain_path: &std::path::Path) -> Result<Option<BrainSync>> {
        let Some(cfg) = SyncToml::load(brain_path)? else {
            return Ok(None);
        };
        let store = s3::S3RemoteStore::from_config(&cfg).await?;
        Ok(Some(BrainSync::new(brain_path.to_path_buf(), cfg, Arc::new(store))))
    }
```

Note `bucket_and_prefix()` (called inside `from_config`) already rejects non-`s3://` URLs, which is what the `for_brain_rejects_non_s3_remote` test exercises.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox-core brain_sync && cargo build -p ninox-core`
Expected: PASS; full crate builds with the AWS SDK.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ninox-core
git commit -m "feat(brain): S3-compatible RemoteStore and for_brain factory"
```

---

### Task 8: `open_synced`, `ensure_sync_toml`, CLI wiring (index/query/show/sync)

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (`open_synced`)
- Modify: `crates/ninox-core/src/brain_sync/config.rs` (`ensure_sync_toml`)
- Modify: `crates/ninox-app/src/main.rs` (`run_brain`: sync-aware Index/Query/Show, new `Sync` action)

**Interfaces:**
- Consumes: `BrainSync` (T5–T7), `AppConfig::remote_config_for` (T1), `BrainIndex` (existing).
- Produces:

```rust
// brain_sync/mod.rs
pub async fn open_synced(brain_path: &Path, embedder: Option<&dyn crate::embeddings::Embedder>) -> Result<crate::BrainIndex>;
// brain_sync/config.rs
pub fn ensure_sync_toml(config: &crate::AppConfig, brain_path: &Path) -> Result<()>;
```

- New CLI: `ninox brain sync`.

- [ ] **Step 1: Write the failing tests**

Append to `brain_sync/mod.rs` tests:

```rust
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
```

Append to `brain_sync/config.rs` tests:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core open_synced ensure_sync_toml 2>&1 | head -15`
Expected: compile errors — functions not defined.

- [ ] **Step 3: Implement core functions**

In `brain_sync/mod.rs` (free function, after the `BrainSync` impl):

```rust
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
        Ok(Some(sync)) => match sync.pull_if_stale().await {
            Ok(report) if report.changed_local() => {
                if let Err(e) = brain.rebuild(embedder) {
                    tracing::warn!("brain: index rebuild after pull failed: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("brain: remote check failed, serving local copy: {e}"),
        },
        Err(e) => tracing::warn!("brain: remote unavailable, serving local copy: {e}"),
    }
    Ok(brain)
}
```

In `brain_sync/config.rs`:

```rust
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
```

Re-export both from `brain_sync/mod.rs`:

```rust
pub use config::{ensure_sync_toml, SyncToml, SYNC_TOML};
```

(and add `open_synced` to any explicit export list if one exists — it's a pub fn in `mod.rs`, so it's already `ninox_core::brain_sync::open_synced`.)

- [ ] **Step 4: Run core tests**

Run: `cargo test -p ninox-core brain_sync`
Expected: all PASS.

- [ ] **Step 5: Wire the CLI**

In `crates/ninox-app/src/main.rs`:

Add a `Sync` variant to `BrainAction` (after `Index`):

```rust
    /// Pull and push all changes to the brain's remote (no-op for a brain
    /// without a remote — see `ninox brain remote set`)
    Sync,
```

Rewrite the top of `run_brain` and the three read/write arms:

```rust
async fn run_brain(action: BrainAction, store: Arc<Store>) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let brain_path = config.resolved_brain_path();
    // First open of a config-declared remote catalogue materializes its
    // .sync.toml (a local-fs write only; the sync itself is lazy).
    if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, &brain_path) {
        tracing::warn!("brain: failed to materialize .sync.toml: {e}");
    }

    match action {
        BrainAction::Index => {
            run_remote_sync_if_configured(&brain_path).await;
            let brain = BrainIndex::open(&brain_path)?;
            let embedder = try_build_embedder();
            let stats = brain.rebuild(embedder.as_deref())?;
            println!(
                "indexed {} entries ({} embedded, {} cached)",
                stats.indexed, stats.embedded, stats.cached
            );
        }
        BrainAction::Sync => {
            match ninox_core::brain_sync::BrainSync::for_brain(&brain_path).await {
                Ok(None) => {
                    eprintln!("this brain has no remote — configure one with `ninox brain remote set s3://bucket/prefix`");
                    std::process::exit(1);
                }
                Ok(Some(sync)) => {
                    let report = sync.sync().await?;
                    print_sync_report(&report);
                    if report.changed_local() {
                        let brain = BrainIndex::open(&brain_path)?;
                        let embedder = try_build_embedder();
                        brain.rebuild(embedder.as_deref())?;
                    }
                }
                Err(e) => anyhow::bail!("brain remote unavailable: {e}"),
            }
        }
        BrainAction::Query { text, entry_type, tag } => {
            let embedder = if text.trim().is_empty() { None } else { try_build_embedder() };
            let brain = ninox_core::brain_sync::open_synced(&brain_path, embedder.as_deref()).await?;
            let filters = QueryFilters { entry_type, tag };
            let entries = brain.query(&text, embedder.as_deref(), filters)?;
            for entry in &entries {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Show { path } => {
            let brain = ninox_core::brain_sync::open_synced(&brain_path, None).await?;
            match brain.get(&path)? {
                Some(entry) => println!("{}", serde_json::to_string_pretty(&entry)?),
                None => {
                    eprintln!("entry not found: {path}");
                    std::process::exit(1);
                }
            }
        }
        // The Export, Import, and DiscoverRepos arms are NOT modified —
        // keep them exactly as they are today (archives and discovery act
        // on local files; a following `ninox brain index` syncs them).
    }

    Ok(())
}

/// `ninox brain index` on a remote-backed brain: full sync BEFORE the
/// rebuild so pulled entries land in the index (spec: pull → resolve →
/// push → rebuild). Failures degrade to local-only indexing — the index
/// step must keep working offline.
async fn run_remote_sync_if_configured(brain_path: &std::path::Path) {
    match ninox_core::brain_sync::BrainSync::for_brain(brain_path).await {
        Ok(None) => {}
        Ok(Some(sync)) => match sync.sync().await {
            Ok(report) => print_sync_report(&report),
            Err(e) => eprintln!("brain sync failed (continuing with local index): {e}"),
        },
        Err(e) => eprintln!("brain remote unavailable (continuing local-only): {e}"),
    }
}

fn print_sync_report(report: &ninox_core::brain_sync::SyncReport) {
    println!(
        "synced with remote: pulled {}, pushed {}, deleted {} local / {} remote, {} conflict{}",
        report.pulled,
        report.pushed,
        report.deleted_local,
        report.deleted_remote,
        report.conflicts.len(),
        if report.conflicts.len() == 1 { "" } else { "s" },
    );
    for rel in &report.conflicts {
        eprintln!("  conflict copy kept: {rel}");
    }
}
```

- [ ] **Step 6: Verify build and full test suite**

Run: `cargo test -p ninox-core && cargo test -p ninox-app 2>&1 | tail -3 && cargo build -p ninox-app`
Expected: PASS / builds.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-core crates/ninox-app
git commit -m "feat(brain): sync-aware index/query/show and ninox brain sync command"
```

---

### Task 9: CLI `brain remote set / status / unset`

**Files:**
- Modify: `crates/ninox-core/src/brain_sync/mod.rs` (`remote_status`, `RemoteStatus`)
- Modify: `crates/ninox-app/src/main.rs` (`RemoteAction` subcommand + handling)

**Interfaces:**
- Consumes: `SyncToml`, `SyncState`, `scan_local` (T1/T2), `BrainSync` (T5–T7).
- Produces:

```rust
// brain_sync/mod.rs — offline status snapshot, no network
pub struct RemoteStatus {
    pub remote: String,
    pub cache_ttl_secs: u64,
    pub generation: u64,
    pub last_check_unix: u64,
    pub pending_pushes: Vec<String>,  // local != base
    pub conflict_files: Vec<String>,  // rel paths containing ".conflict-"
}
pub fn remote_status(brain_path: &Path) -> Result<Option<RemoteStatus>>;
```

- New CLI: `ninox brain remote set <url> [--endpoint] [--region] [--ttl]`, `ninox brain remote status`, `ninox brain remote unset`.

- [ ] **Step 1: Write the failing tests**

Append to `brain_sync/mod.rs` tests:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core remote_status 2>&1 | head -10`
Expected: compile error — `remote_status` not defined.

- [ ] **Step 3: Implement core**

In `brain_sync/mod.rs`:

```rust
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
```

- [ ] **Step 4: Wire the CLI**

In `crates/ninox-app/src/main.rs`, add to `BrainAction` (after `Sync`):

```rust
    /// Manage this brain's remote backing store
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
```

New subcommand enum (next to `BrainAction`):

```rust
#[derive(Subcommand)]
enum RemoteAction {
    /// Attach an S3-compatible remote and run an initial sync
    Set {
        /// Remote URL, e.g. s3://team-brains/main
        url: String,
        /// Custom endpoint for S3-compatible stores (R2, MinIO)
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        region: Option<String>,
        /// Freshness-check cache TTL in seconds (0 = check every lookup)
        #[arg(long, default_value_t = 0)]
        ttl: u64,
    },
    /// Show remote, last sync, pending pushes, and live conflicts (offline)
    Status,
    /// Detach from the remote; the local copy stays a normal brain
    Unset,
}
```

New arm in `run_brain`'s match:

```rust
        BrainAction::Remote { action } => match action {
            RemoteAction::Set { url, endpoint, region, ttl } => {
                let cfg = ninox_core::brain_sync::SyncToml {
                    remote: url,
                    endpoint,
                    region,
                    cache_ttl_secs: ttl,
                };
                cfg.save(&brain_path)?;
                println!("remote set to {} for {}", cfg.remote, brain_path.display());
                match ninox_core::brain_sync::BrainSync::for_brain(&brain_path).await? {
                    Some(sync) => {
                        let report = sync.sync().await?;
                        print_sync_report(&report);
                        let brain = BrainIndex::open(&brain_path)?;
                        let embedder = try_build_embedder();
                        brain.rebuild(embedder.as_deref())?;
                    }
                    None => unreachable!(".sync.toml was just written"),
                }
            }
            RemoteAction::Status => match ninox_core::brain_sync::remote_status(&brain_path)? {
                None => {
                    println!("no remote configured for {}", brain_path.display());
                }
                Some(s) => {
                    println!("remote:          {}", s.remote);
                    println!("cache ttl:       {}s", s.cache_ttl_secs);
                    println!("last generation: {}", s.generation);
                    println!(
                        "last check:      {}",
                        if s.last_check_unix == 0 { "never".to_string() } else { ninox_core::brain_sync::manifest::rfc3339(s.last_check_unix) }
                    );
                    println!("pending pushes:  {}", s.pending_pushes.len());
                    for rel in &s.pending_pushes {
                        println!("  {rel}");
                    }
                    println!("live conflicts:  {}", s.conflict_files.len());
                    for rel in &s.conflict_files {
                        println!("  {rel}");
                    }
                }
            },
            RemoteAction::Unset => {
                let removed_cfg = std::fs::remove_file(brain_path.join(ninox_core::brain_sync::SYNC_TOML)).is_ok();
                let _ = std::fs::remove_file(brain_path.join(ninox_core::brain_sync::SYNC_STATE));
                if removed_cfg {
                    println!("remote detached; {} is a plain local brain again", brain_path.display());
                } else {
                    println!("no remote was configured for {}", brain_path.display());
                }
            }
        },
```

Note: `remote set` on an existing remote intentionally requires no confirmation — the subsequent sync's three-way diff handles overlap safely (existing local files that also exist remotely with different content become conflict copies, never silent overwrites).

- [ ] **Step 5: Run tests and build**

Run: `cargo test -p ninox-core remote_status && cargo build -p ninox-app && cargo run -p ninox-app -- brain remote status`
Expected: tests PASS; the CLI prints `no remote configured for <default brain path>` (do NOT run `remote set` against a real bucket here).

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core crates/ninox-app
git commit -m "feat(brain): remote set/status/unset commands"
```

---

### Task 10: Server route integration

**Files:**
- Modify: `crates/ninox-core/src/brain.rs` (add `BrainIndex::path()` accessor)
- Modify: `crates/ninox-server/src/routes/brain.rs` (optional `BrainSync` in state; pull-if-stale on reads, full sync on `POST /index`)
- Modify: `crates/ninox-server/src/server.rs` (construct `BrainSync` at startup)

**Interfaces:**
- Consumes: `BrainSync::{for_brain, pull_if_stale, sync}`, `SyncReport::changed_local` (T5–T7).
- Produces: `BrainIndex::path(&self) -> &Path`; `brain_router(brain: Arc<BrainIndex>, embedder: Option<Arc<dyn Embedder>>, sync: Option<Arc<BrainSync>>) -> Router` (signature change).

- [ ] **Step 1: Write the failing test**

Add a tests module at the bottom of `crates/ninox-server/src/routes/brain.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query as AxQuery, State};
    use ninox_core::brain_sync::{
        manifest::{entry_key, sha256_hex, Manifest, ManifestEntry, MANIFEST_KEY},
        BrainSync, InMemoryRemoteStore, SyncToml,
    };
    use tempfile::tempdir;

    #[tokio::test]
    async fn query_pulls_fresh_remote_entries_before_answering() {
        let dir = tempdir().unwrap();
        let brain = Arc::new(BrainIndex::open(dir.path()).unwrap());
        brain.rebuild(None).unwrap();

        // Remote already has an entry this server has never seen.
        let store = Arc::new(InMemoryRemoteStore::default());
        let body = "---\nname: Remote Note\n---\nWritten by a teammate about widgetronic.";
        let sha = sha256_hex(body.as_bytes());
        store.put(&entry_key("notes/remote.md", &sha), body.as_bytes().to_vec()).await.unwrap();
        let mut manifest = Manifest::empty();
        manifest.generation = 1;
        manifest.entries.insert(
            "notes/remote.md".into(),
            ManifestEntry { sha256: sha, size: body.len() as u64, updated_by: "teammate".into(), updated_at: "2026-07-22T00:00:00Z".into() },
        );
        store.put(MANIFEST_KEY, manifest.to_bytes().unwrap()).await.unwrap();

        let cfg = SyncToml { remote: "s3://b/p".into(), endpoint: None, region: None, cache_ttl_secs: 0 };
        let sync = Arc::new(BrainSync::new(dir.path().to_path_buf(), cfg, store));
        let state = BrainState { brain, embedder: None, sync: Some(sync) };

        let params = QueryParams { q: Some("widgetronic".into()), entry_type: None, tag: None };
        let Json(entries) = query_entries(State(state), AxQuery(params)).await.unwrap();
        assert_eq!(entries.len(), 1, "the freshly-pulled remote entry should be queryable");
        assert_eq!(entries[0].id, "notes/remote.md");
    }

    #[tokio::test]
    async fn query_serves_local_when_remote_is_down() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("local.md"), "---\nname: Local\n---\nAbout widgetronic.").unwrap();
        let brain = Arc::new(BrainIndex::open(dir.path()).unwrap());
        brain.rebuild(None).unwrap();

        let store = Arc::new(InMemoryRemoteStore::default());
        store.fail_all.store(true, std::sync::atomic::Ordering::SeqCst);
        let cfg = SyncToml { remote: "s3://b/p".into(), endpoint: None, region: None, cache_ttl_secs: 0 };
        let sync = Arc::new(BrainSync::new(dir.path().to_path_buf(), cfg, store));
        let state = BrainState { brain, embedder: None, sync: Some(sync) };

        let params = QueryParams { q: Some("widgetronic".into()), entry_type: None, tag: None };
        let Json(entries) = query_entries(State(state), AxQuery(params)).await.unwrap();
        assert_eq!(entries.len(), 1, "a dead remote must never fail a query");
    }
}
```

(`ninox-server/Cargo.toml` needs `tempfile = "3"` under `[dev-dependencies]` if not already present.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-server brain 2>&1 | head -15`
Expected: compile errors — `sync` field missing on `BrainState`.

- [ ] **Step 3: Implement**

In `crates/ninox-core/src/brain.rs`, add to `impl BrainIndex`:

```rust
    /// The brain directory this index was opened on.
    pub fn path(&self) -> &Path {
        &self.brain_path
    }
```

In `crates/ninox-server/src/routes/brain.rs`:

```rust
use ninox_core::{brain_sync::BrainSync, embeddings::Embedder, BrainEntry, BrainIndex, QueryFilters};

#[derive(Clone)]
pub struct BrainState {
    pub brain: Arc<BrainIndex>,
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Present when the brain is remote-backed; reads freshen via
    /// `pull_if_stale`, `POST /index` runs a full sync.
    pub sync: Option<Arc<BrainSync>>,
}

pub fn brain_router(
    brain: Arc<BrainIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    sync: Option<Arc<BrainSync>>,
) -> Router {
    Router::new()
        .route("/index", post(rebuild_index))
        .route("/query", get(query_entries))
        .route("/entry/*path", get(get_entry))
        .with_state(BrainState { brain, embedder, sync })
}

/// Freshness check before a read (spec §4): pull-if-stale, rebuild on
/// change, degrade to the local copy on any remote failure.
async fn freshen(state: &BrainState) {
    let Some(sync) = &state.sync else { return };
    match sync.pull_if_stale().await {
        Ok(report) if report.changed_local() => {
            if let Err(e) = state.brain.rebuild(state.embedder.as_deref()) {
                tracing::warn!("brain: rebuild after pull failed: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("brain: remote check failed, serving local copy: {e}"),
    }
}
```

Call `freshen(&state).await;` as the first line of `query_entries` and `get_entry`. In `rebuild_index`, run the full sync first:

```rust
async fn rebuild_index(State(state): State<BrainState>) -> Result<Json<IndexResponse>, StatusCode> {
    if let Some(sync) = &state.sync {
        if let Err(e) = sync.sync().await {
            tracing::warn!("brain: remote sync failed (continuing with local rebuild): {e}");
        }
    }
    match state.brain.rebuild(state.embedder.as_deref()) {
        // ... unchanged
    }
}
```

In `crates/ninox-server/src/server.rs`, inside `start(...)` before building the router:

```rust
    let sync = match ninox_core::brain_sync::BrainSync::for_brain(brain.path()).await {
        Ok(Some(s)) => Some(Arc::new(s)),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("brain: remote unavailable, serving local-only: {e}");
            None
        }
    };
```

and change the nest line to `.nest("/api/brain", brain_router(brain, embedder, sync))`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-server && cargo test -p ninox-core brain && cargo build -p ninox-app`
Expected: all PASS; workspace still builds (the only `brain_router` caller is `server.rs`, already updated).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core crates/ninox-server
git commit -m "feat(server): freshness-check remote brains on lookup routes"
```

---

### Task 11: Documentation + final verification

**Files:**
- Modify: `docs/BRAIN.md` (new "Remote brains" section + CLI table rows)
- Modify: `.claude/skills/brain/SKILL.md` (one-paragraph note)

**Interfaces:** none — docs only.

- [ ] **Step 1: Update `docs/BRAIN.md`**

Add rows to the CLI block:

```
ninox brain sync                     pull + push all changes to the brain's remote
ninox brain remote set <url>         attach an S3-compatible remote (--endpoint, --region, --ttl)
ninox brain remote status            remote URL, last sync, pending pushes, live conflicts
ninox brain remote unset             detach; the local copy stays a normal brain
```

Add a section after "Configuration":

```markdown
## Remote brains

A brain can be shared by a team. The canonical copy lives in S3 (or any
S3-compatible store — R2, MinIO); every machine keeps a full local mirror,
so queries stay local-speed. A brain directory is remote-backed when it
contains a `.sync.toml` (written by `ninox brain remote set`, or
materialized automatically from a `[[brain.catalogues]]` entry with a
`remote` field). Auth uses the standard AWS credential chain.

    [[brain.catalogues]]
    name = "team"
    path = "~/.config/ninox/brains/team"
    remote = "s3://team-brains/main"
    # endpoint = "https://<account>.r2.cloudflarestorage.com"
    # region = "eu-west-1"
    # cache_ttl_secs = 0   # 0 = freshness-check every lookup

How it stays in sync:

- **Lookups** (`query`, `show`, and the server's brain routes) first make
  one conditional GET of the remote's `manifest.json`. Unchanged manifest
  = one cheap 304 and zero downloads. Changed = only the changed entries
  are pulled, and only ones you haven't edited locally — a read never
  clobbers unpushed work. Raise `cache_ttl_secs` to trade freshness for
  latency. If the remote is unreachable, the local mirror answers with a
  warning — a query is never blocked by S3.
- **Writes** ride the habit you already have: `ninox brain index` pulls,
  resolves, pushes, then rebuilds. `ninox brain sync` does the same
  without a rebuild being the goal.
- **Conflicts** (two people edited the same entry since they last agreed)
  never lose knowledge: the remote version takes the entry's path and
  your version is kept — and shared — as `<entry>.conflict-<user>-<ts>.md`
  until someone merges the two and deletes it.

`.index.db`, `.sync.toml`, and `.sync-state.json` never leave the
machine; each mirror rebuilds its own index. Entry objects in the bucket
are immutable (`entries/<path>@<hash>`); `manifest.json` alone decides
what's current, and concurrent pushes are serialized by compare-and-swap
on it. Full design: `docs/superpowers/specs/2026-07-22-remote-brain-design.md`.
```

- [ ] **Step 2: Update `.claude/skills/brain/SKILL.md`**

After the "Before you finish" section's `ninox brain index` block, add:

```markdown
If this brain is remote-backed (team-shared), `ninox brain index` also
pushes your new entries to the team and pulls theirs — nothing extra to
do. If it reports a conflict copy (`*.conflict-*.md`), merge it into the
canonical entry and delete the copy when you're confident.
```

- [ ] **Step 3: Full workspace verification**

Run: `cargo test --workspace 2>&1 | tail -20 && cargo clippy --workspace 2>&1 | tail -5`
Expected: all tests PASS, no new clippy warnings in touched files.

- [ ] **Step 4: Commit**

```bash
git add docs/BRAIN.md .claude/skills/brain/SKILL.md
git commit -m "docs(brain): document remote brains and sync behavior"
```
