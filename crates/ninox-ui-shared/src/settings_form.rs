//! `config.toml` as an editable form, shared by the TUI's Settings tab and
//! the desktop app's settings panel so both validate identically. Pure over
//! `AppConfig`: `fields` lists what is shown, `apply` makes one change.
//! `save` does the I/O as read → apply → write, re-loading the file right
//! before every change so edits made elsewhere (the desktop app, an
//! editor) are never clobbered.

use ninox_core::config::{AppConfig, CompletionMode, EditorChoice, RestorePolicy, SendMechanism, ThemeVariant, TuiColors, TuiConfig, UpdateChannel};
use ninox_core::runtime::Backend;
use serde::Serialize;

pub const DEFAULT_MODELS_SECTION: &str = "Agents · default models";

/// Shown under both default-model settings, in the TUI and the desktop app.
pub const MODEL_OVERRIDES: &str = "A worker type's own model, or a model picked in the Spawn dialog, \
     overrides it; running sessions keep the model they started with.";

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
    /// Concurrent session cap for a location (`None` = this machine, else a
    /// `MachineProfile::id`).
    SessionLimit(Option<String>),
    SessionLimitUnlimited(Option<String>),
    RestorePolicy,
    WorkerCompletion,
    RetentionDays,
    GithubPollInterval,
    GithubDetailRefresh,
    BrainHarvest,
    UpdateChannel,
    Port,
    RestartServer,
    RestartAffected,
    OpenFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    #[default]
    Appearance,
    Agents,
    Harnesses,
    Messaging,
    Runtime,
    SessionLimits,
    Terminal,
    Fleet,
    Engine,
    Version,
}

impl SettingsSection {
    /// The section list, grouped; shared so the desktop and terminal apps
    /// show the same sections in the same order.
    pub const GROUPS: [(&'static str, &'static [SettingsSection]); 3] = [
        ("General", &[Self::Appearance, Self::Agents, Self::Harnesses, Self::Messaging]),
        ("Sessions", &[Self::Runtime, Self::SessionLimits, Self::Terminal, Self::Fleet]),
        ("System", &[Self::Engine, Self::Version]),
    ];

    /// Every section in bar order.
    pub fn all() -> Vec<SettingsSection> {
        Self::GROUPS.iter().flat_map(|(_, ss)| ss.iter().copied()).collect()
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Agents => "Agents",
            Self::Harnesses => "Harnesses",
            Self::Messaging => "Messaging",
            Self::Runtime => "Session runtime",
            Self::SessionLimits => "Session limits",
            Self::Terminal => "Terminal",
            Self::Fleet => "Fleet",
            Self::Engine => "Engine",
            Self::Version => "Version",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Self::Appearance => "How the desktop app looks.",
            Self::Agents => "What new orchestrators and workers run, and where workspaces open.",
            Self::Harnesses => "The agent CLIs ninox can launch. Off hides one from every picker.",
            Self::Messaging => "How orchestrators and workers reach each other, and how PRs are watched.",
            Self::Runtime => "Which process hosts new sessions.",
            Self::SessionLimits => "How many agent sessions may run at once on each machine.",
            Self::Terminal => "The terminal UI (nx) and agent panes.",
            Self::Fleet => "Restoring, reaping and keeping sessions.",
            Self::Engine => "The background server every ninox app talks to.",
            Self::Version => "This build, and updates.",
        }
    }
}

impl FieldId {
    /// The settings section this field is shown under. `None` for the
    /// config-file button, which is not a setting.
    pub fn section(&self) -> Option<SettingsSection> {
        use SettingsSection as S;
        Some(match self {
            Self::Theme | Self::Colors => S::Appearance,
            Self::OrchHarness | Self::OrchModel | Self::WorkerHarness | Self::WorkerModel | Self::Editor => S::Agents,
            Self::Harness(_) => S::Harnesses,
            Self::SendMechanism | Self::PrWatch => S::Messaging,
            Self::RuntimeBackend => S::Runtime,
            Self::SessionLimit(_) | Self::SessionLimitUnlimited(_) => S::SessionLimits,
            Self::Prefix => S::Terminal,
            Self::RestorePolicy
            | Self::WorkerCompletion
            | Self::RetentionDays
            | Self::GithubPollInterval
            | Self::GithubDetailRefresh
            | Self::BrainHarvest => S::Fleet,
            Self::Port | Self::RestartServer | Self::RestartAffected => S::Engine,
            Self::UpdateChannel => S::Version,
            Self::OpenFile => return None,
        })
    }
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
    /// Shown in place of an empty Text value.
    pub placeholder: String,
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

/// An enum's TOML spelling (`"session_socket"`, `"field-notes"`).
fn slug<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn pick<T: Serialize + Copy>(all: &[T], s: &str) -> Option<T> {
    all.iter().copied().find(|v| slug(v) == s)
}

const THEMES: [ThemeVariant; 3] = [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox];
const COLORS: [TuiColors; 2] = [TuiColors::Terminal, TuiColors::FieldNotes];
const CHANNELS: [UpdateChannel; 2] = [UpdateChannel::Stable, UpdateChannel::Alpha];
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
        placeholder: "harness default".to_string(),
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
            DEFAULT_MODELS_SECTION,
            "Orchestrator harness",
            cfg.orchestrator.harness.clone(),
            Kind::Choice,
            reg.enabled_names(),
            "What new orchestrators run. Switching clears the model.",
        ),
        f(
            FieldId::OrchModel,
            DEFAULT_MODELS_SECTION,
            "Orchestrator model",
            cfg.orchestrator.model.clone().unwrap_or_default(),
            Kind::Text,
            model_suggestions(cfg, &cfg.orchestrator.harness),
            &format!("Default model for new orchestrators; empty uses the default shown — ninox's built-in \
             pick for the harness, else the harness's own. {MODEL_OVERRIDES} \
             ←/→ step through the models the harness lists; ↵ types one."),
        ),
        f(
            FieldId::WorkerHarness,
            DEFAULT_MODELS_SECTION,
            "Worker harness",
            cfg.worker.harness.clone(),
            Kind::Choice,
            worker_harnesses(cfg),
            "What `ninox spawn` launches when an orchestrator starts a worker (enabled, worker-capable \
             harnesses). Switching clears the model.",
        ),
        f(
            FieldId::WorkerModel,
            DEFAULT_MODELS_SECTION,
            "Worker model",
            cfg.worker.model.clone().unwrap_or_default(),
            Kind::Text,
            model_suggestions(cfg, &cfg.worker.harness),
            &format!("Default model for every other session (workers, standalone sessions, the Ninox prompt); \
             empty uses the default shown — ninox's built-in pick for the harness, else the harness's own. {MODEL_OVERRIDES} ←/→ step through the models the harness \
             lists; ↵ types one."),
        ),
        f(
            FieldId::Editor,
            "Agents · workspaces",
            "Open workspaces in",
            cfg.editor.to_string(),
            Kind::Choice,
            EditorChoice::ALL.iter().map(|e| e.to_string()).collect(),
            "The editor \"open in editor\" launches on a worker's workspace — the desktop app's button, or \
             nx's e key. Neovim is terminal-only: nx suspends and runs it there; the desktop button is \
             disabled for it.",
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
    ]);
    let mut locations: Vec<(Option<String>, String)> = vec![(None, "This machine".to_string())];
    locations.extend(cfg.remote_machines.machines.iter().map(|m| (Some(m.id.clone()), m.label.clone())));
    for (machine, label) in locations {
        let limit = cfg.limits.limit_for(machine.as_deref());
        out.push(f(
            FieldId::SessionLimit(machine.clone()),
            "Session limits",
            &format!("{label}: max sessions"),
            limit.map_or_else(|| "unlimited".to_string(), |n| n.to_string()),
            Kind::Text,
            Vec::new(),
            "How many agent sessions may run at once here. Spawns over the limit are queued and start \
             in order as sessions finish. Applies within a few seconds; lowering it never stops a \
             running session.",
        ));
        out.push(f(
            FieldId::SessionLimitUnlimited(machine),
            "Session limits",
            &format!("{label}: unlimited"),
            on_off(limit.is_none()),
            Kind::Toggle,
            strs(&ON_OFF),
            "On: no cap here; nothing is queued.",
        ));
    }
    out.extend([
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
            FieldId::WorkerCompletion,
            "Fleet",
            "Worker completion",
            cfg.completion_mode().as_str().to_string(),
            Kind::Choice,
            CompletionMode::ALL.iter().map(|m| m.as_str().to_string()).collect(),
            "self: a worker finishes itself with `ninox complete`; a merge only nudges it. reap_on_merge: \
             Ninox removes a worker at merge when its Definition of done is only the PR merging. manual: \
             a merged worker stays until you or the orchestrator reaps it.",
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
            FieldId::GithubPollInterval,
            "Fleet",
            "GitHub poll interval (seconds)",
            cfg.github.poll_interval_secs.to_string(),
            Kind::Text,
            Vec::new(),
            "How often every tracked PR's light status is read (minimum 15). Raise it for fleets with \
             many PRs. Slows itself further when the shared GitHub token runs low. Applies within a \
             poll or two.",
        ),
        f(
            FieldId::GithubDetailRefresh,
            "Fleet",
            "GitHub full re-read (seconds)",
            cfg.github.detail_refresh_secs.to_string(),
            Kind::Text,
            Vec::new(),
            "A PR's checks, reviews and comments are normally re-read only when it changes; this is the \
             longest they may go unread regardless (minimum 15).",
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
            FieldId::UpdateChannel,
            "Version",
            "Update channel",
            slug(&cfg.update.channel),
            Kind::Choice,
            CHANNELS.iter().map(slug).collect(),
            "Which release stream `ninox update` and the update check follow. Alpha builds come from the `v1` \
             branch; switching back to stable never downgrades — run `ninox update --channel stable --version X.Y.Z` for that.",
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
            FieldId::RestartServer,
            "Engine",
            "Restart server",
            String::new(),
            Kind::Action,
            Vec::new(),
            "Restart the background server so it runs the current build and re-reads credentials it \
             captured at start (an env token, aws sso, gh). Running sessions keep running; after a \
             harness login, also restart the sessions with login errors.",
        ),
        f(
            FieldId::RestartAffected,
            "Engine",
            "Restart sessions with login errors",
            String::new(),
            Kind::Action,
            Vec::new(),
            "Relaunch, in place, every session whose pane shows a harness login error. Log in again \
             first (e.g. `claude /login`): a running session keeps the credentials it started with.",
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
    use ninox_core::model_resolution::{unset_setting_placeholder, Role};
    for fd in &mut out {
        match fd.id {
            FieldId::OrchModel   => fd.placeholder = unset_setting_placeholder(Role::Orchestrator, cfg),
            FieldId::WorkerModel => fd.placeholder = unset_setting_placeholder(Role::Worker, cfg),
            _ => {}
        }
    }
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
        FieldId::SessionLimit(machine) => {
            let limit = match s {
                "0" => None,
                s if s.eq_ignore_ascii_case("unlimited") => None,
                n => Some(n.parse::<u32>().map_err(|_| format!("{s:?} is not a whole number (or `unlimited`)"))?),
            };
            cfg.limits.set_limit(machine.as_deref(), limit);
        }
        FieldId::SessionLimitUnlimited(machine) => {
            // Arbitrary starting cap when switching Unlimited off; editable
            // right after.
            let limit = (s != "on").then(|| cfg.limits.limit_for(machine.as_deref()).unwrap_or(6));
            cfg.limits.set_limit(machine.as_deref(), limit);
        }
        FieldId::RestorePolicy => cfg.fleet.restore_policy = pick(&POLICIES, s).ok_or_else(bad)?,
        FieldId::WorkerCompletion => {
            cfg.worker_completion.mode = Some(CompletionMode::parse(s).ok_or_else(bad)?);
        }
        FieldId::BrainHarvest => cfg.brain_harvest.enabled = s == "on",
        FieldId::UpdateChannel => cfg.update.channel = pick(&CHANNELS, s).ok_or_else(bad)?,
        FieldId::RetentionDays => {
            cfg.session_retention.done_retention_days =
                s.parse().map_err(|_| format!("{s:?} is not a whole number of days"))?;
        }
        FieldId::GithubPollInterval | FieldId::GithubDetailRefresh => {
            let min = ninox_core::config::GITHUB_MIN_POLL_INTERVAL_SECS;
            let secs: u64 = s.parse().map_err(|_| format!("{s:?} is not a whole number of seconds"))?;
            if secs < min {
                return Err(format!("the minimum is {min} seconds"));
            }
            if field.id == FieldId::GithubPollInterval {
                cfg.github.poll_interval_secs = secs;
            } else {
                cfg.github.detail_refresh_secs = secs;
            }
        }
        FieldId::Port => cfg.port = s.parse().map_err(|_| format!("{s:?} is not a port (0–65535)"))?,
        FieldId::OpenFile | FieldId::RestartServer | FieldId::RestartAffected => return Err("not a value".into()),
    }
    Ok(())
}

/// Load `config.toml`, or say why it can't be read.
pub fn load() -> Result<AppConfig, String> {
    AppConfig::load().map_err(|e| format!("{} doesn't parse: {e}", AppConfig::config_path().display()))
}

/// Read → apply → write through `AppConfig::update`. A file that doesn't
/// parse is never overwritten.
pub fn save(change: &Change) -> Result<(AppConfig, String), String> {
    load()?;
    let (cfg, msg) = AppConfig::update(|cfg| apply(cfg, change))
        .map_err(|e| format!("could not save {}: {e}", AppConfig::config_path().display()))?;
    Ok((cfg, msg?))
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
            FieldId::UpdateChannel,
            FieldId::RestorePolicy,
            FieldId::RetentionDays,
            FieldId::GithubPollInterval,
            FieldId::GithubDetailRefresh,
            FieldId::Port,
            FieldId::RestartServer,
            FieldId::RestartAffected,
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
    fn every_field_but_the_file_button_has_a_section_and_every_section_a_field() {
        let fs = fields(&AppConfig::default());
        for f in &fs {
            assert_eq!(f.id.section().is_none(), f.id == FieldId::OpenFile, "{:?}", f.id);
        }
        for (_, sections) in SettingsSection::GROUPS {
            for s in sections {
                assert!(fs.iter().any(|f| f.id.section() == Some(*s)), "{s:?} has no field");
            }
        }
    }

    #[test]
    fn section_groups_and_titles_match_the_desktop_settings_page() {
        let shape: Vec<(&str, Vec<&str>)> =
            SettingsSection::GROUPS.iter().map(|(g, ss)| (*g, ss.iter().map(|s| s.title()).collect())).collect();
        assert_eq!(
            shape,
            [
                ("General", vec!["Appearance", "Agents", "Harnesses", "Messaging"]),
                ("Sessions", vec!["Session runtime", "Session limits", "Terminal", "Fleet"]),
                ("System", vec!["Engine", "Version"]),
            ]
        );
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
        assert!(!cfg.pr_watch.enabled, "stepping the default-on toggle turns it off");
        step(&mut cfg, FieldId::UpdateChannel, false).unwrap();
        assert_eq!(cfg.update.channel, UpdateChannel::Alpha);
        step(&mut cfg, FieldId::UpdateChannel, false).unwrap();
        assert_eq!(cfg.update.channel, UpdateChannel::Stable, "wraps back to the default");
        step(&mut cfg, FieldId::Colors, false).unwrap();
        assert_eq!(cfg.tui.colors, TuiColors::FieldNotes);
        let msg = step(&mut cfg, FieldId::Editor, false).unwrap();
        assert_eq!((cfg.editor, msg.as_str()), (EditorChoice::Cursor, "Open workspaces in → Cursor"));
        let msg = step(&mut cfg, FieldId::Editor, false).unwrap();
        assert_eq!((cfg.editor, msg.as_str()), (EditorChoice::Neovim, "Open workspaces in → Neovim"));
        let msg = step(&mut cfg, FieldId::Editor, false).unwrap();
        assert_eq!((cfg.editor, msg.as_str()), (EditorChoice::VsCode, "Open workspaces in → VS Code"), "wraps back around");
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
        assert!(set_text(&mut cfg, FieldId::GithubPollInterval, "14").is_err());
        assert!(set_text(&mut cfg, FieldId::GithubPollInterval, "x").is_err());
        set_text(&mut cfg, FieldId::GithubPollInterval, "90").unwrap();
        set_text(&mut cfg, FieldId::GithubDetailRefresh, "900").unwrap();
        assert_eq!((cfg.github.poll_interval_secs, cfg.github.detail_refresh_secs), (90, 900));
        assert!(set_text(&mut cfg, FieldId::RetentionDays, "-1").is_err());
        set_text(&mut cfg, FieldId::RetentionDays, "7").unwrap();
        assert_eq!(cfg.session_retention.done_retention_days, 7);
    }

    #[test]
    fn session_limits_take_a_number_or_unlimited_per_location() {
        let mut cfg = AppConfig::default();
        cfg.remote_machines.machines.push(ninox_core::config::MachineProfile {
            id: "m1".into(), label: "gpu-box".into(), ssh_target: "gpu".into(),
            remote_session: "default".into(), enabled: true,
        });
        let fs = fields(&cfg);
        assert_eq!(field(&fs, &FieldId::SessionLimit(None)).value, "6");
        assert_eq!(field(&fs, &FieldId::SessionLimit(Some("m1".into()))).value, "unlimited");
        assert!(field(&fs, &FieldId::SessionLimit(Some("m1".into()))).label.starts_with("gpu-box"));

        set_text(&mut cfg, FieldId::SessionLimit(None), "3").unwrap();
        assert_eq!(cfg.limits.limit_for(None), Some(3));
        assert!(set_text(&mut cfg, FieldId::SessionLimit(None), "lots").is_err());
        set_text(&mut cfg, FieldId::SessionLimit(Some("m1".into())), "4").unwrap();
        assert_eq!(cfg.limits.limit_for(Some("m1")), Some(4));

        step(&mut cfg, FieldId::SessionLimitUnlimited(None), false).unwrap();
        assert_eq!(cfg.limits.limit_for(None), None);
        step(&mut cfg, FieldId::SessionLimitUnlimited(None), false).unwrap();
        assert_eq!(cfg.limits.limit_for(None), Some(6), "switching Unlimited off starts from a cap");
        set_text(&mut cfg, FieldId::SessionLimit(None), "unlimited").unwrap();
        assert_eq!(cfg.limits.limit_for(None), None);
    }

    #[test]
    fn a_session_limit_saves_through_the_guarded_update() {
        let _iso = ninox_core::test_isolation::isolate();
        save(&Change::Set { id: FieldId::SessionLimit(None), text: "2".into() }).unwrap();
        assert_eq!(AppConfig::load().unwrap().limits.limit_for(None), Some(2));
        std::fs::write(AppConfig::config_path(), "limits = [").unwrap();
        assert!(save(&Change::Set { id: FieldId::SessionLimit(None), text: "3".into() }).is_err());
        assert_eq!(std::fs::read_to_string(AppConfig::config_path()).unwrap(), "limits = [");
    }

    #[test]
    fn saving_rereads_the_file_and_keeps_what_the_form_does_not_show() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = ninox_core::test_isolation::isolate_with_config(&path);
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
        }
    }
}
