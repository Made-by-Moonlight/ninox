//! Toolkit-neutral logic behind the Brain tab's Sources view
//! (`BrainSources.dc.html`), shared by the GUI and TUI: a source's type,
//! health and sync state, the add/edit form and its validation, the config
//! mutations (always applied inside `AppConfig::update`, the guarded
//! read→apply→write path), syncing, and keeping credentials off screen.
//!
//! Source *type* (Local folder / GitHub wiki / S3) is never stored — it's
//! always derived from `CatalogueRef.remote` (`None` / a wiki URL / else),
//! matching how `brain_sync` itself routes a remote to a `RemoteStore` impl.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ninox_core::brain_sync::{RemoteStatus, SyncReport};
use ninox_core::config::{AppConfig, CatalogueRef};
use ninox_core::{BrainIndex, QueryFilters};

/// The implicit catalogue `AppConfig::catalogue_options()` always lists
/// first, built from `[brain]` itself — there's no `[[brain.catalogues]]`
/// entry behind it to edit, toggle or remove.
pub const DEFAULT_SOURCE: &str = "default";

pub const MASK: &str = "••••";

/// A source's live status — computed on demand (not kept fresh
/// automatically), so stale rather than wrong.
#[derive(Debug, Clone, Default)]
pub struct SourceStatus {
    pub entry_count: usize,
    /// `None` for a plain Local folder (no `.sync.toml`) or when the
    /// catalogue hasn't been opened yet.
    pub remote_status: Option<RemoteStatus>,
    /// Set when a sync fails, cleared on the next successful one —
    /// session-only, matching `SyncState` having no persisted error field.
    pub last_error: Option<String>,
    pub syncing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceType {
    #[default]
    Local,
    GithubWiki,
    S3,
}

impl SourceType {
    pub const ALL: [SourceType; 3] = [SourceType::Local, SourceType::GithubWiki, SourceType::S3];

    pub fn from_remote(remote: Option<&str>) -> Self {
        match remote {
            None => SourceType::Local,
            Some(r) if ninox_core::brain_sync::is_github_wiki_remote(r) => SourceType::GithubWiki,
            Some(_) => SourceType::S3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceType::Local => "LOCAL FOLDER",
            SourceType::GithubWiki => "GITHUB WIKI",
            SourceType::S3 => "S3",
        }
    }

    /// The form's Type choice, as the design's segmented control words it.
    pub fn choice_label(self) -> &'static str {
        match self {
            SourceType::Local => "Local folder",
            SourceType::GithubWiki => "GitHub wiki",
            SourceType::S3 => "S3",
        }
    }

    /// What the location field holds for this type.
    pub fn location_label(self) -> &'static str {
        match self {
            SourceType::Local => "Folder path",
            SourceType::GithubWiki => "Wiki URL",
            SourceType::S3 => "s3://bucket/prefix",
        }
    }

    /// Where a remote type's credentials come from — never ninox itself.
    pub fn credentials_hint(self) -> Option<&'static str> {
        match self {
            SourceType::Local => None,
            SourceType::GithubWiki => Some("Uses your ambient git credentials — nothing is stored here."),
            SourceType::S3 => Some("Uses your AWS profile/SSO — nothing is stored here."),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Healthy,
    /// The last sync this session failed.
    Failing,
}

pub fn health(status: Option<&SourceStatus>) -> Health {
    if status.is_some_and(|s| s.last_error.is_some()) {
        Health::Failing
    } else {
        Health::Healthy
    }
}

pub fn pending_pushes(status: Option<&SourceStatus>) -> usize {
    status.and_then(|s| s.remote_status.as_ref()).map_or(0, |rs| rs.pending_pushes.len())
}

pub const CADENCES: [&str; 3] = ["manual", "on_launch", "every_10_min"];
pub const ACCESSES: [&str; 2] = ["read_write", "read_only"];

pub fn cadence_label(cadence: &str) -> &'static str {
    match cadence {
        "on_launch" => "on launch",
        "every_10_min" => "every 10 min",
        _ => "manual sync",
    }
}

pub fn access_label(access: &str) -> &'static str {
    match access {
        "read_only" => "read only",
        _ => "read and write",
    }
}

/// Bucketed "X ago" for an elapsed `Duration` (testable without a clock).
pub fn format_ago(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A source's sync state in words: "local only" for a plain folder, else
/// "synced X ago" / "never synced", then its cadence.
pub fn sync_label(c: &CatalogueRef, status: Option<&SourceStatus>, now_unix: u64) -> String {
    if SourceType::from_remote(c.remote.as_deref()) == SourceType::Local {
        return "local only".to_string();
    }
    // `remote_status` is `None` until the catalogue has a materialized
    // `.sync.toml` (first Reindex/Sync now) — that's "hasn't synced yet",
    // not "has no remote".
    let synced = status
        .and_then(|s| s.remote_status.as_ref())
        .filter(|rs| rs.last_check_unix > 0)
        .map(|rs| format!("synced {}", format_ago(Duration::from_secs(now_unix.saturating_sub(rs.last_check_unix)))))
        .unwrap_or_else(|| "never synced".to_string());
    format!("{synced} · {}", cadence_label(&c.cadence))
}

/// Shown for a source wherever its location appears: the remote URL, else
/// the folder.
pub fn location_label(c: &CatalogueRef) -> String {
    match c.remote.as_deref() {
        Some(remote) => redact_credentials(remote),
        None => c.path.to_string_lossy().to_string(),
    }
}

/// The add/edit form — shared by "Add source" (`editing: None`) and editing
/// an existing one (`editing: Some(original_name)`, so Save can find and
/// replace it even if the name field itself changed).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceEditForm {
    pub editing: Option<String>,
    pub name: String,
    pub source_type: SourceType,
    /// Folder path (Local) or the wiki URL / `s3://bucket/prefix`.
    pub location: String,
    pub region: String,
    pub endpoint: String,
    /// Local cache path — only meaningful for remote types; for Local
    /// folder, `location` doubles as this (there's nothing to cache).
    pub cache_path: String,
    pub cadence: String,
    pub access: String,
    pub enabled: bool,
    /// Human-readable refusal reason from the last Save attempt, if any.
    pub error: Option<String>,
}

impl Default for SourceEditForm {
    fn default() -> Self {
        Self {
            editing: None,
            name: String::new(),
            source_type: SourceType::Local,
            location: String::new(),
            region: String::new(),
            endpoint: String::new(),
            cache_path: String::new(),
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
            enabled: true,
            error: None,
        }
    }
}

/// Populates an edit form from an existing catalogue — the inverse of
/// `build_catalogue_ref`.
pub fn form_from_catalogue(c: &CatalogueRef) -> SourceEditForm {
    let source_type = SourceType::from_remote(c.remote.as_deref());
    let (location, cache_path) = match source_type {
        SourceType::Local => (c.path.to_string_lossy().to_string(), String::new()),
        SourceType::GithubWiki | SourceType::S3 => (c.remote.clone().unwrap_or_default(), c.path.to_string_lossy().to_string()),
    };
    SourceEditForm {
        editing: Some(c.name.clone()),
        name: c.name.clone(),
        source_type,
        location,
        region: c.region.clone().unwrap_or_default(),
        endpoint: c.endpoint.clone().unwrap_or_default(),
        cache_path,
        cadence: c.cadence.clone(),
        access: c.access.clone(),
        enabled: c.enabled,
        error: None,
    }
}

/// Validates and builds a `CatalogueRef` from the form's string fields —
/// pure (no filesystem access, no uniqueness check). Returns the first
/// problem in the order the fields appear, so the message points at the
/// field the user is looking at.
pub fn build_catalogue_ref(form: &SourceEditForm) -> Result<CatalogueRef, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("give this source a name".to_string());
    }

    if form.location.contains(MASK) || form.endpoint.contains(MASK) {
        return Err(format!("re-enter the credentials: {MASK} is the masked value, not the secret"));
    }

    let (path, remote, region, endpoint) = match form.source_type {
        SourceType::Local => {
            let loc = form.location.trim();
            if loc.is_empty() {
                return Err("give this source a folder path".to_string());
            }
            (PathBuf::from(loc), None, None, None)
        }
        SourceType::GithubWiki => {
            let url = form.location.trim();
            if url.is_empty() {
                return Err("give this source a wiki URL".to_string());
            }
            if !ninox_core::brain_sync::is_github_wiki_remote(url) {
                return Err("wiki URL must look like https://github.com/{owner}/{repo}/wiki".to_string());
            }
            let cache = form.cache_path.trim();
            if cache.is_empty() {
                return Err("give this source a local cache path".to_string());
            }
            // Sync uses ambient git credentials, so a token in the URL is
            // never needed and must not be written to config.
            (PathBuf::from(cache), Some(strip_userinfo(url)), None, None)
        }
        SourceType::S3 => {
            let loc = form.location.trim();
            if !loc.starts_with("s3://") {
                return Err("S3 location must start with s3://".to_string());
            }
            let cache = form.cache_path.trim();
            if cache.is_empty() {
                return Err("give this source a local cache path".to_string());
            }
            let region = Some(form.region.trim().to_string()).filter(|s| !s.is_empty());
            let endpoint = Some(form.endpoint.trim().to_string()).filter(|s| !s.is_empty());
            (PathBuf::from(cache), Some(loc.to_string()), region, endpoint)
        }
    };

    Ok(CatalogueRef {
        name: name.to_string(),
        path,
        remote,
        endpoint,
        region,
        cache_ttl_secs: None,
        enabled: form.enabled,
        cadence: form.cadence.clone(),
        access: form.access.clone(),
    })
}

/// `https://user:tok@host/p` → `https://host/p`.
fn strip_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{path}"),
        None => url.to_string(),
    }
}

/// A name must stay unique against every *other* source — the one being
/// edited may keep its own.
pub fn check_name_free(existing: &[CatalogueRef], name: &str, editing: Option<&str>) -> Result<(), String> {
    if existing.iter().any(|c| c.name == name && Some(c.name.as_str()) != editing) {
        return Err(format!("a source named {name} already exists"));
    }
    Ok(())
}

/// Everything a Save does before touching config: validate, check the name
/// against `existing`, create the folder/cache directory and initialise its
/// index (creates `.index.db` + `.gitignore`; a no-op if it exists).
pub fn prepare_save(form: &SourceEditForm, existing: &[CatalogueRef]) -> Result<CatalogueRef, String> {
    let catalogue = build_catalogue_ref(form)?;
    check_name_free(existing, &catalogue.name, form.editing.as_deref())?;
    std::fs::create_dir_all(&catalogue.path).map_err(|e| format!("couldn't create {}: {e}", catalogue.path.display()))?;
    BrainIndex::open(&catalogue.path).map_err(|e| format!("couldn't initialize brain index: {e}"))?;
    Ok(catalogue)
}

/// Replaces the source being edited (by its original name) or adds a new
/// one. Run it inside `AppConfig::update`.
pub fn apply_save(cfg: &mut AppConfig, editing: Option<&str>, catalogue: &CatalogueRef) {
    if let Some(old_name) = editing {
        cfg.brain.catalogues.retain(|existing| existing.name != old_name);
    }
    cfg.brain.catalogues.push(catalogue.clone());
}

/// Flips "Include in search". `false` when no configured source has that
/// name (including the built-in default, which has no entry to flip).
pub fn apply_toggle(cfg: &mut AppConfig, name: &str) -> bool {
    match cfg.brain.catalogues.iter_mut().find(|c| c.name == name) {
        Some(c) => {
            c.enabled = !c.enabled;
            true
        }
        None => false,
    }
}

/// Forgets a source's config entry only — never touches the remote, and
/// leaves any local `.sync.toml`/cache files alone. `false` when there was
/// nothing to remove.
pub fn apply_remove(cfg: &mut AppConfig, name: &str) -> bool {
    let before = cfg.brain.catalogues.len();
    cfg.brain.catalogues.retain(|c| c.name != name);
    cfg.brain.catalogues.len() != before
}

/// Entry count and remote status for one source; opens its index, so call
/// it off any UI thread.
pub fn load_status(c: &CatalogueRef) -> SourceStatus {
    let entry_count = BrainIndex::open(&c.path)
        .and_then(|idx| idx.query("", None, QueryFilters::default()))
        .map(|entries| entries.len())
        .unwrap_or(0);
    let remote_status = ninox_core::brain_sync::remote_status(&c.path).ok().flatten();
    SourceStatus { entry_count, remote_status, ..Default::default() }
}

/// A full sync (pull → resolve → push) of one source. Its first sync
/// materializes `.sync.toml` from the config; a plain folder reports an
/// empty sync.
pub async fn sync_source(config: &AppConfig, path: &Path) -> Result<SyncReport, String> {
    if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(config, path) {
        tracing::warn!("sources: failed to materialize .sync.toml: {e}");
    }
    match ninox_core::brain_sync::BrainSync::for_brain(path).await {
        Ok(None) => Ok(SyncReport::default()),
        Ok(Some(sync)) => sync.sync().await.map_err(|e| redact_credentials(&e.to_string())),
        Err(e) => Err(redact_credentials(&e.to_string())),
    }
}

pub fn sync_log_line(name: &str, report: &SyncReport) -> String {
    let n = report.conflicts.len();
    format!("{name} — pulled {} · pushed {} · {n} conflict{}", report.pulled, report.pushed, if n == 1 { "" } else { "s" })
}

/// Masks credentials anywhere in `text`: a URL's userinfo
/// (`https://user:tok@host` → `https://user:••••@host`, a bare
/// `https://tok@host` → `https://••••@host`) and the values of
/// credential-like query parameters (`X-Amz-Signature`, `token`, `sig`, …).
/// Safe on any text — locations, endpoints, sync error messages.
pub fn redact_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in text.split_inclusive(char::is_whitespace) {
        let split = piece.char_indices().last().filter(|(_, c)| c.is_whitespace()).map_or(piece.len(), |(i, _)| i);
        out.push_str(&redact_word(&piece[..split]));
        out.push_str(&piece[split..]);
    }
    out
}

/// Applies an edit made to the masked display (`redact_credentials(real)`)
/// back onto `real`. An edit wholly before the first or after the last mask
/// keeps the hidden secret; one that touches a mask takes `edited` as typed
/// (a leftover mask is then refused by `build_catalogue_ref`).
pub fn apply_masked_edit(real: &str, edited: &str) -> String {
    let masked = redact_credentials(real);
    if masked == real {
        return edited.to_string();
    }
    let prefix: usize = masked.chars().zip(edited.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a.len_utf8()).sum();
    let max_suffix = masked.len().min(edited.len()) - prefix;
    let suffix: usize = masked[prefix..]
        .chars()
        .rev()
        .zip(edited[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a.len_utf8())
        .scan(0, |acc, n| {
            *acc += n;
            Some(*acc)
        })
        .take_while(|total| *total <= max_suffix)
        .last()
        .unwrap_or(0);
    let changed_end = masked.len() - suffix;
    let (Some(first), Some(last)) = (masked.find(MASK), masked.rfind(MASK)) else {
        return edited.to_string();
    };
    let last_end = last + MASK.len();
    // Text outside the masks is byte-identical in `real` and `masked`, so
    // an unchanged edge maps across by its length from the far end. An edit
    // touching a mask's edge would merge into the secret, so it's excluded.
    let spliced = if prefix > last_end {
        let kept = real.len() - (masked.len() - prefix);
        format!("{}{}", &real[..kept], &edited[prefix..])
    } else if changed_end < first {
        format!("{}{}", &edited[..edited.len() - suffix], &real[changed_end..])
    } else {
        return edited.to_string();
    };
    // Deleting the `@`, a `/` of `://` or a secret key's `=` would unmask
    // the kept secret; only keep it when it still hides behind the mask.
    if redact_credentials(&spliced) == edited { spliced } else { edited.to_string() }
}

/// Userinfo runs to the URL's last `@`, not its first `/`: a password may
/// contain `/`, and over-masking an `@` in a path is the safe failure.
fn redact_word(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(i) = rest.find("://") {
        out.push_str(&rest[..i + 3]);
        rest = &rest[i + 3..];
        let url_end = match rest.find("://") {
            Some(next) => rest[..next].trim_end_matches(|c: char| c.is_ascii_alphanumeric() || "+-.".contains(c)).len(),
            None => rest.len(),
        };
        let url = &rest[..url_end];
        let after_userinfo = match url.rfind('@') {
            Some(at) => {
                if let Some((user, _)) = url[..at].split_once(':') {
                    out.push_str(user);
                    out.push(':');
                }
                out.push_str(MASK);
                &url[at..]
            }
            None => url,
        };
        out.push_str(&redact_query(after_userinfo));
        rest = &rest[url_end..];
    }
    out.push_str(rest);
    out
}

fn redact_query(tail: &str) -> String {
    let Some(q) = tail.find('?') else { return tail.to_string() };
    let (path, query) = tail.split_at(q + 1);
    let (query, fragment) = query.split_at(query.find('#').unwrap_or(query.len()));
    let params: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) if !v.is_empty() && is_secret_key(k) => format!("{k}={MASK}"),
            _ => kv.to_string(),
        })
        .collect();
    format!("{path}{}{fragment}", params.join("&"))
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "password", "passwd", "sig", "credential", "key", "auth"].iter().any(|s| key.contains(s))
}

/// One labelled value for a source's detail view. A `secret` field only
/// ever renders as `set`/`unset`; everything else is shown with any
/// embedded credentials masked. Route every displayed source value through
/// `shown()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceField {
    pub label: &'static str,
    pub value: Option<String>,
    pub secret: bool,
}

impl SourceField {
    fn plain(label: &'static str, value: impl Into<String>) -> Self {
        Self { label, value: Some(value.into()), secret: false }
    }

    pub fn shown(&self) -> String {
        match (&self.value, self.secret) {
            (Some(v), true) if !v.is_empty() => "set".to_string(),
            (_, true) => "unset".to_string(),
            (Some(v), false) if !v.is_empty() => redact_credentials(v),
            _ => "—".to_string(),
        }
    }
}

/// The configuration side of a source's detail view, in the edit form's
/// field order.
pub fn source_fields(c: &CatalogueRef) -> Vec<SourceField> {
    let t = SourceType::from_remote(c.remote.as_deref());
    let mut fields = vec![SourceField::plain("Type", t.choice_label()), SourceField::plain(t.location_label(), location_label(c))];
    if t == SourceType::S3 {
        fields.push(SourceField { label: "Region", value: c.region.clone(), secret: false });
        fields.push(SourceField { label: "Endpoint", value: c.endpoint.clone(), secret: false });
    }
    if t != SourceType::Local {
        fields.push(SourceField::plain("Local cache", c.path.to_string_lossy()));
        fields.push(SourceField::plain("Sync", cadence_label(&c.cadence)));
        fields.push(SourceField::plain("Access", access_label(&c.access)));
    }
    fields.push(SourceField::plain("Search", if c.enabled { "included" } else { "excluded" }));
    fields
}

#[cfg(test)]
mod tests {

    #[test]
    fn masked_edit_after_the_secret_keeps_it() {
        let real = "https://bob:hunter2@github.com/acme/repo/wiki";
        let masked = redact_credentials(real);
        let edited = masked.replace("acme/repo", "acme/other");
        assert_eq!(apply_masked_edit(real, &edited), "https://bob:hunter2@github.com/acme/other/wiki");
        assert_eq!(apply_masked_edit(real, &format!("{masked}/x")), format!("{real}/x"));
    }

    #[test]
    fn masked_edit_before_the_secret_keeps_it() {
        let real = "https://bob:hunter2@github.com/acme/repo/wiki";
        let edited = redact_credentials(real).replacen("bob", "alice", 1);
        assert_eq!(apply_masked_edit(real, &edited), "https://alice:hunter2@github.com/acme/repo/wiki");
    }

    #[test]
    fn masked_edit_touching_the_secret_takes_the_typed_value() {
        let real = "https://tok@github.com/a/b/wiki";
        let masked = redact_credentials(real);
        assert_eq!(apply_masked_edit(real, "https://newtok@github.com/a/b/wiki"), "https://newtok@github.com/a/b/wiki");
        let partial = masked.replacen(MASK, "•••", 1);
        assert_eq!(apply_masked_edit(real, &partial), partial);
    }

    #[test]
    fn masked_edit_that_would_unmask_the_secret_takes_the_typed_value() {
        let real = "https://tok@github.com/a/b/wiki";
        let masked = redact_credentials(real);
        for edited in [masked.replacen('@', "", 1), masked.replacen("://", ":/", 1)] {
            let out = apply_masked_edit(real, &edited);
            assert!(!out.contains("tok"), "{edited:?} -> {out:?}");
            assert_eq!(out, edited);
        }
        let signed = "https://h/b?X-Amz-Signature=abc&region=eu";
        let out = apply_masked_edit(signed, &redact_credentials(signed).replacen('=', "", 1));
        assert!(!out.contains("abc"), "{out:?}");
    }

    #[test]
    fn typing_against_a_mask_edge_never_changes_the_secret() {
        let real = "https://tok@github.com/a/b/wiki";
        let masked = redact_credentials(real);
        let after = masked.replacen(&format!("{MASK}@"), &format!("{MASK}x@"), 1);
        let before = masked.replacen(MASK, &format!("x{MASK}"), 1);
        for edited in [after, before] {
            let out = apply_masked_edit(real, &edited);
            assert!(!out.contains("tok"), "{edited:?} -> {out:?}");
        }
    }

    #[test]
    fn unmasked_values_are_taken_as_typed() {
        assert_eq!(apply_masked_edit("/tmp/brain", "/tmp/brain2"), "/tmp/brain2");
        assert_eq!(apply_masked_edit("", "s3://b/p"), "s3://b/p");
    }

    #[test]
    fn save_refuses_a_leftover_mask() {
        let form = SourceEditForm {
            name: "wiki".into(),
            source_type: SourceType::GithubWiki,
            location: format!("https://{MASK}@github.com/a/b/wiki"),
            cache_path: "/tmp/wiki".into(),
            ..Default::default()
        };
        assert!(build_catalogue_ref(&form).unwrap_err().contains("re-enter the credentials"));
    }

    #[test]
    fn saving_a_credentialed_wiki_url_drops_the_userinfo() {
        let form = SourceEditForm {
            name: "wiki".into(),
            source_type: SourceType::GithubWiki,
            location: "https://bob:ghp_SECRET@github.com/acme/handbook/wiki".into(),
            cache_path: "/tmp/wiki".into(),
            ..Default::default()
        };
        let c = build_catalogue_ref(&form).unwrap();
        assert_eq!(c.remote.as_deref(), Some("https://github.com/acme/handbook/wiki"));
        assert_eq!(strip_userinfo("https://github.com/a/b/wiki"), "https://github.com/a/b/wiki");
    }

    #[test]
    fn credentialed_and_clone_form_wiki_urls_are_wiki_sources() {
        for remote in ["https://bob:tok@github.com/acme/handbook/wiki", "https://github.com/acme/handbook.wiki.git"] {
            let t = SourceType::from_remote(Some(remote));
            assert_eq!(t, SourceType::GithubWiki, "{remote}");
            assert_eq!((t.label(), t.location_label()), ("GITHUB WIKI", "Wiki URL"));
        }
    }

    use super::*;

    fn local_form(name: &str, path: &str) -> SourceEditForm {
        SourceEditForm { name: name.to_string(), location: path.to_string(), ..Default::default() }
    }

    fn catalogue(name: &str, remote: Option<&str>) -> CatalogueRef {
        CatalogueRef {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{name}")),
            remote: remote.map(str::to_string),
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "every_10_min".to_string(),
            access: "read_write".to_string(),
        }
    }

    fn remote_status(last_check_unix: u64, pending: usize) -> RemoteStatus {
        RemoteStatus {
            remote: "s3://b/p".to_string(),
            cache_ttl_secs: 0,
            generation: 0,
            last_check_unix,
            pending_pushes: (0..pending).map(|i| format!("e{i}.md")).collect(),
            conflict_files: Vec::new(),
        }
    }

    #[test]
    fn source_type_from_remote_distinguishes_all_three() {
        assert_eq!(SourceType::from_remote(None), SourceType::Local);
        assert_eq!(SourceType::from_remote(Some("https://github.com/acme/brain/wiki")), SourceType::GithubWiki);
        assert_eq!(SourceType::from_remote(Some("s3://bucket/prefix")), SourceType::S3);
    }

    #[test]
    fn source_type_from_remote_treats_a_non_wiki_github_url_as_s3() {
        // Only an exact `{owner}/{repo}/wiki` is a wiki remote — anything
        // else falls through to S3, the same way `BrainSync::for_brain` does.
        assert_eq!(SourceType::from_remote(Some("https://github.com/acme/wiki")), SourceType::S3);
    }

    #[test]
    fn build_catalogue_ref_rejects_an_empty_name() {
        assert!(build_catalogue_ref(&local_form("  ", "/tmp/brain")).unwrap_err().contains("name"));
    }

    #[test]
    fn build_catalogue_ref_rejects_an_empty_local_path() {
        assert!(build_catalogue_ref(&local_form("team", "  ")).unwrap_err().contains("folder path"));
    }

    #[test]
    fn build_catalogue_ref_builds_a_local_source_with_no_remote() {
        let c = build_catalogue_ref(&local_form("team", "/tmp/team-brain")).unwrap();
        assert_eq!(c.name, "team");
        assert_eq!(c.path, PathBuf::from("/tmp/team-brain"));
        assert_eq!(c.remote, None);
    }

    #[test]
    fn build_catalogue_ref_rejects_a_malformed_wiki_url() {
        let form = SourceEditForm {
            name: "team".to_string(),
            source_type: SourceType::GithubWiki,
            location: "https://github.com/acme/brain".to_string(),
            cache_path: "/tmp/cache".to_string(),
            ..Default::default()
        };
        assert!(build_catalogue_ref(&form).unwrap_err().contains("wiki URL"));
    }

    #[test]
    fn build_catalogue_ref_rejects_a_wiki_source_with_no_cache_path() {
        let form = SourceEditForm {
            name: "team".to_string(),
            source_type: SourceType::GithubWiki,
            location: "https://github.com/acme/brain/wiki".to_string(),
            ..Default::default()
        };
        assert!(build_catalogue_ref(&form).unwrap_err().contains("cache path"));
    }

    #[test]
    fn build_catalogue_ref_builds_a_wiki_source() {
        let form = SourceEditForm {
            name: "team".to_string(),
            source_type: SourceType::GithubWiki,
            location: "https://github.com/acme/brain/wiki".to_string(),
            cache_path: "/tmp/cache".to_string(),
            ..Default::default()
        };
        let c = build_catalogue_ref(&form).unwrap();
        assert_eq!(c.path, PathBuf::from("/tmp/cache"));
        assert_eq!(c.remote.as_deref(), Some("https://github.com/acme/brain/wiki"));
    }

    #[test]
    fn build_catalogue_ref_rejects_an_s3_location_without_the_scheme() {
        let form = SourceEditForm {
            name: "team".to_string(),
            source_type: SourceType::S3,
            location: "bucket/prefix".to_string(),
            cache_path: "/tmp/cache".to_string(),
            ..Default::default()
        };
        assert!(build_catalogue_ref(&form).unwrap_err().contains("s3://"));
    }

    #[test]
    fn build_catalogue_ref_builds_an_s3_source_with_region_and_endpoint() {
        let form = SourceEditForm {
            name: "team".to_string(),
            source_type: SourceType::S3,
            location: "s3://team-brains/main".to_string(),
            region: "eu-west-1".to_string(),
            endpoint: "https://r2.example.com".to_string(),
            cache_path: "/tmp/cache".to_string(),
            ..Default::default()
        };
        let c = build_catalogue_ref(&form).unwrap();
        assert_eq!(c.remote.as_deref(), Some("s3://team-brains/main"));
        assert_eq!(c.region.as_deref(), Some("eu-west-1"));
        assert_eq!(c.endpoint.as_deref(), Some("https://r2.example.com"));
    }

    #[test]
    fn build_catalogue_ref_omits_blank_region_and_endpoint() {
        let form = SourceEditForm {
            name: "team".to_string(),
            source_type: SourceType::S3,
            location: "s3://team-brains/main".to_string(),
            cache_path: "/tmp/cache".to_string(),
            ..Default::default()
        };
        let c = build_catalogue_ref(&form).unwrap();
        assert_eq!(c.region, None);
        assert_eq!(c.endpoint, None);
    }

    #[test]
    fn build_catalogue_ref_carries_cadence_access_and_enabled_from_the_form() {
        let form = SourceEditForm { cadence: "every_10_min".to_string(), access: "read_only".to_string(), enabled: false, ..local_form("team", "/tmp/brain") };
        let c = build_catalogue_ref(&form).unwrap();
        assert_eq!(c.cadence, "every_10_min");
        assert_eq!(c.access, "read_only");
        assert!(!c.enabled);
    }

    #[test]
    fn form_from_catalogue_round_trips_a_local_source() {
        let c = CatalogueRef { cadence: "manual".to_string(), ..catalogue("team", None) };
        let form = form_from_catalogue(&c);
        assert_eq!(form.editing.as_deref(), Some("team"));
        assert_eq!(form.source_type, SourceType::Local);
        assert_eq!(form.location, "/tmp/team");
        assert_eq!(build_catalogue_ref(&form).unwrap(), c);
    }

    #[test]
    fn form_from_catalogue_round_trips_an_s3_source() {
        let c = CatalogueRef {
            path: PathBuf::from("/tmp/cache"),
            endpoint: Some("https://r2.example.com".to_string()),
            region: Some("eu-west-1".to_string()),
            enabled: false,
            access: "read_only".to_string(),
            ..catalogue("team", Some("s3://team-brains/main"))
        };
        let form = form_from_catalogue(&c);
        assert_eq!(form.source_type, SourceType::S3);
        assert_eq!(form.location, "s3://team-brains/main");
        assert_eq!(form.cache_path, "/tmp/cache");
        assert!(!form.enabled);
        assert_eq!(build_catalogue_ref(&form).unwrap(), c);
    }

    #[test]
    fn format_ago_buckets_by_magnitude() {
        assert_eq!(format_ago(Duration::from_secs(5)), "just now");
        assert_eq!(format_ago(Duration::from_secs(125)), "2 min ago");
        assert_eq!(format_ago(Duration::from_secs(3 * 3600)), "3h ago");
        assert_eq!(format_ago(Duration::from_secs(2 * 86400)), "2d ago");
    }

    #[test]
    fn health_fails_only_after_a_failed_sync() {
        assert_eq!(health(None), Health::Healthy);
        assert_eq!(health(Some(&SourceStatus::default())), Health::Healthy);
        let failed = SourceStatus { last_error: Some("ExpiredToken".into()), ..Default::default() };
        assert_eq!(health(Some(&failed)), Health::Failing);
    }

    #[test]
    fn pending_pushes_counts_the_remote_status_queue() {
        assert_eq!(pending_pushes(None), 0);
        let s = SourceStatus { remote_status: Some(remote_status(1, 2)), ..Default::default() };
        assert_eq!(pending_pushes(Some(&s)), 2);
    }

    #[test]
    fn sync_label_reads_local_never_and_synced() {
        let local = catalogue("default", None);
        assert_eq!(sync_label(&local, None, 1_000), "local only");
        let s3 = catalogue("archive", Some("s3://b/p"));
        assert_eq!(sync_label(&s3, None, 1_000), "never synced · every 10 min");
        let never = SourceStatus { remote_status: Some(remote_status(0, 0)), ..Default::default() };
        assert_eq!(sync_label(&s3, Some(&never), 1_000), "never synced · every 10 min");
        let synced = SourceStatus { remote_status: Some(remote_status(1_000 - 240, 0)), ..Default::default() };
        assert_eq!(sync_label(&s3, Some(&synced), 1_000), "synced 4 min ago · every 10 min");
    }

    #[test]
    fn check_name_free_lets_an_edit_keep_its_own_name() {
        let existing = vec![catalogue("default", None), catalogue("team", None)];
        assert!(check_name_free(&existing, "team", None).unwrap_err().contains("already exists"));
        assert!(check_name_free(&existing, "team", Some("team")).is_ok());
        assert!(check_name_free(&existing, "team", Some("other")).is_err());
        assert!(check_name_free(&existing, "fresh", None).is_ok());
    }

    #[test]
    fn apply_save_adds_or_replaces_by_original_name() {
        let mut cfg = AppConfig::default();
        apply_save(&mut cfg, None, &catalogue("team", None));
        assert_eq!(cfg.brain.catalogues.len(), 1);
        apply_save(&mut cfg, Some("team"), &catalogue("renamed", None));
        let names: Vec<_> = cfg.brain.catalogues.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["renamed"]);
    }

    #[test]
    fn apply_toggle_and_remove_report_whether_a_source_matched() {
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues.push(catalogue("team", None));
        assert!(apply_toggle(&mut cfg, "team"));
        assert!(!cfg.brain.catalogues[0].enabled);
        assert!(!apply_toggle(&mut cfg, DEFAULT_SOURCE));
        assert!(!apply_remove(&mut cfg, DEFAULT_SOURCE));
        assert!(apply_remove(&mut cfg, "team"));
        assert!(cfg.brain.catalogues.is_empty());
    }

    #[test]
    fn sync_log_line_pluralizes_conflicts() {
        let r = SyncReport { pulled: 3, pushed: 1, conflicts: vec!["a".into()], ..Default::default() };
        assert_eq!(sync_log_line("team", &r), "team — pulled 3 · pushed 1 · 1 conflict");
        assert_eq!(sync_log_line("team", &SyncReport::default()), "team — pulled 0 · pushed 0 · 0 conflicts");
    }

    #[test]
    fn redact_credentials_masks_url_userinfo() {
        assert_eq!(redact_credentials("https://bob:ghp_SECRET@github.com/acme/brain/wiki"), "https://bob:••••@github.com/acme/brain/wiki");
        assert_eq!(redact_credentials("https://ghp_SECRET@github.com/acme/brain/wiki"), "https://••••@github.com/acme/brain/wiki");
        assert_eq!(redact_credentials("https://AKIA:abc/def@host/x"), "https://AKIA:••••@host/x", "a password may contain /");
        assert_eq!(
            redact_credentials("tried a,https://x:SECRET1@h1/p,https://u:SECRET2@h2"),
            "tried a,https://x:••••@h1/p,https://u:••••@h2",
            "every URL in one word"
        );
        assert!(!location_label(&catalogue("team", Some("https://bob:ghp_SECRET@github.com/acme/brain/wiki"))).contains("ghp_SECRET"));
    }

    #[test]
    fn redact_credentials_masks_credential_query_values() {
        let url = "https://r2.example.com/b?X-Amz-Credential=AKIA123&X-Amz-Signature=abc&region=eu";
        assert_eq!(redact_credentials(url), "https://r2.example.com/b?X-Amz-Credential=••••&X-Amz-Signature=••••&region=eu");
    }

    #[test]
    fn redact_credentials_leaves_plain_text_and_clean_urls_alone_and_works_inside_sentences() {
        assert_eq!(redact_credentials("s3://team-brains/main"), "s3://team-brains/main");
        assert_eq!(redact_credentials("/Users/me/brain"), "/Users/me/brain");
        let msg = "clone of https://x:tok@github.com/a/b/wiki failed:\n  auth required";
        assert_eq!(redact_credentials(msg), "clone of https://x:••••@github.com/a/b/wiki failed:\n  auth required");
    }

    #[test]
    fn source_field_never_shows_a_secret_value() {
        let set = SourceField { label: "Token", value: Some("ghp_SECRET".into()), secret: true };
        assert_eq!(set.shown(), "set");
        let empty = SourceField { label: "Token", value: Some(String::new()), secret: true };
        assert_eq!(empty.shown(), "unset");
        let none = SourceField { label: "Token", value: None, secret: true };
        assert_eq!(none.shown(), "unset");
        let url = SourceField { label: "Wiki URL", value: Some("https://u:p@github.com/a/b/wiki".into()), secret: false };
        assert_eq!(url.shown(), "https://u:••••@github.com/a/b/wiki");
        assert_eq!(SourceField { label: "Region", value: None, secret: false }.shown(), "—");
    }

    #[test]
    fn source_fields_follow_the_form_order_per_type() {
        let labels = |c: &CatalogueRef| source_fields(c).iter().map(|f| f.label).collect::<Vec<_>>();
        assert_eq!(labels(&catalogue("default", None)), ["Type", "Folder path", "Search"]);
        assert_eq!(
            labels(&catalogue("archive", Some("s3://b/p"))),
            ["Type", "s3://bucket/prefix", "Region", "Endpoint", "Local cache", "Sync", "Access", "Search"]
        );
        assert!(source_fields(&catalogue("team", Some("https://github.com/a/b/wiki"))).iter().all(|f| !f.secret));
    }
}
