//! The Brain tab's Sources sub-view (`S`): the TUI's translation of
//! `BrainSources.dc.html` — one row per configured source, a detail pane,
//! the add/edit form, and the config writes behind them. The source logic
//! itself (types, health, validation, config mutations, sync, credential
//! masking) is `ninox_ui_shared::brain_sources`, shared with the GUI.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ninox_core::brain_sync::SyncReport;
use ninox_core::config::{AppConfig, CatalogueRef};
use ninox_ui_shared::brain_sources::{self as shared, Health, SourceEditForm, SourceStatus, SourceType};

/// Arbitrary, bump if a session needs more history on screen.
pub const MAX_LOG: usize = 50;

#[derive(Debug, Default)]
pub struct SourcesView {
    pub open: bool,
    pub cursor: usize,
    /// The detail pane beside the list (`↵` toggles).
    pub detail: bool,
    /// `AppConfig::catalogue_options()` as of the last load.
    pub catalogues: Vec<CatalogueRef>,
    pub status: HashMap<PathBuf, SourceStatus>,
    /// Recent sync results, newest first.
    pub log: VecDeque<String>,
}

impl SourcesView {
    pub fn selected(&self) -> Option<&CatalogueRef> {
        self.catalogues.get(self.cursor)
    }

    pub fn status_of(&self, c: &CatalogueRef) -> Option<&SourceStatus> {
        self.status.get(&c.path)
    }

    pub fn move_cursor(&mut self, delta: i64) {
        let last = self.catalogues.len().saturating_sub(1) as i64;
        self.cursor = (self.cursor as i64).saturating_add(delta).clamp(0, last) as usize;
    }

    /// A fresh load. The cursor stays on the same source by name, and the
    /// session-only bits of status (an in-flight sync, the last error)
    /// survive it.
    pub fn set_catalogues(&mut self, catalogues: Vec<CatalogueRef>, loaded: Vec<(PathBuf, SourceStatus)>) {
        let selected = self.selected().map(|c| c.name.clone());
        let mut status: HashMap<PathBuf, SourceStatus> = loaded.into_iter().collect();
        for (path, s) in &mut status {
            if let Some(old) = self.status.get(path) {
                s.syncing = old.syncing;
                s.last_error = old.last_error.clone();
            }
        }
        self.status = status;
        self.catalogues = catalogues;
        self.cursor = selected.and_then(|n| self.catalogues.iter().position(|c| c.name == n)).unwrap_or(self.cursor);
        self.move_cursor(0);
    }

    /// Marks `names` syncing, skipping any already in flight; returns what
    /// to sync.
    pub fn start_sync(&mut self, names: &[String]) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        for c in self.catalogues.iter().filter(|c| names.contains(&c.name)) {
            let s = self.status.entry(c.path.clone()).or_default();
            if !s.syncing {
                s.syncing = true;
                out.push((c.name.clone(), c.path.clone()));
            }
        }
        out
    }

    /// Applies a finished sync to its row and the activity log (`clock` is
    /// its "HH:MM" stamp); returns the notice to show.
    pub fn sync_done(&mut self, done: &SyncDone, clock: &str) -> Result<String, String> {
        let mut scratch = SourceStatus::default();
        let s = if self.catalogues.iter().any(|c| c.path == done.path) {
            self.status.entry(done.path.clone()).or_default()
        } else {
            &mut scratch
        };
        s.syncing = false;
        let (line, result) = match &done.result {
            Ok(report) => {
                s.last_error = None;
                let line = shared::sync_log_line(&done.name, report);
                (line.clone(), Ok(format!("synced {line}")))
            }
            Err(e) => {
                let e = shared::redact_credentials(e);
                s.last_error = Some(e.clone());
                (format!("{} — failed · {e}", done.name), Err(format!("{} sync failed: {e}", done.name)))
            }
        };
        self.log.push_front(format!("{clock}  {line}"));
        self.log.truncate(MAX_LOG);
        result
    }

    pub fn needs_attention(&self) -> usize {
        self.catalogues.iter().filter(|c| shared::health(self.status_of(c)) == Health::Failing).count()
    }
}

pub struct SyncDone {
    pub name: String,
    pub path: PathBuf,
    pub result: Result<SyncReport, String>,
}

/// `[brain]`'s own catalogue has no `[[brain.catalogues]]` entry to edit,
/// toggle or remove.
pub fn is_builtin(c: &CatalogueRef) -> bool {
    c.name == shared::DEFAULT_SOURCE
}

pub const BUILTIN_NOTE: &str = "the default source comes from [brain] in config.toml — change it there";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormField {
    Name,
    Type,
    Location,
    Region,
    Endpoint,
    CachePath,
    Cadence,
    Access,
}

impl FormField {
    pub fn is_choice(self) -> bool {
        matches!(self, Self::Type | Self::Cadence | Self::Access)
    }
}

/// The add/edit modal: the GUI's form fields plus which one has focus.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceForm {
    pub form: SourceEditForm,
    pub focus: FormField,
}

impl SourceForm {
    pub fn add() -> Self {
        Self { form: SourceEditForm::default(), focus: FormField::Name }
    }

    pub fn edit(c: &CatalogueRef) -> Self {
        Self { form: shared::form_from_catalogue(c), focus: FormField::Name }
    }

    /// The fields this source type shows, in the GUI panel's order.
    pub fn fields(&self) -> Vec<FormField> {
        use FormField::*;
        match self.form.source_type {
            SourceType::Local => vec![Name, Type, Location, Cadence, Access],
            SourceType::GithubWiki => vec![Name, Type, Location, CachePath, Cadence, Access],
            SourceType::S3 => vec![Name, Type, Location, Region, Endpoint, CachePath, Cadence, Access],
        }
    }

    pub fn label(&self, field: FormField) -> &'static str {
        match field {
            FormField::Name => "Name",
            FormField::Type => "Type",
            FormField::Location => self.form.source_type.location_label(),
            FormField::Region => "Region",
            FormField::Endpoint => "Endpoint",
            FormField::CachePath => "Local cache",
            FormField::Cadence => "Sync",
            FormField::Access => "Access",
        }
    }

    /// The value as displayed — credentials in a URL are masked even while
    /// editing, so an edit form never echoes a stored secret back.
    pub fn shown(&self, field: FormField) -> String {
        let f = &self.form;
        match field {
            FormField::Name => f.name.clone(),
            FormField::Type => f.source_type.choice_label().to_string(),
            FormField::Location => shared::redact_credentials(&f.location),
            FormField::Region => f.region.clone(),
            FormField::Endpoint => shared::redact_credentials(&f.endpoint),
            FormField::CachePath => f.cache_path.clone(),
            FormField::Cadence => shared::cadence_label(&f.cadence).to_string(),
            FormField::Access => shared::access_label(&f.access).to_string(),
        }
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        let f = &mut self.form;
        match self.focus {
            FormField::Name => Some(&mut f.name),
            FormField::Location => Some(&mut f.location),
            FormField::Region => Some(&mut f.region),
            FormField::Endpoint => Some(&mut f.endpoint),
            FormField::CachePath => Some(&mut f.cache_path),
            FormField::Type | FormField::Cadence | FormField::Access => None,
        }
    }

    pub fn next_focus(&mut self, back: bool) {
        let fields = self.fields();
        let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let n = fields.len();
        self.focus = fields[if back { (i + n - 1) % n } else { (i + 1) % n }];
    }

    /// Steps the focused choice field; a no-op on a text field.
    pub fn step(&mut self, back: bool) {
        fn cycle<T: Copy + PartialEq>(all: &[T], cur: T, back: bool) -> T {
            let i = all.iter().position(|x| *x == cur).unwrap_or(0);
            let n = all.len();
            all[if back { (i + n - 1) % n } else { (i + 1) % n }]
        }
        let f = &mut self.form;
        match self.focus {
            FormField::Type => f.source_type = cycle(&SourceType::ALL, f.source_type, back),
            FormField::Cadence => f.cadence = cycle(&shared::CADENCES, f.cadence.as_str(), back).to_string(),
            FormField::Access => f.access = cycle(&shared::ACCESSES, f.access.as_str(), back).to_string(),
            _ => return,
        }
        f.error = None;
    }

    /// Validates against `existing` (`build_catalogue_ref`, then the name
    /// check); on failure the error lands on the form.
    pub fn validate(&mut self, existing: &[CatalogueRef]) -> bool {
        let checked = shared::build_catalogue_ref(&self.form)
            .and_then(|c| shared::check_name_free(existing, &c.name, self.form.editing.as_deref()));
        self.form.error = checked.err();
        self.form.error.is_none()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FormOutcome {
    Open,
    Cancel,
    Submit,
}

/// Tab/Shift-Tab (or ↓/↑) move between fields; ←/→ (or space) step a
/// choice; typing edits a text field (Ctrl+U clears it); ↵ validates and
/// submits; esc cancels.
pub fn form_key(form: &mut SourceForm, key: KeyEvent, existing: &[CatalogueRef]) -> FormOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => return FormOutcome::Cancel,
        KeyCode::Enter => {
            if form.validate(existing) {
                return FormOutcome::Submit;
            }
        }
        KeyCode::Tab | KeyCode::Down => form.next_focus(false),
        KeyCode::BackTab | KeyCode::Up => form.next_focus(true),
        KeyCode::Left if form.focus.is_choice() => form.step(true),
        KeyCode::Right | KeyCode::Char(' ') if form.focus.is_choice() => form.step(false),
        KeyCode::Char('u') if ctrl => {
            if let Some(t) = form.text_mut() {
                t.clear();
                form.form.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Some(t) = form.text_mut() {
                t.pop();
                form.form.error = None;
            }
        }
        KeyCode::Char(c) if !ctrl => {
            if let Some(t) = form.text_mut() {
                t.push(c);
                form.form.error = None;
            }
        }
        _ => {}
    }
    FormOutcome::Open
}

fn save_error(e: impl std::fmt::Display) -> String {
    format!("could not save {}: {e}", AppConfig::config_path().display())
}

/// Saves the form through the guarded config path: an unparseable
/// `config.toml` is refused before anything is created, and the write
/// itself is `AppConfig::update` (read → apply → atomic write, `.bak`).
/// Returns the saved source's name.
pub fn save(form: &SourceEditForm) -> Result<String, String> {
    let existing = AppConfig::load_detailed().map_err(save_error)?.catalogue_options();
    let catalogue = shared::prepare_save(form, &existing)?;
    AppConfig::update(|cfg| shared::apply_save(cfg, form.editing.as_deref(), &catalogue)).map_err(save_error)?;
    Ok(catalogue.name)
}

/// Flips "Include in search"; returns the new state.
pub fn toggle(name: &str) -> Result<bool, String> {
    let (cfg, found) = AppConfig::update(|cfg| shared::apply_toggle(cfg, name)).map_err(save_error)?;
    if !found {
        return Err(BUILTIN_NOTE.to_string());
    }
    Ok(cfg.brain.catalogues.iter().find(|c| c.name == name).is_some_and(|c| c.enabled))
}

pub fn remove(name: &str) -> Result<(), String> {
    let (_, found) = AppConfig::update(|cfg| shared::apply_remove(cfg, name)).map_err(save_error)?;
    if found {
        Ok(())
    } else {
        Err(format!("no configured source named {name}"))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ninox_core::brain_sync::RemoteStatus;

    pub(crate) fn source(name: &str, remote: Option<&str>, enabled: bool) -> CatalogueRef {
        CatalogueRef {
            name: name.to_string(),
            path: PathBuf::from(format!("/brains/{name}")),
            remote: remote.map(str::to_string),
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled,
            cadence: "every_10_min".to_string(),
            access: "read_write".to_string(),
        }
    }

    pub(crate) fn sample() -> SourcesView {
        let mut v = SourcesView { open: true, ..Default::default() };
        let cats = vec![
            source("default", None, true),
            source("team-mlops", Some("https://github.com/acme/brain/wiki"), true),
            source("research-archive", Some("s3://bucket/brain"), false),
        ];
        let status = vec![
            (cats[0].path.clone(), SourceStatus { entry_count: 869, ..Default::default() }),
            (
                cats[1].path.clone(),
                SourceStatus {
                    entry_count: 42,
                    remote_status: Some(RemoteStatus {
                        remote: "https://github.com/acme/brain/wiki".into(),
                        cache_ttl_secs: 0,
                        generation: 1,
                        last_check_unix: 1,
                        pending_pushes: vec!["a.md".into(), "b.md".into()],
                        conflict_files: Vec::new(),
                    }),
                    ..Default::default()
                },
            ),
        ];
        v.set_catalogues(cats, status);
        v
    }

    fn type_text(form: &mut SourceForm, text: &str) {
        for c in text.chars() {
            form_key(form, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &[]);
        }
    }

    fn press(form: &mut SourceForm, code: KeyCode, existing: &[CatalogueRef]) -> FormOutcome {
        form_key(form, KeyEvent::new(code, KeyModifiers::NONE), existing)
    }

    #[test]
    fn cursor_clamps_and_follows_its_source_across_reloads() {
        let mut v = sample();
        v.move_cursor(10);
        assert_eq!(v.selected().unwrap().name, "research-archive");
        v.move_cursor(-10);
        assert_eq!(v.cursor, 0);
        v.cursor = 1;
        let mut reordered = v.catalogues.clone();
        reordered.swap(0, 1);
        v.set_catalogues(reordered, Vec::new());
        assert_eq!(v.selected().unwrap().name, "team-mlops");
    }

    #[test]
    fn a_reload_keeps_an_in_flight_sync_and_the_last_error() {
        let mut v = sample();
        let started = v.start_sync(&["team-mlops".into()]);
        assert_eq!(started.len(), 1);
        assert!(v.start_sync(&["team-mlops".into()]).is_empty(), "a source already syncing isn't started twice");
        let path = v.catalogues[1].path.clone();
        v.status.get_mut(&path).unwrap().last_error = Some("boom".into());
        v.set_catalogues(v.catalogues.clone(), vec![(path.clone(), SourceStatus::default())]);
        assert!(v.status[&path].syncing);
        assert_eq!(v.status[&path].last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn sync_done_records_the_result_inline_and_redacts_errors() {
        let mut v = sample();
        let path = v.catalogues[2].path.clone();
        v.start_sync(&["research-archive".into()]);
        let failed = SyncDone { name: "research-archive".into(), path: path.clone(), result: Err("GET https://k:SECRET@host/x denied".into()) };
        let msg = v.sync_done(&failed, "09:41").unwrap_err();
        assert!(!msg.contains("SECRET"));
        assert!(!v.status[&path].syncing);
        assert_eq!(v.needs_attention(), 1);
        assert!(v.log[0].starts_with("09:41  research-archive — failed"));
        assert!(!v.log[0].contains("SECRET"));

        let ok = SyncDone { name: "research-archive".into(), path: path.clone(), result: Ok(SyncReport { pulled: 3, ..Default::default() }) };
        assert_eq!(v.sync_done(&ok, "10:36").unwrap(), "synced research-archive — pulled 3 · pushed 0 · 0 conflicts");
        assert_eq!(v.status[&path].last_error, None);
        assert_eq!(v.needs_attention(), 0);
        assert!(v.log[0].starts_with("10:36"));

        let gone = PathBuf::from("/brains/removed");
        let late = SyncDone { name: "removed".into(), path: gone.clone(), result: Err("timed out".into()) };
        assert!(v.sync_done(&late, "10:40").is_err());
        assert!(!v.status.contains_key(&gone), "a sync that lands after its source was removed leaves no status behind");
    }

    #[test]
    fn form_fields_follow_the_source_type() {
        let mut f = SourceForm::add();
        assert_eq!(f.fields(), [FormField::Name, FormField::Type, FormField::Location, FormField::Cadence, FormField::Access]);
        f.focus = FormField::Type;
        f.step(false);
        assert_eq!(f.form.source_type, SourceType::GithubWiki);
        assert!(f.fields().contains(&FormField::CachePath));
        f.step(false);
        assert_eq!(f.form.source_type, SourceType::S3);
        assert!(f.fields().contains(&FormField::Region) && f.fields().contains(&FormField::Endpoint));
        f.step(false);
        assert_eq!(f.form.source_type, SourceType::Local, "Type wraps around");
        f.step(true);
        assert_eq!(f.form.source_type, SourceType::S3);
    }

    #[test]
    fn tab_walks_the_visible_fields_and_wraps() {
        let mut f = SourceForm::add();
        let order: Vec<_> = (0..5)
            .map(|_| {
                press(&mut f, KeyCode::Tab, &[]);
                f.focus
            })
            .collect();
        assert_eq!(order, [FormField::Type, FormField::Location, FormField::Cadence, FormField::Access, FormField::Name]);
        press(&mut f, KeyCode::BackTab, &[]);
        assert_eq!(f.focus, FormField::Access);
    }

    #[test]
    fn typing_edits_text_fields_only_and_arrows_step_choices() {
        let mut f = SourceForm::add();
        type_text(&mut f, "team");
        assert_eq!(f.form.name, "team");
        press(&mut f, KeyCode::Backspace, &[]);
        assert_eq!(f.form.name, "tea");
        f.focus = FormField::Cadence;
        type_text(&mut f, "x");
        assert_eq!(f.form.cadence, "manual", "a letter doesn't edit a choice");
        press(&mut f, KeyCode::Right, &[]);
        assert_eq!(f.form.cadence, "on_launch");
        press(&mut f, KeyCode::Left, &[]);
        assert_eq!(f.form.cadence, "manual");
        f.focus = FormField::Access;
        press(&mut f, KeyCode::Char(' '), &[]);
        assert_eq!(f.form.access, "read_only");
        f.focus = FormField::Name;
        form_key(&mut f, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), &[]);
        assert_eq!(f.form.name, "");
    }

    #[test]
    fn enter_validates_before_submitting_and_esc_cancels() {
        let existing = vec![source("default", None, true), source("team", None, true)];
        let mut f = SourceForm::add();
        assert_eq!(press(&mut f, KeyCode::Enter, &existing), FormOutcome::Open);
        assert!(f.form.error.as_deref().unwrap().contains("name"));
        type_text(&mut f, "team");
        assert_eq!(f.form.error, None, "editing clears the error");
        f.focus = FormField::Location;
        type_text(&mut f, "/tmp/team");
        assert_eq!(press(&mut f, KeyCode::Enter, &existing), FormOutcome::Open);
        assert!(f.form.error.as_deref().unwrap().contains("already exists"));
        f.focus = FormField::Name;
        type_text(&mut f, "2");
        assert_eq!(press(&mut f, KeyCode::Enter, &existing), FormOutcome::Submit);
        assert_eq!(press(&mut f, KeyCode::Esc, &existing), FormOutcome::Cancel);
    }

    #[test]
    fn editing_keeps_its_own_name_and_never_echoes_url_credentials() {
        let c = source("team", Some("https://bob:ghp_SECRET@github.com/acme/brain/wiki"), true);
        let mut f = SourceForm::edit(&c);
        assert_eq!(f.form.editing.as_deref(), Some("team"));
        let shown: Vec<String> = f.fields().into_iter().map(|fd| f.shown(fd)).collect();
        assert!(shown.iter().all(|v| !v.contains("ghp_SECRET")), "{shown:?}");
        assert_eq!(f.form.location, "https://bob:ghp_SECRET@github.com/acme/brain/wiki", "the stored value is untouched");
        f.form.cache_path.clear();
        assert_eq!(press(&mut f, KeyCode::Enter, std::slice::from_ref(&c)), FormOutcome::Open);
        assert!(!f.form.error.as_deref().unwrap().contains("ghp_SECRET"));
        assert_eq!(SourceForm::edit(&c).form.source_type, SourceType::GithubWiki, "a credentialed wiki URL is still a wiki");

        let clean = source("team", Some("https://github.com/acme/brain/wiki"), true);
        let mut f = SourceForm::edit(&clean);
        assert_eq!(press(&mut f, KeyCode::Enter, std::slice::from_ref(&clean)), FormOutcome::Submit);
    }

    fn write_config(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn save_goes_through_the_guarded_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = write_config(dir.path(), "port = 45123\n");
        let _iso = ninox_core::test_isolation::isolate_with_config(&cfg_path);
        let form = SourceEditForm { name: "team".into(), location: dir.path().join("team").to_string_lossy().into(), ..Default::default() };
        assert_eq!(save(&form).unwrap(), "team");
        let written = AppConfig::load_detailed().unwrap();
        assert_eq!(written.brain.catalogues.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["team"]);
        assert_eq!(written.port, 45123, "an update keeps the rest of the file");
        assert!(dir.path().join("config.toml.bak").exists(), "the previous file is backed up");
        assert!(dir.path().join("team").join(".index.db").exists(), "the folder's index is initialised");

        assert!(!toggle("team").unwrap());
        assert!(toggle("team").unwrap());
        assert_eq!(toggle(shared::DEFAULT_SOURCE).unwrap_err(), BUILTIN_NOTE);
        remove("team").unwrap();
        assert!(AppConfig::load_detailed().unwrap().brain.catalogues.is_empty());
    }

    #[test]
    fn save_refuses_an_unparseable_config_and_leaves_it_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "port = [not toml\n";
        let cfg_path = write_config(dir.path(), broken);
        let _iso = ninox_core::test_isolation::isolate_with_config(&cfg_path);
        let target = dir.path().join("team");
        let form = SourceEditForm { name: "team".into(), location: target.to_string_lossy().into(), ..Default::default() };
        assert!(save(&form).is_err());
        assert!(!target.exists(), "nothing is created before the config is known to be writable");
        assert!(toggle("team").is_err());
        assert!(remove("team").is_err());
        assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), broken);
        assert!(!dir.path().join("config.toml.bak").exists());
    }
}
