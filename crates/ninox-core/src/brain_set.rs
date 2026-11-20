//! A session's brain set: the catalogues it reads from and the one it
//! writes to.
//!
//! Persisted on the session row (`Session::brain_reads` /
//! `Session::brain_write`) and exported to the session as `NINOX_BRAIN`
//! (the write path) plus `NINOX_BRAIN_READS` (a JSON array of read paths).
//! The env vars are hints for the harness only: every `ninox brain`
//! command inside a session re-resolves the set from its row, so a
//! session can't widen what it reads or where it writes by editing env.
//!
//! Read-only-ness and display names are never stored here — they come
//! from config (`CatalogueRef::access` / `name`) at the moment of use.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, CatalogueRef};
use crate::types::Session;

pub const READS_ENV: &str = "NINOX_BRAIN_READS";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrainSet {
    /// Ordered, deduplicated; always contains `write`.
    pub reads: Vec<PathBuf>,
    pub write: PathBuf,
    /// `true` when this set was chosen to restrict the session (a limit);
    /// `false` for the default of every enabled brain, which is re-evaluated
    /// against the live config each time. A set that deserializes without
    /// the field counts as limited, so nothing widens silently.
    #[serde(default = "limited_by_default")]
    pub limited: bool,
}

fn limited_by_default() -> bool {
    true
}

/// One member of a resolved set, with its config-derived identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub catalogue: CatalogueRef,
    pub writable:  bool,
}

impl Member {
    pub fn name(&self) -> &str {
        &self.catalogue.name
    }
}

pub fn is_writable(c: &CatalogueRef) -> bool {
    c.access != "read_only"
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let norm = |p: &Path| p.components().collect::<PathBuf>();
    if norm(a) == norm(b) {
        return true;
    }
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Every catalogue a set may draw from: the default (at its configured
/// location, not `NINOX_BRAIN`) followed by `[[brain.catalogues]]`.
pub fn pool(config: &AppConfig) -> Vec<CatalogueRef> {
    let mut pool = config.catalogue_options();
    if let Some(default) = pool.first_mut() {
        default.path = config.configured_brain_path();
    }
    pool
}

/// [`pool`] as seen by a caller outside any session: "default" is
/// `resolved_brain_path`, so a terminal-level `NINOX_BRAIN` override stands
/// in for the default brain rather than being added beside it.
fn human_pool(config: &AppConfig) -> Vec<CatalogueRef> {
    let mut pool = config.catalogue_options();
    if let Some(default) = pool.first_mut() {
        default.path = config.resolved_brain_path();
    }
    pool
}

fn find_in<'a>(pool: &'a [CatalogueRef], path: &Path) -> Option<&'a CatalogueRef> {
    pool.iter().find(|c| same_path(&c.path, path))
}

impl BrainSet {
    fn new(mut reads: Vec<PathBuf>, write: PathBuf) -> Self {
        if !reads.iter().any(|r| same_path(r, &write)) {
            reads.insert(0, write.clone());
        }
        let mut deduped: Vec<PathBuf> = Vec::with_capacity(reads.len());
        for r in reads {
            if !deduped.iter().any(|d| same_path(d, &r)) {
                deduped.push(r);
            }
        }
        Self { reads: deduped, write, limited: true }
    }

    /// The set a session gets when nothing was chosen: every source with
    /// "Include in search" on, writing to the default catalogue
    /// (`resolved_brain_path`, so a terminal-level `NINOX_BRAIN` override
    /// still redirects writes as it always has).
    pub fn default_for(config: &AppConfig) -> Self {
        let reads = human_pool(config).into_iter().filter(|c| c.enabled).map(|c| c.path).collect();
        Self { limited: false, ..Self::new(reads, config.resolved_brain_path()) }
    }

    /// The unlimited set for a live session: every enabled brain, writing to
    /// the default brain at its configured location. Unlike [`default_for`]
    /// it never reads `NINOX_BRAIN`, so a session can't redirect where it
    /// writes by editing its environment.
    pub fn session_default(config: &AppConfig) -> Self {
        let reads = pool(config).into_iter().filter(|c| c.enabled).map(|c| c.path).collect();
        Self { limited: false, ..Self::new(reads, config.configured_brain_path()) }
    }

    /// What goes on a session row: nothing for the default (every enabled
    /// brain, resolved live), the explicit lists for a limit.
    pub fn stored_reads(&self) -> Vec<String> {
        if !self.limited {
            return Vec::new();
        }
        self.reads.iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }

    pub fn stored_write(&self) -> Option<String> {
        self.limited.then(|| self.write_str())
    }

    /// An explicit choice that amounts to the default (every enabled brain,
    /// writing to the default brain) is the default, not a limit.
    fn unlimited_if_default(self, default: Self) -> Self {
        let same_reads = self.reads.len() == default.reads.len() && self.reads.iter().all(|r| default.reads.iter().any(|d| same_path(d, r)));
        if same_reads && same_path(&self.write, &default.write) {
            default
        } else {
            self
        }
    }

    /// For a process with no session: shrinks the default set to the paths in
    /// `NINOX_BRAIN_READS`, when set (the env-scrubbed harvest subprocess
    /// gets the harvested session's set this way). Only ever narrows — an
    /// env value can't add a brain the default set doesn't already have.
    fn narrowed_by_env(self, config: &AppConfig) -> Self {
        let Ok(raw) = std::env::var(READS_ENV) else { return self };
        let wanted = parse_reads_env(&raw);
        if wanted.is_empty() {
            return self;
        }
        let reads: Vec<PathBuf> = self.reads.iter().filter(|r| wanted.iter().any(|w| same_path(w, r))).cloned().collect();
        if reads.is_empty() {
            return self;
        }
        let write = config.resolved_brain_path();
        let write = if reads.iter().any(|r| same_path(r, &write)) { write } else { reads[0].clone() };
        Self::new(reads, write)
    }

    /// A single-catalogue set — what every session had before brain sets.
    pub fn single(path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        Self::new(vec![p.clone()], p)
    }

    /// The set recorded on a session row. A row with no write catalogue
    /// (filed before catalogue tracking) falls back to [`default_for`]; a
    /// row with a write catalogue but no reads (filed before brain sets,
    /// i.e. migrated from `catalogue_path`) reads only that catalogue.
    pub fn from_session(session: &Session, config: &AppConfig) -> Self {
        match session.brain_write.as_deref().filter(|w| !w.is_empty()) {
            None => Self::session_default(config),
            Some(w) if Self::is_old_default_row(session, w, config) => Self::session_default(config),
            Some(w) => Self::new(session.brain_reads.iter().map(PathBuf::from).collect(), PathBuf::from(w)),
        }
    }

    /// Before limits existed every session stored `reads=[default]
    /// write=default`, because that was the default, not a choice. Such a
    /// row counts as unlimited, decided at read time so the stored row (and
    /// a downgrade) is untouched. Any other stored set is a real choice.
    fn is_old_default_row(session: &Session, write: &str, config: &AppConfig) -> bool {
        let is_default = |p: &Path| same_path(p, &config.configured_brain_path()) || same_path(p, &config.resolved_brain_path());
        is_default(Path::new(write)) && !session.brain_reads.is_empty() && session.brain_reads.iter().all(|r| is_default(Path::new(r)))
    }

    pub fn apply_to(&self, session: &mut Session) {
        session.brain_reads = self.stored_reads();
        session.brain_write = self.stored_write();
    }

    pub fn write_str(&self) -> String {
        self.write.to_string_lossy().into_owned()
    }

    /// `NINOX_BRAIN_READS`'s value: a JSON array of paths (paths may
    /// legitimately contain `:`, so no PATH-style joining).
    pub fn reads_env_value(&self) -> String {
        let reads: Vec<String> = self.reads.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        serde_json::to_string(&reads).unwrap_or_else(|_| "[]".into())
    }

    /// Each read catalogue matched to its config entry, or a synthetic
    /// writable entry named after its directory when config doesn't know
    /// the path. Names are unique within the result.
    pub fn members(&self, config: &AppConfig) -> Vec<Member> {
        let pool = pool(config);
        let mut out: Vec<Member> = Vec::new();
        for path in &self.reads {
            let mut catalogue = find_in(&pool, path).cloned().unwrap_or_else(|| synthetic(path));
            let base = catalogue.name.clone();
            let mut n = 2;
            while out.iter().any(|m| m.catalogue.name == catalogue.name) {
                catalogue.name = format!("{base}-{n}");
                n += 1;
            }
            let writable = is_writable(&catalogue);
            out.push(Member { catalogue, writable });
        }
        out
    }

    pub fn write_member(&self, config: &AppConfig) -> Member {
        // A deserialized set may name a write outside its reads, or a path
        // may stop canonicalizing the same way: never panic over it.
        self.members(config)
            .into_iter()
            .find(|m| same_path(&m.catalogue.path, &self.write))
            .unwrap_or_else(|| {
                let catalogue = find_in(&pool(config), &self.write).cloned().unwrap_or_else(|| synthetic(&self.write));
                Member { writable: is_writable(&catalogue), catalogue }
            })
    }

    /// The catalogue a write should land in: `target` (a member name or
    /// path) or else the set's write catalogue. Refuses read-only sources
    /// and anything outside the set.
    pub fn authorize_write(&self, config: &AppConfig, target: Option<&str>) -> Result<Member> {
        let member = match target {
            None => self.write_member(config),
            Some(t) => match lookup(&self.members(config), t) {
                Some(m) => m,
                None => bail!(
                    "brain catalogue '{t}' is not in this session's brain set (reads: {})",
                    self.names(config).join(", ")
                ),
            },
        };
        if !member.writable {
            bail!("brain catalogue '{}' is read-only; writes are refused", member.name());
        }
        Ok(member)
    }

    /// The members a read should search: all of them, or just `target`.
    pub fn authorize_read(&self, config: &AppConfig, target: Option<&str>) -> Result<Vec<Member>> {
        let members = self.members(config);
        match target {
            None => Ok(members),
            Some(t) => match lookup(&members, t) {
                Some(m) => Ok(vec![m]),
                None => bail!(
                    "brain catalogue '{t}' is not in this session's brain set (reads: {})",
                    members.iter().map(|m| m.name().to_string()).collect::<Vec<_>>().join(", ")
                ),
            },
        }
    }

    pub fn names(&self, config: &AppConfig) -> Vec<String> {
        self.members(config).into_iter().map(|m| m.catalogue.name).collect()
    }

    /// `"reads A, B, C · writes A"`.
    pub fn summary(&self, config: &AppConfig) -> String {
        format!("reads {} · writes {}", self.names(config).join(", "), self.write_member(config).name())
    }

    /// A child's set. `reads`/`write` are catalogue names (or paths) the
    /// spawner asked for; empty / `None` inherit. Every requested read
    /// must already be one of this set's reads — a child never gets more
    /// than its parent — and the write must be a writable requested read.
    pub fn narrow(&self, config: &AppConfig, reads: &[String], write: Option<&str>) -> Result<Self> {
        let set = choose(&self.members(config), self, reads, write, "the parent's brain set")?;
        // A child of an unlimited parent that asks for everything is unlimited
        // too; a child of a limited parent never widens past that limit.
        Ok(if self.limited { set } else { set.unlimited_if_default(Self::session_default(config)) })
    }

    /// A top-level set chosen by a human (no parent): any configured
    /// catalogue may be picked; nothing picked gives [`default_for`].
    pub fn select(config: &AppConfig, reads: &[String], write: Option<&str>) -> Result<Self> {
        let default = Self::default_for(config);
        let mut pool: Vec<Member> =
            human_pool(config).into_iter().map(|c| Member { writable: is_writable(&c), catalogue: c }).collect();
        for m in default.members(config) {
            if !pool.iter().any(|p| same_path(&p.catalogue.path, &m.catalogue.path)) {
                pool.push(m);
            }
        }
        Ok(choose(&pool, &default, reads, write, "the configured brain catalogues")?.unlimited_if_default(Self::default_for(config)))
    }
}

/// `"Brains: all"` for a session with no limit, else
/// `"Brains: limited to A, B · writes A"`. Read-only brains are never write
/// targets whatever the limit.
pub fn session_brains_line(session: &Session, config: &AppConfig) -> String {
    let set = BrainSet::from_session(session, config);
    if !set.limited {
        return "Brains: all".to_string();
    }
    format!("Brains: limited to {} · writes {}", set.names(config).join(", "), set.write_member(config).name())
}

fn synthetic(path: &Path) -> CatalogueRef {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    CatalogueRef {
        name,
        path: path.to_path_buf(),
        remote: None,
        endpoint: None,
        region: None,
        cache_ttl_secs: None,
        enabled: true,
        cadence: "manual".into(),
        access: "read_write".into(),
        write_rules_override: None,
    }
}

fn lookup(members: &[Member], key: &str) -> Option<Member> {
    members
        .iter()
        .find(|m| m.catalogue.name == key)
        .or_else(|| members.iter().find(|m| same_path(&m.catalogue.path, Path::new(key))))
        .cloned()
}

fn choose(pool: &[Member], inherit: &BrainSet, reads: &[String], write: Option<&str>, scope: &str) -> Result<BrainSet> {
    if reads.is_empty() && write.is_none() {
        return Ok(inherit.clone());
    }
    let not_found = |k: &str| anyhow::anyhow!("brain catalogue '{k}' is not in {scope}");
    let chosen: Vec<Member> = if reads.is_empty() {
        inherit
            .reads
            .iter()
            .filter_map(|p| pool.iter().find(|m| same_path(&m.catalogue.path, p)).cloned())
            .collect()
    } else {
        reads.iter().map(|k| lookup(pool, k).ok_or_else(|| not_found(k))).collect::<Result<_>>()?
    };
    let write_member = match write {
        Some(w) => {
            let m = lookup(&chosen, w).ok_or_else(|| {
                if lookup(pool, w).is_some() {
                    anyhow::anyhow!("--brain-write '{w}' must also be one of the brains this session reads")
                } else {
                    not_found(w)
                }
            })?;
            if !m.writable {
                bail!("brain catalogue '{w}' is read-only and can't be the write catalogue");
            }
            m
        }
        None => {
            if let Some(m) = chosen.iter().find(|m| same_path(&m.catalogue.path, &inherit.write)) {
                m.clone()
            } else if let Some(m) = chosen.iter().find(|m| m.writable) {
                m.clone()
            } else {
                bail!("none of the selected brains is writable; pick a writable one with --brain-write")
            }
        }
    };
    Ok(BrainSet::new(chosen.into_iter().map(|m| m.catalogue.path).collect(), write_member.catalogue.path))
}

/// Parses `NINOX_BRAIN_READS` defensively: anything that isn't a JSON
/// array of non-empty strings yields an empty list rather than an error,
/// since the value is only a hint.
pub fn parse_reads_env(value: &str) -> Vec<PathBuf> {
    serde_json::from_str::<Vec<serde_json::Value>>(value)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(PathBuf::from))
        .collect()
}

/// The set the current process acts with, and the session it belongs to.
/// Inside a live session that is the session row's set — `NINOX_BRAIN`
/// and `NINOX_BRAIN_READS` are ignored so env edits can't widen it. With
/// no session (a human at a terminal, or the env-scrubbed harvest
/// subprocess) it is [`BrainSet::default_for`], whose write honors
/// `NINOX_BRAIN`. A process that claims a session (`NINOX_SESSION`) but
/// can't be verified as one — e.g. detached from its pane — is refused
/// rather than treated as the unrestricted human.
pub fn resolve_for_caller(
    store: &crate::store::Store,
    pane_session_id: Option<&str>,
    config: &AppConfig,
) -> Result<(BrainSet, Option<String>)> {
    match verified_caller_row(store, pane_session_id)? {
        Some(s) => Ok((BrainSet::from_session(&s, config), Some(s.id))),
        None => Ok((BrainSet::default_for(config).narrowed_by_env(config), None)),
    }
}

fn verified_caller_row(store: &crate::store::Store, pane_session_id: Option<&str>) -> Result<Option<Session>> {
    let id = crate::orchestrator_auth::resolve_live_caller_session_id(store, pane_session_id);
    verified_row(store, id.as_deref())
}

fn verified_row(store: &crate::store::Store, caller_session_id: Option<&str>) -> Result<Option<Session>> {
    if let Some(row) = caller_session_id.and_then(|id| store.get_session(id).ok().flatten()) {
        return Ok(Some(row));
    }
    // Test binaries inherit the developer's own session env when run from
    // inside a ninox session; the claim check is exercised directly instead.
    let in_test = cfg!(test) || (cfg!(feature = "test-isolation") && crate::tmux::is_test_binary());
    if !in_test {
        refuse_unverified_claim(std::env::var("NINOX_SESSION").ok().as_deref())?;
    }
    Ok(None)
}

fn refuse_unverified_claim(claimed: Option<&str>) -> Result<()> {
    match claimed.filter(|c| !c.is_empty()) {
        Some(c) => bail!(
            "this process claims session '{c}' (NINOX_SESSION) but can't be verified as running inside it, \
             so its brain set can't be resolved; run the command from the session's own shell"
        ),
        None => Ok(()),
    }
}

/// The set a session spawned by the caller gets: `reads`/`write` narrowed
/// from the caller's own set when the caller is a session, else chosen
/// freely. `caller_session_id` must be the pane-verified id from
/// `orchestrator_auth::resolve_live_caller_session_id`, never an env var.
pub fn resolve_child(
    store: &crate::store::Store,
    caller_session_id: Option<&str>,
    config: &AppConfig,
    reads: &[String],
    write: Option<&str>,
) -> Result<BrainSet> {
    match verified_row(store, caller_session_id)? {
        Some(p) => BrainSet::from_session(&p, config).narrow(config, reads, write),
        None => BrainSet::select(config, reads, write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_isolation::isolate;

    fn cat(name: &str, path: &Path, enabled: bool, access: &str) -> CatalogueRef {
        CatalogueRef {
            name: name.into(),
            path: path.to_path_buf(),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled,
            cadence: "manual".into(),
            access: access.into(),
            write_rules_override: None,
        }
    }

    struct Fixture {
        config: AppConfig,
        default: PathBuf,
        a: PathBuf,
        b: PathBuf,
        c: PathBuf,
        _dir: tempfile::TempDir,
    }

    /// default + A (rw) + B (read-only) + C (rw, search disabled).
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        let mut config = AppConfig::default();
        config.brain.path = Some(p("default"));
        config.brain.catalogues = vec![
            cat("a", &p("a"), true, "read_write"),
            cat("b", &p("b"), true, "read_only"),
            cat("c", &p("c"), false, "read_write"),
        ];
        Fixture { default: p("default"), a: p("a"), b: p("b"), c: p("c"), config, _dir: dir }
    }

    fn session() -> Session {
        serde_json::from_value(serde_json::json!({
            "id": "s", "orchestrator_id": null, "name": "w", "repo": "r", "status": "working",
            "agent_type": "claude-code", "cost_usd": 0.0, "started_at": 0, "pr_number": null,
            "pr_id": null, "workspace_path": null, "pid": null,
        }))
        .unwrap()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_reads_every_enabled_source_and_writes_default() {
        let _iso = isolate();
        let f = fixture();
        let set = BrainSet::default_for(&f.config);
        assert_eq!(set.reads, vec![f.default.clone(), f.a.clone(), f.b.clone()]);
        assert_eq!(set.write, f.default);
        assert_eq!(set.summary(&f.config), "reads default, a, b · writes default");
    }

    #[test]
    fn single_brain_setup_is_unchanged() {
        let _iso = isolate();
        let mut config = AppConfig::default();
        let dir = tempfile::tempdir().unwrap();
        config.brain.path = Some(dir.path().join("brain"));
        let set = BrainSet::default_for(&config);
        let single = BrainSet::single(dir.path().join("brain"));
        assert_eq!((set.reads.clone(), set.write.clone()), (single.reads, single.write));
        assert!(!set.limited, "the default is unlimited");
    }

    #[test]
    fn select_picks_any_configured_catalogue_including_search_disabled() {
        let _iso = isolate();
        let f = fixture();
        let set = BrainSet::select(&f.config, &strs(&["a", "b", "c"]), Some("a")).unwrap();
        assert_eq!(set.reads, vec![f.a.clone(), f.b.clone(), f.c.clone()]);
        assert_eq!(set.write, f.a);
    }

    #[test]
    fn select_refuses_read_only_write_and_unknown_names() {
        let _iso = isolate();
        let f = fixture();
        let err = BrainSet::select(&f.config, &strs(&["a", "b"]), Some("b")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        let err = BrainSet::select(&f.config, &strs(&["nope"]), None).unwrap_err();
        assert!(err.to_string().contains("'nope'"), "{err}");
        let err = BrainSet::select(&f.config, &strs(&["b"]), None).unwrap_err();
        assert!(err.to_string().contains("writable"), "{err}");
    }

    #[test]
    fn write_must_be_one_of_the_reads() {
        let _iso = isolate();
        let f = fixture();
        let err = BrainSet::select(&f.config, &strs(&["b"]), Some("a")).unwrap_err();
        assert!(err.to_string().contains("must also be one of the brains"), "{err}");
    }

    #[test]
    fn child_inherits_parent_set_verbatim() {
        let _iso = isolate();
        let f = fixture();
        let parent = BrainSet::select(&f.config, &strs(&["a", "b"]), Some("a")).unwrap();
        assert_eq!(parent.narrow(&f.config, &[], None).unwrap(), parent);
    }

    #[test]
    fn child_may_narrow_to_a_subset() {
        let _iso = isolate();
        let f = fixture();
        let parent = BrainSet::select(&f.config, &strs(&["default", "a", "b"]), Some("a")).unwrap();
        let child = parent.narrow(&f.config, &strs(&["b", "default"]), None).unwrap();
        assert_eq!(child.reads, vec![f.b.clone(), f.default.clone()]);
        assert_eq!(child.write, f.default, "parent's write dropped, so the first writable read is chosen");
        let child = parent.narrow(&f.config, &[], Some("default")).unwrap();
        assert_eq!(child.reads, parent.reads);
        assert_eq!(child.write, f.default);
    }

    #[test]
    fn child_cannot_widen() {
        let _iso = isolate();
        let f = fixture();
        let parent = BrainSet::select(&f.config, &strs(&["a", "b"]), Some("a")).unwrap();
        let err = parent.narrow(&f.config, &strs(&["a", "c"]), None).unwrap_err();
        assert!(err.to_string().contains("'c' is not in the parent's brain set"), "{err}");
        let err = parent.narrow(&f.config, &[], Some("default")).unwrap_err();
        assert!(err.to_string().contains("'default'"), "{err}");
        let err = parent.narrow(&f.config, &strs(&[f.c.to_str().unwrap()]), None).unwrap_err();
        assert!(err.to_string().contains("not in the parent's brain set"), "{err}");
    }

    #[test]
    fn authorize_write_refuses_read_only_and_out_of_set() {
        let _iso = isolate();
        let f = fixture();
        let set = BrainSet::select(&f.config, &strs(&["a", "b"]), Some("a")).unwrap();
        assert_eq!(set.authorize_write(&f.config, None).unwrap().catalogue.path, f.a);
        let err = set.authorize_write(&f.config, Some("b")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        let err = set.authorize_write(&f.config, Some("default")).unwrap_err();
        assert!(err.to_string().contains("not in this session's brain set"), "{err}");
    }

    #[test]
    fn read_only_flip_in_config_is_honored_without_touching_the_row() {
        let _iso = isolate();
        let mut f = fixture();
        let set = BrainSet::select(&f.config, &strs(&["a"]), Some("a")).unwrap();
        f.config.brain.catalogues[0].access = "read_only".into();
        assert!(set.authorize_write(&f.config, None).is_err());
    }

    #[test]
    fn from_session_migrated_row_reads_only_its_catalogue() {
        let _iso = isolate();
        let f = fixture();
        let mut s = session();
        s.brain_write = Some(f.a.to_string_lossy().into_owned());
        assert_eq!(BrainSet::from_session(&s, &f.config), BrainSet::single(&f.a));
        s.brain_write = None;
        assert_eq!(BrainSet::from_session(&s, &f.config), BrainSet::default_for(&f.config));
    }

    #[test]
    fn apply_to_round_trips_through_from_session() {
        let _iso = isolate();
        let f = fixture();
        let set = BrainSet::select(&f.config, &strs(&["a", "b"]), Some("a")).unwrap();
        let mut s = session();
        set.apply_to(&mut s);
        assert_eq!(BrainSet::from_session(&s, &f.config), set);
    }

    #[test]
    fn unknown_paths_become_named_writable_members() {
        let _iso = isolate();
        let f = fixture();
        let set = BrainSet::new(vec![f.a.clone(), PathBuf::from("/x/team:wiki")], f.a.clone());
        let names = set.names(&f.config);
        assert_eq!(names, vec!["a", "team:wiki"]);
        assert!(set.members(&f.config)[1].writable);
    }

    #[test]
    fn child_of_a_session_narrows_from_its_row_and_ignores_env() {
        let _iso = isolate();
        let f = fixture();
        let store = crate::store::Store::open(f._dir.path().join("t.db")).unwrap();
        let mut parent = session();
        parent.id = "orch".into();
        BrainSet::select(&f.config, &strs(&["a", "b"]), Some("a")).unwrap().apply_to(&mut parent);
        store.upsert_session(&parent).unwrap();

        let inherited = resolve_child(&store, Some("orch"), &f.config, &[], None).unwrap();
        assert_eq!(inherited.reads, vec![f.a.clone(), f.b.clone()]);
        assert!(resolve_child(&store, Some("orch"), &f.config, &strs(&["c"]), None).is_err());
        // A human (no caller session) may pick anything configured.
        assert!(resolve_child(&store, None, &f.config, &strs(&["c"]), None).is_ok());

        // A hint claiming more reads changes nothing: the row decides.
        let _guard = crate::config::ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let wide = BrainSet::new(vec![f.a.clone(), f.b.clone(), f.c.clone(), f.default.clone()], f.default.clone());
        std::env::set_var(READS_ENV, wide.reads_env_value());
        let child = resolve_child(&store, Some("orch"), &f.config, &[], None);
        std::env::remove_var(READS_ENV);
        assert_eq!(child.unwrap(), inherited);
    }

    #[test]
    fn brains_line_names_reads_and_write() {
        let _iso = isolate();
        let f = fixture();
        let mut s = session();
        assert_eq!(session_brains_line(&s, &f.config), "Brains: all");
        BrainSet::select(&f.config, &strs(&["a", "b", "c"]), Some("a")).unwrap().apply_to(&mut s);
        assert_eq!(session_brains_line(&s, &f.config), "Brains: limited to a, b, c · writes a");
    }

    #[test]
    fn an_unverifiable_session_claim_is_refused_not_treated_as_the_user() {
        assert!(refuse_unverified_claim(Some("w1")).is_err());
        assert!(refuse_unverified_claim(Some("")).is_ok());
        assert!(refuse_unverified_claim(None).is_ok());
    }

    #[test]
    fn a_set_whose_write_is_outside_its_reads_does_not_panic() {
        let _iso = isolate();
        let f = fixture();
        let odd = BrainSet { reads: vec![f.a.clone()], write: f.b.clone(), limited: true };
        assert_eq!(odd.write_member(&f.config).name(), "b");
        assert!(odd.authorize_write(&f.config, None).is_err(), "b is read-only");
    }

    #[test]
    fn reads_env_is_a_json_array_and_parses_defensively() {
        let set = BrainSet::new(vec![PathBuf::from("/a:b"), PathBuf::from("/c")], PathBuf::from("/c"));
        let v = set.reads_env_value();
        assert_eq!(v, r#"["/a:b","/c"]"#);
        assert_eq!(parse_reads_env(&v), set.reads);
        assert!(parse_reads_env("/a:/b").is_empty());
        assert!(parse_reads_env("").is_empty());
        assert_eq!(parse_reads_env(r#"["/a", 3, "", null]"#), vec![PathBuf::from("/a")]);
    }

    #[test]
    fn automated_writers_target_only_the_write_member() {
        let _iso = isolate();
        let f = fixture();
        let set = BrainSet::select(&f.config, &strs(&["a", "c"]), Some("a")).unwrap();
        let m = set.authorize_write(&f.config, None).unwrap();
        assert_eq!(m.catalogue.path, f.a, "harvest/discover-repos use authorize_write(None): never a fan-out");
    }

    #[test]
    fn env_scope_only_narrows_a_sessionless_process() {
        let _iso = isolate();
        let f = fixture();
        let everything = BrainSet::default_for(&f.config);
        let narrowed = crate::config::with_env_override(
            READS_ENV,
            serde_json::to_string(&[&f.a, &f.c]).unwrap(),
            || BrainSet::default_for(&f.config).narrowed_by_env(&f.config),
        );
        assert_eq!(narrowed.reads, vec![f.a.clone()], "c is search-disabled so it is not in the default set; env can't add it");
        assert_ne!(narrowed.reads, everything.reads);
        let by_name: Vec<String> = narrowed.members(&f.config).iter().map(|m| m.name().to_string()).collect();
        assert_eq!(by_name, vec!["a"]);
        assert!(narrowed.authorize_write(&f.config, Some("b")).is_err(), "b is outside the scope (and read-only)");
    }

    #[test]
    fn an_unlimited_session_can_write_to_a_catalogue_added_after_it_started() {
        let _iso = isolate();
        let mut f = fixture();
        let s = session();
        assert!(BrainSet::from_session(&s, &f.config).authorize_write(&f.config, Some("late")).is_err());
        let dir = tempfile::tempdir().unwrap();
        f.config.brain.catalogues.push(cat("late", dir.path(), true, "read_write"));
        let set = BrainSet::from_session(&s, &f.config);
        assert_eq!(set.authorize_write(&f.config, Some("late")).unwrap().name(), "late", "no re-spawn needed");
    }

    #[test]
    fn a_limited_session_is_refused_outside_its_limit_and_never_widens() {
        let _iso = isolate();
        let mut f = fixture();
        let mut s = session();
        BrainSet::select(&f.config, &strs(&["a"]), Some("a")).unwrap().apply_to(&mut s);
        let dir = tempfile::tempdir().unwrap();
        f.config.brain.catalogues.push(cat("late", dir.path(), true, "read_write"));
        let set = BrainSet::from_session(&s, &f.config);
        assert!(set.limited);
        let err = set.authorize_write(&f.config, Some("late")).unwrap_err();
        assert!(err.to_string().contains("not in this session's brain set"), "{err}");
        assert!(set.authorize_write(&f.config, Some("default")).is_err(), "default is outside the limit too");
        assert_eq!(set.authorize_write(&f.config, None).unwrap().name(), "a");
    }

    #[test]
    fn workers_inherit_their_orchestrators_limit_unless_narrower() {
        let _iso = isolate();
        let f = fixture();
        let limited = BrainSet::select(&f.config, &strs(&["a", "default"]), Some("a")).unwrap();
        let inherited = limited.narrow(&f.config, &[], None).unwrap();
        assert!(inherited.limited && inherited.reads == limited.reads && inherited.write == limited.write);
        let narrower = limited.narrow(&f.config, &strs(&["a"]), Some("a")).unwrap();
        assert_eq!(narrower.reads, vec![f.a.clone()]);
        assert!(limited.narrow(&f.config, &strs(&["c"]), None).is_err(), "a child never exceeds its parent's limit");
        let unlimited_parent = BrainSet::default_for(&f.config);
        assert!(!unlimited_parent.narrow(&f.config, &[], None).unwrap().limited, "an unlimited parent gives unlimited workers");
    }

    #[test]
    fn read_only_catalogues_are_never_write_targets_whatever_the_limit() {
        let _iso = isolate();
        let f = fixture();
        for set in [BrainSet::default_for(&f.config), BrainSet::select(&f.config, &strs(&["a", "b"]), Some("a")).unwrap()] {
            let err = set.authorize_write(&f.config, Some("b")).unwrap_err();
            assert!(err.to_string().contains("read-only"), "{err}");
        }
        assert!(BrainSet::select(&f.config, &strs(&["a", "b"]), Some("b")).is_err(), "read-only can't be the write brain");
    }

    #[test]
    fn a_stored_explicit_set_keeps_acting_as_a_limit_and_asking_for_everything_is_unlimited() {
        let _iso = isolate();
        let mut f = fixture();
        let mut s = session();
        s.brain_reads = vec![f.a.to_string_lossy().into_owned()];
        s.brain_write = Some(f.a.to_string_lossy().into_owned());
        let dir = tempfile::tempdir().unwrap();
        f.config.brain.catalogues.push(cat("later", dir.path(), true, "read_write"));
        let set = BrainSet::from_session(&s, &f.config);
        assert!(set.limited && set.authorize_write(&f.config, Some("later")).is_err(), "a running session isn't silently widened");

        let all = BrainSet::select(&f.config, &strs(&["default", "a", "b", "later"]), Some("default")).unwrap();
        assert!(!all.limited, "choosing every enabled brain is the default, not a limit");
        let mut row = session();
        all.apply_to(&mut row);
        assert!(row.brain_reads.is_empty() && row.brain_write.is_none());
    }

    #[test]
    fn a_row_that_stores_only_the_old_default_is_unlimited_and_its_workers_reach_new_brains() {
        let _iso = isolate();
        let mut f = fixture();
        let mut s = session();
        s.brain_reads = vec![f.default.to_string_lossy().into_owned()];
        s.brain_write = Some(f.default.to_string_lossy().into_owned());
        let row_before = (s.brain_reads.clone(), s.brain_write.clone());
        let dir = tempfile::tempdir().unwrap();
        f.config.brain.catalogues.push(cat("later", dir.path(), true, "read_write"));

        let set = BrainSet::from_session(&s, &f.config);
        assert!(!set.limited, "the old default is not a choice");
        assert_eq!(session_brains_line(&s, &f.config), "Brains: all");
        let worker = set.narrow(&f.config, &[], None).unwrap();
        assert_eq!(worker.authorize_write(&f.config, Some("later")).unwrap().name(), "later");
        assert_eq!((s.brain_reads, s.brain_write), row_before, "nothing is migrated in the store");
    }

    #[test]
    fn a_row_that_stores_more_than_the_old_default_stays_limited() {
        let _iso = isolate();
        let f = fixture();
        let mut s = session();
        s.brain_reads = vec![f.default.to_string_lossy().into_owned(), f.a.to_string_lossy().into_owned()];
        s.brain_write = Some(f.default.to_string_lossy().into_owned());
        assert!(BrainSet::from_session(&s, &f.config).limited);
        s.brain_reads = vec![f.a.to_string_lossy().into_owned()];
        s.brain_write = Some(f.a.to_string_lossy().into_owned());
        assert!(BrainSet::from_session(&s, &f.config).limited, "a different single brain is a real choice");
        s.brain_reads = vec![];
        s.brain_write = Some(f.default.to_string_lossy().into_owned());
        assert!(BrainSet::from_session(&s, &f.config).limited, "no reads with a write brain is a real choice, not the old default");
    }

    #[test]
    fn an_unlimited_session_ignores_ninox_brain_so_env_cannot_redirect_its_writes() {
        let _iso = isolate();
        let f = fixture();
        let elsewhere = tempfile::tempdir().unwrap();
        let set = crate::config::with_env_override("NINOX_BRAIN", elsewhere.path(), || BrainSet::from_session(&session(), &f.config));
        assert_eq!(set.write, f.default, "write stays the configured default brain");
        assert!(set.reads.iter().all(|r| r != elsewhere.path()));
        assert!(set.authorize_write(&f.config, Some(&elsewhere.path().to_string_lossy())).is_err());
    }
}
