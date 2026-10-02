//! The Settings tab: `config.toml` as an editable form, mirroring the
//! desktop app's settings panel (`components::settings_panel`). Pure over
//! `AppConfig`: `fields` lists what is shown, `apply` makes one change.
//! `save` does the I/O as read → apply → write, re-loading the file right
//! before every change so edits made elsewhere (the desktop app, an
//! editor) are never clobbered.

use ninox_core::config::{AppConfig, EditorChoice, RestorePolicy, SendMechanism, ThemeVariant, TuiColors, TuiConfig};
use ninox_core::runtime::Backend;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldId {
    RuntimeBackend,
    Prefix,
    Colors,
    Theme,
    OrchHarness,
    OrchModel,
    WorkerHarness,
    WorkerModel,
    Editor,
    Harness(String),
    SendMechanism,
    PrWatch,
    RestorePolicy,
    AutoReap,
    RetentionDays,
    BrainHarvest,
    Port,
    OpenFile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Toggle,
    /// One of `options`; Enter/Space/→ step forward, ← back.
    Choice,
    /// Free text, typed in an inline editor; `options` (if any) are
    /// suggestions ←/→ step through.
    Text,
    /// A button (open the file in `$EDITOR`).
    Action,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub id: FieldId,
    pub section: &'static str,
    pub label: String,
    /// Shown value; for Text fields also what the editor opens with.
    pub value: String,
    pub kind: Kind,
    pub options: Vec<String>,
    /// Shown for the selected field.
    pub help: String,
    /// Can't be changed (claude-code is always enabled).
    pub locked: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Step { id: FieldId, back: bool },
    Set { id: FieldId, text: String },
}

impl Change {
    pub fn id(&self) -> &FieldId {
        match self {
            Self::Step { id, .. } | Self::Set { id, .. } => id,
        }
    }
}

/// Client state of the Settings tab.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsView {
    pub fields: Vec<Field>,
    pub selected: usize,
    /// The inline editor's buffer while a Text field is being edited.
    pub editing: Option<String>,
    /// Why the last change was refused; shown under the field.
    pub error: Option<String>,
    /// `config.toml` did not parse: nothing is saved until it does.
    pub load_error: Option<String>,
}

impl SettingsView {
    pub fn selected_field(&self) -> Option<&Field> {
        self.fields.get(self.selected)
    }

    pub fn reload(&mut self, loaded: Result<AppConfig, String>) {
        let id = self.selected_field().map(|f| f.id.clone());
        match loaded {
            Ok(cfg) => {
                self.fields = fields(&cfg);
                self.load_error = None;
            }
            Err(e) => {
                self.fields = fields(&AppConfig::default());
                self.load_error = Some(e);
            }
        }
        self.selected = id
            .and_then(|id| self.fields.iter().position(|f| f.id == id))
            .unwrap_or(self.selected)
            .min(self.fields.len().saturating_sub(1));
    }
}

/// An enum's TOML spelling (`"session_socket"`, `"field-notes"`).
fn slug<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn pick<T: Serialize + Copy>(all: &[T], s: &str) -> Option<T> {
    all.iter().copied().find(|v| slug(v) == s)
}

const THEMES: [ThemeVariant; 3] = [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox];
const COLORS: [TuiColors; 2] = [TuiColors::Terminal, TuiColors::FieldNotes];
const POLICIES: [RestorePolicy; 3] = [RestorePolicy::Manual, RestorePolicy::Prompt, RestorePolicy::Auto];
/// Prefix suggestions ←/→ step through; any valid `C-<key>` can be typed.
const PREFIXES: [&str; 5] = ["Ctrl+\\", "Ctrl+Space", "Ctrl+g", "Ctrl+^", "Ctrl+_"];
const ON_OFF: [&str; 2] = ["on", "off"];

fn on_off(b: bool) -> String {
    (if b { "on" } else { "off" }).to_string()
}

fn worker_harnesses(cfg: &AppConfig) -> Vec<String> {
    let reg = cfg.registry();
    reg.enabled_names().into_iter().filter(|n| reg.spec(n).worker_args.is_some()).collect()
}

fn model_suggestions(cfg: &AppConfig, harness: &str) -> Vec<String> {
    let mut v = cfg.registry().spec(harness).known_models;
    v.insert(0, String::new());
    v
}

pub fn fields(cfg: &AppConfig) -> Vec<Field> {
    let reg = cfg.registry();
    let f = |id: FieldId, section: &'static str, label: &str, value: String, kind: Kind, options: Vec<String>, help: &str| Field {
        id,
        section,
        label: label.to_string(),
        value,
        kind,
        options,
        help: help.to_string(),
        locked: false,
    };
    let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mechanism = cfg.send_mechanism();
    let mut out = vec![
        f(
            FieldId::RuntimeBackend,
            "Session runtime",
            "Runtime for new sessions",
            slug(&cfg.runtime.backend),
            Kind::Choice,
            Backend::ALL.iter().map(slug).collect(),
            cfg.runtime.backend.description(),
        ),
        f(
            FieldId::Prefix,
            "Terminal UI",
            "Prefix key",
            cfg.tui.prefix.clone(),
            Kind::Text,
            strs(&PREFIXES),
            "The chord that starts a command from inside an agent pane (`prefix ?` lists them). Applies \
             at once. Ctrl+] always returns to the fleet, whatever this is. macOS takes Ctrl+Space for input \
             sources, so the default there is Ctrl+\\.",
        ),
        f(
            FieldId::Colors,
            "Terminal UI",
            "Colours",
            slug(&cfg.tui.colors),
            Kind::Choice,
            COLORS.iter().map(slug).collect(),
            "terminal: your terminal's own colours and 16-colour palette. field-notes: the desktop app's \
             theme, in RGB.",
        ),
        f(
            FieldId::Theme,
            "Terminal UI",
            "Theme",
            slug(&cfg.theme),
            Kind::Choice,
            THEMES.iter().map(slug).collect(),
            "The desktop app's theme; the TUI paints it too when colours = field-notes.",
        ),
        f(
            FieldId::OrchHarness,
            "Agents",
            "Orchestrator harness",
            cfg.orchestrator.harness.clone(),
            Kind::Choice,
            reg.enabled_names(),
            "What new orchestrators run. Switching clears the model.",
        ),
        f(
            FieldId::OrchModel,
            "Agents",
            "Orchestrator model",
            cfg.orchestrator.model.clone().unwrap_or_default(),
            Kind::Text,
            model_suggestions(cfg, &cfg.orchestrator.harness),
            "Model id passed to the harness; empty uses the harness default. ←/→ step through the models \
             the harness lists; ↵ types one.",
        ),
        f(
            FieldId::WorkerHarness,
            "Agents",
            "Worker harness",
            cfg.worker.harness.clone(),
            Kind::Choice,
            worker_harnesses(cfg),
            "What `ninox spawn` launches when an orchestrator starts a worker (enabled, worker-capable \
             harnesses). Switching clears the model.",
        ),
        f(
            FieldId::WorkerModel,
            "Agents",
            "Worker model",
            cfg.worker.model.clone().unwrap_or_default(),
            Kind::Text,
            model_suggestions(cfg, &cfg.worker.harness),
            "Model id passed to the worker harness; empty uses the harness default. ←/→ step through the \
             models the harness lists; ↵ types one.",
        ),
        f(
            FieldId::Editor,
            "Agents",
            "Open workspaces in",
            cfg.editor.to_string(),
            Kind::Choice,
            EditorChoice::ALL.iter().map(|e| e.to_string()).collect(),
            "The editor the desktop app's \"Open in editor\" launches on a worker's workspace.",
        ),
    ];
    for name in reg.names() {
        let spec = reg.spec(&name);
        let binary = spec.binary.clone().unwrap_or_else(|| name.clone());
        let workers = if spec.worker_args.is_some() { "can run workers" } else { "orchestrators only" };
        let locked = name == AppConfig::LOCKED_HARNESS;
        let mut field = f(
            FieldId::Harness(name.clone()),
            "Harnesses",
            &name,
            on_off(spec.enabled),
            Kind::Toggle,
            strs(&ON_OFF),
            &format!(
                "Runs `{binary}`; {workers}.{}",
                if locked { " The default harness; always on." } else { " Off hides it from the harness pickers." }
            ),
        );
        field.locked = locked;
        out.push(field);
    }
    out.extend([
        f(
            FieldId::SendMechanism,
            "Messaging",
            "Send mechanism",
            slug(&mechanism),
            Kind::Choice,
            SendMechanism::ALL.iter().map(slug).collect(),
            mechanism.description(),
        ),
        f(
            FieldId::PrWatch,
            "Messaging",
            "Consolidated PR watching",
            on_off(cfg.pr_watch.enabled),
            Kind::Toggle,
            strs(&ON_OFF),
            "Off: PR/CI state is polled per session via REST. On: one batched GraphQL query per tick \
             covers every watched PR, including extra PRs registered with `ninox open --pr`.",
        ),
        f(
            FieldId::RestorePolicy,
            "Fleet",
            "Restore after a reboot",
            slug(&cfg.fleet.restore_policy),
            Kind::Choice,
            POLICIES.iter().map(slug).collect(),
            "manual: nothing until you run `ninox fleet restore`. prompt: nx offers to restore. auto: the \
             engine restores interrupted sessions as it starts.",
        ),
        f(
            FieldId::AutoReap,
            "Fleet",
            "Reap workers when their PR merges",
            on_off(cfg.auto_reap.enabled),
            Kind::Toggle,
            strs(&ON_OFF),
            "On: a merged worker's session and worktree are removed at once. Off: they stay for \
             post-merge checks until reaped.",
        ),
        f(
            FieldId::RetentionDays,
            "Fleet",
            "Keep finished sessions (days)",
            cfg.session_retention.done_retention_days.to_string(),
            Kind::Text,
            Vec::new(),
            "How long a done or ended session stays on the board before it is purged.",
        ),
        f(
            FieldId::BrainHarvest,
            "Fleet",
            "Harvest PRs into the brain",
            on_off(cfg.brain_harvest.enabled),
            Kind::Toggle,
            strs(&ON_OFF),
            "When a worker opens a PR, a short `claude -p` run reads the diff and writes what it learns \
             into the brain.",
        ),
        f(
            FieldId::Port,
            "Engine",
            "Port",
            cfg.port.to_string(),
            Kind::Text,
            Vec::new(),
            "The engine's HTTP port. Takes effect the next time the engine starts.",
        ),
        f(
            FieldId::OpenFile,
            "File",
            "Open config.toml in $EDITOR",
            String::new(),
            Kind::Action,
            Vec::new(),
            "Everything else (brain, catalogues, harness definitions, the GitHub token) lives in the \
             file. nx suspends while the editor runs and reloads when it exits.",
        ),
    ]);
    out
}

/// Make one change. `Ok` carries a short confirmation for the footer.
pub fn apply(cfg: &mut AppConfig, change: &Change) -> Result<String, String> {
    let field = fields(cfg).into_iter().find(|f| &f.id == change.id()).ok_or("no such setting")?;
    if field.locked {
        return Err(format!("{} can't be turned off", field.label));
    }
    let text = match change {
        Change::Set { text, .. } => text.trim().to_string(),
        Change::Step { back, .. } => {
            let opts = &field.options;
            if opts.is_empty() {
                return Err(format!("{} has no choices", field.label));
            }
            let n = opts.len();
            let next = match opts.iter().position(|o| *o == field.value) {
                Some(i) if *back => (i + n - 1) % n,
                Some(i) => (i + 1) % n,
                None if *back => n - 1,
                None => 0,
            };
            opts[next].clone()
        }
    };
    set(cfg, &field, &text)?;
    let shown = if text.is_empty() { "default".to_string() } else { text };
    Ok(format!("{} → {shown}", field.label))
}

fn set(cfg: &mut AppConfig, field: &Field, s: &str) -> Result<(), String> {
    let bad = || format!("{s:?} is not one of: {}", field.options.join(", "));
    match &field.id {
        FieldId::RuntimeBackend => cfg.runtime.backend = pick(&Backend::ALL, s).ok_or_else(bad)?,
        FieldId::Prefix => {
            TuiConfig { prefix: s.to_string(), ..cfg.tui.clone() }.prefix_byte()?;
            cfg.tui.prefix = s.to_string();
        }
        FieldId::Colors => cfg.tui.colors = pick(&COLORS, s).ok_or_else(bad)?,
        FieldId::Theme => cfg.theme = pick(&THEMES, s).ok_or_else(bad)?,
        FieldId::OrchHarness | FieldId::WorkerHarness => {
            if !field.options.iter().any(|o| o == s) {
                return Err(bad());
            }
            let agent = if field.id == FieldId::OrchHarness { &mut cfg.orchestrator } else { &mut cfg.worker };
            agent.set_harness(s);
        }
        FieldId::OrchModel | FieldId::WorkerModel => {
            if s.chars().any(char::is_whitespace) {
                return Err("a model id has no spaces".into());
            }
            let agent = if field.id == FieldId::OrchModel { &mut cfg.orchestrator } else { &mut cfg.worker };
            agent.model = (!s.is_empty()).then(|| s.to_string());
        }
        FieldId::Editor => {
            cfg.editor = EditorChoice::ALL.iter().copied().find(|e| e.to_string() == s).ok_or_else(bad)?;
        }
        FieldId::Harness(name) => {
            if (s == "on") != cfg.registry().spec(name).enabled {
                cfg.toggle_harness(name);
            }
        }
        FieldId::SendMechanism => cfg.set_send_mechanism(pick(&SendMechanism::ALL, s).ok_or_else(bad)?),
        FieldId::PrWatch => cfg.pr_watch.enabled = s == "on",
        FieldId::RestorePolicy => cfg.fleet.restore_policy = pick(&POLICIES, s).ok_or_else(bad)?,
        FieldId::AutoReap => cfg.auto_reap.enabled = s == "on",
        FieldId::BrainHarvest => cfg.brain_harvest.enabled = s == "on",
        FieldId::RetentionDays => {
            cfg.session_retention.done_retention_days =
                s.parse().map_err(|_| format!("{s:?} is not a whole number of days"))?;
        }
        FieldId::Port => cfg.port = s.parse().map_err(|_| format!("{s:?} is not a port (0–65535)"))?,
        FieldId::OpenFile => return Err("not a value".into()),
    }
    Ok(())
}

/// Load `config.toml`, or say why it can't be read.
pub fn load() -> Result<AppConfig, String> {
    AppConfig::load().map_err(|e| format!("{} doesn't parse: {e}", AppConfig::config_path().display()))
}

/// Read → apply → write. A file that doesn't parse is never overwritten.
pub fn save(change: &Change) -> Result<(AppConfig, String), String> {
    let mut cfg = load()?;
    let msg = apply(&mut cfg, change)?;
    cfg.save().map_err(|e| format!("could not save {}: {e}", AppConfig::config_path().display()))?;
    Ok((cfg, msg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(fs: &'a [Field], id: &FieldId) -> &'a Field {
        fs.iter().find(|f| &f.id == id).unwrap()
    }

    fn step(cfg: &mut AppConfig, id: FieldId, back: bool) -> Result<String, String> {
        apply(cfg, &Change::Step { id, back })
    }

    fn set_text(cfg: &mut AppConfig, id: FieldId, text: &str) -> Result<String, String> {
        apply(cfg, &Change::Set { id, text: text.into() })
    }

    #[test]
    fn the_form_covers_the_desktop_panel_and_the_tui() {
        let fs = fields(&AppConfig::default());
        for id in [
            FieldId::RuntimeBackend,
            FieldId::Prefix,
            FieldId::Colors,
            FieldId::Theme,
            FieldId::OrchHarness,
            FieldId::OrchModel,
            FieldId::WorkerHarness,
            FieldId::WorkerModel,
            FieldId::Editor,
            FieldId::Harness("claude-code".into()),
            FieldId::Harness("codex".into()),
            FieldId::SendMechanism,
            FieldId::PrWatch,
            FieldId::RestorePolicy,
            FieldId::RetentionDays,
            FieldId::Port,
            FieldId::OpenFile,
        ] {
            assert!(fs.iter().any(|f| f.id == id), "{id:?} missing");
        }
        let backend = field(&fs, &FieldId::RuntimeBackend);
        assert_eq!(backend.options, ["tmux", "ptyd"]);
        assert_eq!(backend.help, Backend::Tmux.description());
        assert_eq!(field(&fs, &FieldId::SendMechanism).options, ["session_socket", "inbox", "keystrokes"]);
        assert!(field(&fs, &FieldId::Harness("claude-code".into())).locked);
        let sections: Vec<&str> = fs.iter().map(|f| f.section).collect();
        let mut seen = Vec::new();
        for s in sections {
            if seen.last() != Some(&s) {
                assert!(!seen.contains(&s), "section {s} is split");
                seen.push(s);
            }
        }
    }

    #[test]
    fn choices_cycle_both_ways_and_wrap() {
        let mut cfg = AppConfig::default();
        step(&mut cfg, FieldId::RuntimeBackend, false).unwrap();
        assert_eq!(cfg.runtime.backend, Backend::Ptyd);
        step(&mut cfg, FieldId::RuntimeBackend, false).unwrap();
        assert_eq!(cfg.runtime.backend, Backend::Tmux);
        step(&mut cfg, FieldId::RestorePolicy, true).unwrap();
        assert_eq!(cfg.fleet.restore_policy, RestorePolicy::Auto);
        step(&mut cfg, FieldId::SendMechanism, false).unwrap();
        assert_eq!(cfg.send_mechanism(), SendMechanism::Inbox);
        step(&mut cfg, FieldId::PrWatch, false).unwrap();
        assert!(cfg.pr_watch.enabled);
        step(&mut cfg, FieldId::Colors, false).unwrap();
        assert_eq!(cfg.tui.colors, TuiColors::FieldNotes);
        let msg = step(&mut cfg, FieldId::Editor, false).unwrap();
        assert_eq!((cfg.editor, msg.as_str()), (EditorChoice::Cursor, "Open workspaces in → Cursor"));
    }

    #[test]
    fn harness_toggles_write_the_full_spec_and_claude_code_is_locked() {
        let mut cfg = AppConfig::default();
        let builtin = cfg.registry().spec("codex");
        step(&mut cfg, FieldId::Harness("codex".into()), false).unwrap();
        let saved = &cfg.harnesses["codex"];
        assert_ne!(saved.enabled, builtin.enabled);
        assert_eq!(saved.interactive_args, builtin.interactive_args, "the builtin's args survive");
        assert!(step(&mut cfg, FieldId::Harness("claude-code".into()), false).is_err());
        assert!(cfg.registry().spec("claude-code").enabled);
    }

    #[test]
    fn switching_harness_clears_the_model_and_models_take_free_text() {
        let mut cfg = AppConfig::default();
        set_text(&mut cfg, FieldId::WorkerModel, " claude-haiku-4-5 ").unwrap();
        assert_eq!(cfg.worker.model.as_deref(), Some("claude-haiku-4-5"));
        set_text(&mut cfg, FieldId::WorkerModel, "").unwrap();
        assert_eq!(cfg.worker.model, None, "empty means the harness default");
        cfg.worker.model = Some("m".into());
        if !cfg.registry().spec("codex").enabled {
            cfg.toggle_harness("codex");
        }
        set_text(&mut cfg, FieldId::WorkerHarness, "codex").unwrap();
        assert_eq!((cfg.worker.harness.as_str(), cfg.worker.model.as_deref()), ("codex", None));
        assert!(set_text(&mut cfg, FieldId::OrchHarness, "nope").is_err());
        assert!(set_text(&mut cfg, FieldId::OrchModel, "two words").is_err());
    }

    #[test]
    fn text_fields_are_validated() {
        let mut cfg = AppConfig::default();
        let err = set_text(&mut cfg, FieldId::Prefix, "Ctrl+b").unwrap_err();
        assert!(err.contains("screen/tmux"), "{err}");
        assert!(set_text(&mut cfg, FieldId::Prefix, "x").is_err());
        set_text(&mut cfg, FieldId::Prefix, "Ctrl+g").unwrap();
        assert_eq!(cfg.tui.prefix_byte(), Ok(0x07));
        assert!(set_text(&mut cfg, FieldId::Port, "70000").is_err());
        set_text(&mut cfg, FieldId::Port, "9123").unwrap();
        assert_eq!(cfg.port, 9123);
        assert!(set_text(&mut cfg, FieldId::RetentionDays, "-1").is_err());
        set_text(&mut cfg, FieldId::RetentionDays, "7").unwrap();
        assert_eq!(cfg.session_retention.done_retention_days, 7);
    }

    #[test]
    fn saving_rereads_the_file_and_keeps_what_the_form_does_not_show() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        crate::test_fixtures::with_env_override("NINOX_CONFIG", &path, || {
            std::fs::write(
                &path,
                "port = 8080\nfont_size = 15.0\ngithub_token = \"ghp_keep\"\n\n[brain]\nremote = \"s3://b/p\"\n\n\
                 [harnesses.fake]\nenabled = true\nbinary = \"sh\"\nworker_args = [\"-c\", \"cat\"]\n",
            )
            .unwrap();
            let (_, msg) = save(&Change::Step { id: FieldId::RuntimeBackend, back: false }).unwrap();
            assert!(msg.contains("ptyd"), "{msg}");
            // Edited elsewhere between two changes: the next save keeps it.
            let mut other = AppConfig::load().unwrap();
            other.zoom = 1.5;
            other.save().unwrap();
            save(&Change::Set { id: FieldId::Port, text: "9999".into() }).unwrap();

            let back = AppConfig::load().unwrap();
            assert_eq!(back.runtime.backend, Backend::Ptyd);
            assert_eq!(back.port, 9999);
            assert_eq!(back.zoom, 1.5);
            assert_eq!(back.font_size, 15.0);
            assert_eq!(back.github_token.as_deref(), Some("ghp_keep"));
            assert_eq!(back.brain.remote.as_deref(), Some("s3://b/p"));
            assert_eq!(back.harnesses["fake"].binary.as_deref(), Some("sh"));

            std::fs::write(&path, "port = [nope").unwrap();
            assert!(save(&Change::Set { id: FieldId::Port, text: "1".into() }).unwrap_err().contains("doesn't parse"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "port = [nope", "a broken file is left alone");
        });
    }

    #[test]
    fn reload_keeps_the_selection_by_field() {
        let mut v = SettingsView::default();
        v.reload(Ok(AppConfig::default()));
        v.selected = v.fields.iter().position(|f| f.id == FieldId::Port).unwrap();
        let mut cfg = AppConfig::default();
        cfg.toggle_harness("codex");
        cfg.harnesses.insert("zzz".into(), Default::default());
        v.reload(Ok(cfg));
        assert_eq!(v.selected_field().map(|f| &f.id), Some(&FieldId::Port));
        v.reload(Err("bad".into()));
        assert_eq!(v.load_error.as_deref(), Some("bad"));
    }
}
