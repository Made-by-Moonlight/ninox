//! The Worker Types tab: `[[worker_types]]` as a list+form editor, per
//! item 7's management-screen design
//! (`docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`)
//! and the worker-types-backend spec's data model
//! (`docs/superpowers/specs/2026-10-22-worker-types-backend-design.md`).
//!
//! Mirrors `settings.rs`'s shape (`Field`/`Change`/read-apply-write-reload)
//! but scoped to one `WorkerTypeSpec` at a time, plus a name list: builtin
//! Orchestrator/Implementor/Researcher/Evaluator specs are always present
//! (`WorkerTypeRegistry::from_config(&[])`, shown via `user_facing_names()`
//! so a future hidden/system type never appears here), overlaid/extended by
//! `AppConfig::worker_types`. `custom` names (customized builtins and pure
//! custom types) are visually distinguished from plain builtins — see
//! `WorkerTypesView::names`/`custom`.

use std::collections::HashSet;
use std::path::PathBuf;

use ninox_core::config::AppConfig;
use ninox_core::store::Store;
use ninox_core::worker_type::{display_name, is_system_type, WorkerTypeSpec, SYSTEM_APPLIES_NOTE};
use ninox_core::worker_type_cli::{self, Fields};

pub use super::settings::Kind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleList {
    Allow,
    Deny,
    Ask,
}

impl RuleList {
    fn label(self) -> &'static str {
        match self {
            Self::Allow => "Allow rules",
            Self::Deny => "Deny rules",
            Self::Ask => "Ask rules",
        }
    }

    fn rules(self, spec: &WorkerTypeSpec) -> &Vec<String> {
        match self {
            Self::Allow => &spec.permissions.allow,
            Self::Deny => &spec.permissions.deny,
            Self::Ask => &spec.permissions.ask,
        }
    }

    fn rules_mut(self, spec: &mut WorkerTypeSpec) -> &mut Vec<String> {
        match self {
            Self::Allow => &mut spec.permissions.allow,
            Self::Deny => &mut spec.permissions.deny,
            Self::Ask => &mut spec.permissions.ask,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldId {
    Name,
    UseWhen,
    Brief,
    Harness,
    Model,
    PermEnabled,
    /// One rule row; `None` is the placeholder row of an empty list.
    Rule(RuleList, Option<usize>),
    /// The can-spawn summary row above its options.
    CanSpawn,
    /// One can-spawn option: `*` or a type name.
    CanSpawnOption(String),
    CanManageTypes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub id: FieldId,
    pub label: String,
    pub value: String,
    pub kind: Kind,
    pub options: Vec<String>,
    pub help: String,
    /// The name field is locked once a type exists — renaming would need a
    /// delete+recreate, not an in-place edit.
    pub locked: bool,
    /// A later row of a multi-row field (a rule list): drawn without its
    /// label, which the help pane still shows.
    pub continued: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Step { id: FieldId, back: bool },
    Set { id: FieldId, text: String },
    AddRule { list: RuleList, text: String },
    /// `rule` is the text the row showed: if config.toml changed since,
    /// the change follows that rule rather than whatever now sits at
    /// `index`, or refuses if it's gone.
    RemoveRule { list: RuleList, index: usize, rule: String },
    EditRule { list: RuleList, index: usize, rule: String, text: String },
}

/// The trailing "+ New type" row's index is always `names.len()`.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerTypesView {
    pub names: Vec<String>,
    /// Names with a `[[worker_types]]` config entry (customized builtins
    /// and pure custom types) — drives the list's builtin/custom badge.
    /// Always recomputed from the same `cfg` as `names`/`effective` inside
    /// `reload()`; never update this independently of those two, or the
    /// badge can silently disagree with what's actually in config.toml.
    pub custom: HashSet<String>,
    /// Names with a compiled-in default — the ones `R` can reset.
    pub builtin: HashSet<String>,
    /// Cached resolved spec per name, from the last `reload()` — lets
    /// `rebuild_fields` run in memory when the list selection changes
    /// (keys or a click), with no disk read.
    effective: std::collections::HashMap<String, WorkerTypeSpec>,
    /// Cached enabled harness names, from the last `reload()`.
    harness_names: Vec<String>,
    /// What each type's model resolves to when it sets none, from the last
    /// `reload()`; key `""` is a new, unsaved type.
    inherited_models: std::collections::HashMap<String, String>,
    /// Shown in place of an empty Default model field.
    pub model_hint: String,
    pub list_selected: usize,
    /// Tab-toggled focus: the name list, or the selected type's form.
    /// Starts `true` — picking a type comes before editing it.
    pub on_list: bool,
    pub fields: Vec<Field>,
    pub field_selected: usize,
    pub editing: Option<String>,
    /// `a` is typing a new rule for this list (into `editing`).
    pub adding_rule: Option<RuleList>,
    pub error: Option<String>,
    pub load_error: Option<String>,
    /// Result of the last export/import, shown in the footer.
    pub notice: Option<String>,
}

impl Default for WorkerTypesView {
    fn default() -> Self {
        Self {
            names: Vec::new(),
            custom: HashSet::new(),
            builtin: HashSet::new(),
            effective: std::collections::HashMap::new(),
            harness_names: Vec::new(),
            inherited_models: std::collections::HashMap::new(),
            model_hint: String::new(),
            list_selected: 0,
            on_list: true,
            fields: Vec::new(),
            field_selected: 0,
            editing: None,
            adding_rule: None,
            error: None,
            load_error: None,
            notice: None,
        }
    }
}

impl WorkerTypesView {
    pub fn selected_name(&self) -> Option<&str> {
        self.names.get(self.list_selected).map(String::as_str)
    }

    pub fn is_new_row(&self) -> bool {
        self.list_selected == self.names.len()
    }

    pub fn selected_field(&self) -> Option<&Field> {
        self.fields.get(self.field_selected)
    }

    /// A customized built-in: one `R` can reset to its compiled-in default.
    pub fn resettable(&self, name: &str) -> bool {
        self.builtin.contains(name) && self.custom.contains(name)
    }

    pub fn reload(&mut self, loaded: Result<AppConfig, String>) {
        let selected_name = self.selected_name().map(str::to_string);
        match loaded {
            Ok(cfg) => {
                let reg = cfg.worker_type_registry();
                // Hidden/system types (e.g. a `prompt` type used
                // internally) never belong in a user-facing list.
                self.names = reg.user_facing_names();
                self.names.extend(reg.names().into_iter().filter(|n| is_system_type(n)));
                // Canonicalized: a legacy override written under an alias
                // (e.g. `name = "Implementation"`) must still badge the
                // canonical "Implementor" row shown in `names`, not a
                // name that never appears in the list.
                self.custom = cfg.worker_types.iter()
                    .map(|s| ninox_core::worker_type::canonical_name(&s.name))
                    .collect();
                self.builtin = self.names.iter().filter(|n| worker_type_cli::is_builtin(n)).cloned().collect();
                self.effective = self.names.iter().map(|n| (n.clone(), reg.get(n).cloned().unwrap_or_default())).collect();
                self.harness_names = cfg.registry().enabled_names();
                let inherited = |spec: &WorkerTypeSpec| {
                    let bare = WorkerTypeSpec { model: None, ..spec.clone() };
                    ninox_core::model_resolution::type_model_summary(&bare, &cfg).trim_start_matches("model: ").to_string()
                };
                self.inherited_models = self.effective.iter().map(|(n, spec)| (n.clone(), inherited(spec))).collect();
                self.inherited_models.insert(String::new(), inherited(&WorkerTypeSpec::default()));
                self.load_error = None;
                self.list_selected = selected_name
                    .and_then(|n| self.names.iter().position(|x| *x == n))
                    .unwrap_or(self.list_selected)
                    .min(self.names.len());
                self.rebuild_fields();
            }
            Err(e) => {
                self.names = Vec::new();
                self.custom = HashSet::new();
                self.builtin = HashSet::new();
                self.effective = std::collections::HashMap::new();
                self.harness_names = Vec::new();
                self.fields = Vec::new();
                self.load_error = Some(e);
            }
        }
    }

    /// Recompute `fields` for the currently selected name (or a blank draft
    /// on the trailing "+ New type" row) from the cached `effective`/
    /// `harness_names` snapshot — no disk I/O, safe to call after every
    /// list-selection change.
    pub fn rebuild_fields(&mut self) {
        let spec = match self.selected_name() {
            Some(name) => self.effective.get(name).cloned().unwrap_or_default(),
            None => WorkerTypeSpec::default(),
        };
        let locked_name = self.selected_name().is_some();
        self.model_hint = self.inherited_models.get(self.selected_name().unwrap_or("")).cloned().unwrap_or_default();
        let spawnable: Vec<String> = self.names.iter().filter(|n| !is_system_type(n)).cloned().collect();
        self.fields = fields(&spec, &self.harness_names, &spawnable, locked_name);
        self.field_selected = self.field_selected.min(self.fields.len().saturating_sub(1));
    }

    /// Move the list cursor to `index` (clamped to the trailing "+ New
    /// type" row) and rebuild the form for whatever is now selected,
    /// resetting the field cursor — switching types starts back at the top
    /// of its form.
    pub fn select_list_row(&mut self, index: usize) {
        self.list_selected = index.min(self.names.len());
        self.field_selected = 0;
        self.error = None;
        self.rebuild_fields();
    }
}

fn on_off(b: bool) -> String {
    (if b { "on" } else { "off" }).to_string()
}

const ON_OFF: [&str; 2] = ["on", "off"];

fn can_spawn_summary(can_spawn: &[String]) -> String {
    match can_spawn {
        [] => "none".into(),
        [all] if all == "*" => "* (every type)".into(),
        names => names.join(", "),
    }
}

/// The form for `spec`. `type_names` are the user-facing types `can_spawn`
/// can name.
pub fn fields(spec: &WorkerTypeSpec, harness_names: &[String], type_names: &[String], locked_name: bool) -> Vec<Field> {
    let f = |id: FieldId, label: &str, value: String, kind: Kind, options: Vec<String>, help: &str, locked: bool| Field {
        id,
        label: label.to_string(),
        value,
        kind,
        options,
        help: help.to_string(),
        locked,
        continued: false,
    };
    let on_off_opts = || ON_OFF.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let system = is_system_type(&spec.name);
    let locked_deny = spec.locked_deny_rules();
    let locked_allow = spec.locked_allow_rules();
    let (name_help, brief_label, brief_value, brief_help) = if system {
        (
            format!("System · single instance. {SYSTEM_APPLIES_NOTE}"),
            "Extra instructions",
            spec.brief_addendum.clone().unwrap_or_default(),
            "Appended to the built-in Ninox brief, which is never replaced.",
        )
    } else {
        (
            "This type's name, as referenced by `ninox spawn --type <name>`. Can't be changed once created \
             — delete and recreate to rename.".to_string(),
            "Brief template",
            spec.brief_template.clone().unwrap_or_default(),
            "Composed with `--prompt` via a required {prompt} placeholder. Empty passes the prompt through \
             unchanged.",
        )
    };
    let mut out = vec![
        f(
            FieldId::Name,
            "Name",
            display_name(&spec.name).to_string(),
            Kind::Text,
            Vec::new(),
            &name_help,
            locked_name,
        ),
        f(
            FieldId::UseWhen,
            "Use when",
            spec.use_when.clone().unwrap_or_default(),
            Kind::Text,
            Vec::new(),
            "One line telling orchestrators, the planner and the Ninox prompt when to pick this type \
             (`ninox worker-type list` shows it).",
            system,
        ),
        f(FieldId::Brief, brief_label, brief_value, Kind::Text, Vec::new(), brief_help, false),
        f(
            FieldId::Harness,
            "Default harness",
            spec.harness.clone(),
            Kind::Choice,
            harness_names.to_vec(),
            if system {
                "Harness fixed: its safety rules are enforced by Claude Code."
            } else {
                "The harness `ninox spawn --type` runs for this type, in place of the Worker default harness."
            },
            system,
        ),
        f(
            FieldId::Model,
            "Default model",
            spec.model.clone().unwrap_or_default(),
            Kind::Text,
            Vec::new(),
            "Model id passed to the harness; empty inherits the Worker default model (or the harness default if that is set for another harness). A model picked at spawn time overrides it.",
            false,
        ),
        f(
            FieldId::PermEnabled,
            "Permissions enforced",
            on_off(spec.permissions.enabled),
            Kind::Toggle,
            on_off_opts(),
            "Off: this type spawns with no extra permission scoping. On: allow/deny/ask below are seeded \
             into the worktree's settings.",
            system,
        ),
    ];
    for list in [RuleList::Allow, RuleList::Deny, RuleList::Ask] {
        let help = "One Claude Code permission rule per row, e.g. Read or Bash(gh pr merge:*). \
                    a adds a rule · ↵ edits this one · x deletes it.";
        let rules = list.rules(spec);
        if rules.is_empty() {
            out.push(f(FieldId::Rule(list, None), list.label(), String::new(), Kind::Text, Vec::new(), help, false));
        }
        for (i, rule) in rules.iter().enumerate() {
            let built_in = (list == RuleList::Deny && locked_deny.contains(rule))
                || (list == RuleList::Allow && locked_allow.contains(rule));
            let mut row = f(FieldId::Rule(list, Some(i)), list.label(), rule.clone(), Kind::Text, Vec::new(), help, built_in);
            row.continued = i > 0;
            out.push(row);
        }
    }
    if system {
        out.push(f(
            FieldId::CanSpawn,
            "Can spawn",
            "none (system type, never spawnable)".into(),
            Kind::Action,
            Vec::new(),
            "The Ninox prompt has exactly one instance and is never spawned by `ninox spawn` or any type.",
            true,
        ));
        out.push(f(
            FieldId::CanManageTypes,
            "Can manage types",
            on_off(spec.can_manage_types),
            Kind::Toggle,
            on_off_opts(),
            "Fixed for the Ninox type. Its brief tells it to read back and confirm every worker-type change with you.",
            true,
        ));
        return out;
    }
    out.push(f(
        FieldId::CanSpawn,
        "Can spawn",
        can_spawn_summary(&spec.can_spawn),
        Kind::Action,
        Vec::new(),
        "Which types this type's sessions may `ninox spawn --type`. * is every user-facing type; it and \
         named types are exclusive. Toggle the rows below.",
        false,
    ));
    let wildcard = spec.can_spawn.iter().any(|n| n == "*");
    // An entry that isn't a listed type (a deleted custom type, a
    // hand-written alias) still gets a row, so it can be turned off —
    // the backend refuses every can_spawn save while an unknown one stays.
    let stray = spec.can_spawn.iter().filter(|n| *n != "*" && !type_names.contains(n)).cloned();
    for opt in std::iter::once("*".to_string()).chain(type_names.iter().cloned()).chain(stray) {
        let on = if opt == "*" { wildcard } else { !wildcard && spec.can_spawn.contains(&opt) };
        let note = if opt != "*" && !type_names.contains(&opt) { " (not a listed type)" } else { "" };
        out.push(f(
            FieldId::CanSpawnOption(opt.clone()),
            &format!("  ↳ {opt}{note}"),
            on_off(on),
            Kind::Toggle,
            on_off_opts(),
            "Space or ↵ toggles whether this type may spawn it.",
            false,
        ));
    }
    out.push(f(
        FieldId::CanManageTypes,
        "Can manage types",
        on_off(spec.can_manage_types),
        Kind::Toggle,
        on_off_opts(),
        "Lets this type's sessions create, edit, delete, reset and import worker types (never their own). \
         Only you, not an agent, can change this.",
        false,
    ));
    out
}

/// Make one change to `spec` (a clone of the selected type's resolved spec,
/// or a blank draft for "+ New type"). `Ok` carries a short confirmation.
pub fn apply(spec: &mut WorkerTypeSpec, harness_names: &[String], type_names: &[String], change: &Change) -> Result<String, String> {
    let (id, text) = match change {
        Change::AddRule { list, text } => {
            let rule = new_rule(spec, *list, text, None)?;
            list.rules_mut(spec).push(rule.clone());
            return Ok(format!("{} + {rule}", list.label()));
        }
        Change::RemoveRule { list, index, rule } => {
            ensure_rule_not_locked(spec, *list, rule)?;
            let at = locate(list.rules(spec), *index, rule)?;
            list.rules_mut(spec).remove(at);
            return Ok(format!("{} − {rule}", list.label()));
        }
        Change::EditRule { list, index, rule, text } => {
            ensure_rule_not_locked(spec, *list, rule)?;
            let at = locate(list.rules(spec), *index, rule)?;
            if text.trim().is_empty() {
                list.rules_mut(spec).remove(at);
                return Ok(format!("{} − {rule}", list.label()));
            }
            let new = new_rule(spec, *list, text, Some(at))?;
            list.rules_mut(spec)[at] = new.clone();
            return Ok(format!("{}: {rule} → {new}", list.label()));
        }
        Change::Set { id, text } => (id, Some(text.trim().to_string())),
        Change::Step { id, .. } => (id, None),
    };
    let locked_name = !spec.name.is_empty();
    let field = fields(spec, harness_names, type_names, locked_name).into_iter().find(|f| &f.id == id).ok_or("no such field")?;
    if field.locked {
        return Err(if is_system_type(&spec.name) {
            format!("{} is fixed for the Ninox type", field.label.trim())
        } else {
            format!("{} can't be changed once created", field.label)
        });
    }
    let text = match (text, change) {
        (Some(text), _) => text,
        (None, Change::Step { back, .. }) => {
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
        (None, _) => return Err("nothing to change".into()),
    };
    set(spec, &field, &text)?;
    let shown = if text.is_empty() { "default".to_string() } else { text };
    Ok(format!("{} → {shown}", field.label.trim()))
}

fn ensure_rule_not_locked(spec: &WorkerTypeSpec, list: RuleList, rule: &str) -> Result<(), String> {
    let locked = match list {
        RuleList::Deny => spec.locked_deny_rules(),
        RuleList::Allow => spec.locked_allow_rules(),
        RuleList::Ask => Vec::new(),
    };
    if locked.iter().any(|r| r == rule) {
        return Err(format!("{rule} is a built-in Ninox rule and can't be removed"));
    }
    Ok(())
}

/// `text` as a rule for `list`, refusing an empty one or one the list
/// already has (other than at `replacing`).
fn new_rule(spec: &WorkerTypeSpec, list: RuleList, text: &str, replacing: Option<usize>) -> Result<String, String> {
    let rule = text.trim().to_string();
    if rule.is_empty() {
        return Err("a rule can't be empty".into());
    }
    if list.rules(spec).iter().enumerate().any(|(i, r)| *r == rule && Some(i) != replacing) {
        return Err(format!("{rule} is already in {}", list.label()));
    }
    Ok(rule)
}

/// Where `rule` (shown at `index`) is now.
fn locate(rules: &[String], index: usize, rule: &str) -> Result<usize, String> {
    if rules.get(index).is_some_and(|r| r == rule) {
        return Ok(index);
    }
    rules.iter().position(|r| r == rule).ok_or_else(|| format!("{rule} isn't there any more (config.toml changed); reloaded"))
}

fn set(spec: &mut WorkerTypeSpec, field: &Field, s: &str) -> Result<(), String> {
    let bad = || format!("{s:?} is not one of: {}", field.options.join(", "));
    match &field.id {
        FieldId::Name => {
            if s.trim().is_empty() {
                return Err("a worker type needs a name".into());
            }
            spec.name = s.to_string();
        }
        FieldId::UseWhen => {
            if s.contains('\n') {
                return Err("a use-when description is one line".into());
            }
            spec.use_when = (!s.trim().is_empty()).then(|| s.trim().to_string());
        }
        FieldId::Brief if is_system_type(&spec.name) => spec.brief_addendum = (!s.is_empty()).then(|| s.to_string()),
        FieldId::Brief => spec.brief_template = (!s.is_empty()).then(|| s.to_string()),
        FieldId::Harness => {
            if !field.options.iter().any(|o| o == s) {
                return Err(bad());
            }
            // Mirrors AgentConfig::set_harness — a model id from one
            // harness must not leak into another's launch command.
            spec.harness = s.to_string();
            spec.model = None;
        }
        FieldId::Model => {
            if s.chars().any(char::is_whitespace) {
                return Err("a model id has no spaces".into());
            }
            spec.model = (!s.is_empty()).then(|| s.to_string());
        }
        FieldId::PermEnabled => spec.permissions.enabled = s == "on",
        // Text on an empty list's placeholder adds its first rule; an
        // existing rule row changes through `Change::EditRule`.
        FieldId::Rule(list, None) => {
            if !s.is_empty() {
                let rule = new_rule(spec, *list, s, None)?;
                list.rules_mut(spec).push(rule);
            }
        }
        FieldId::Rule(_, Some(_)) => return Err("edit a rule through its row".into()),
        FieldId::CanSpawn => return Err("toggle the options below it".into()),
        FieldId::CanSpawnOption(name) => {
            let on = field.value == "on";
            if (s == "on") != on {
                worker_type_cli::toggle_can_spawn(&mut spec.can_spawn, name);
            }
        }
        FieldId::CanManageTypes => spec.can_manage_types = s == "on",
    }
    Ok(())
}

/// Only what changed between `old` and `new`, so an untouched field (above
/// all `can_manage_types`) is never sent — `worker_type_cli` refuses an
/// agent caller that so much as names it.
fn changed_fields(old: &WorkerTypeSpec, new: &WorkerTypeSpec) -> Fields {
    let rules = |o: &Vec<String>, n: &Vec<String>| -> (Vec<String>, bool) {
        if o == n { (Vec::new(), false) } else { (n.clone(), n.is_empty()) }
    };
    let (allow, clear_allow) = rules(&old.permissions.allow, &new.permissions.allow);
    let (deny, clear_deny) = rules(&old.permissions.deny, &new.permissions.deny);
    let (ask, clear_ask) = rules(&old.permissions.ask, &new.permissions.ask);
    Fields {
        brief: if is_system_type(&new.name) {
            (old.brief_addendum != new.brief_addendum).then(|| new.brief_addendum.clone().unwrap_or_default())
        } else {
            (old.brief_template != new.brief_template).then(|| new.brief_template.clone().unwrap_or_default())
        },
        use_when: (old.use_when != new.use_when).then(|| new.use_when.clone().unwrap_or_default()),
        harness: (old.harness != new.harness).then(|| new.harness.clone()),
        model: (old.model != new.model).then(|| new.model.clone().unwrap_or_default()),
        allow,
        deny,
        ask,
        clear_allow,
        clear_deny,
        clear_ask,
        can_spawn: (old.can_spawn != new.can_spawn).then(|| new.can_spawn.join(",")),
        can_manage_types: (old.can_manage_types != new.can_manage_types).then_some(new.can_manage_types),
        permissions_enabled: (old.permissions.enabled != new.permissions.enabled).then_some(new.permissions.enabled),
    }
}

/// Load `config.toml`, or say why it can't be read.
pub fn load() -> Result<AppConfig, String> {
    AppConfig::load().map_err(|e| format!("{} doesn't parse: {e}", AppConfig::config_path().display()))
}

/// Read → apply one change to `name`'s resolved spec → write it through
/// `worker_type_cli::edit` (its validation, and its rules for an agent
/// `caller`) → return the saved config and a short confirmation.
pub fn save(store: &Store, caller: Option<&str>, name: &str, change: &Change) -> Result<(AppConfig, String), String> {
    let cfg = load()?;
    let reg = cfg.worker_type_registry();
    let current = reg.get(name).cloned().ok_or_else(|| format!("no worker type named {name:?}"))?;
    let mut spec = current.clone();
    let msg = apply(&mut spec, &cfg.registry().enabled_names(), &reg.user_facing_names(), change)?;
    // Nothing changed: writing anyway would pin an untouched built-in as a
    // customization.
    if spec == current {
        return Ok((cfg, "no change".into()));
    }
    worker_type_cli::edit(store, caller, name, changed_fields(&current, &spec)).map_err(|e| e.to_string())?;
    Ok((load()?, msg))
}

/// Create a blank worker type named `name` (`worker_type_cli::create`
/// rejects an empty, invalid or colliding name).
pub fn new_type(store: &Store, caller: Option<&str>, name: &str) -> Result<AppConfig, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a worker type needs a name".into());
    }
    worker_type_cli::create(store, caller, name, "", Fields::default()).map_err(|e| e.to_string())?;
    load()
}

/// Delete a custom type. A built-in refuses — `reset_type` is its honest
/// counterpart.
pub fn delete_type(store: &Store, caller: Option<&str>, name: &str) -> Result<AppConfig, String> {
    worker_type_cli::delete(store, caller, name).map_err(|e| e.to_string())?;
    load()
}

/// Drop a built-in's customization, back to its compiled-in default.
pub fn reset_type(store: &Store, caller: Option<&str>, name: &str) -> Result<AppConfig, String> {
    worker_type_cli::reset(store, caller, name).map_err(|e| e.to_string())?;
    load()
}

/// Serialize `name`'s resolved spec to a standalone TOML snippet under
/// `AppConfig::worker_type_exports_dir()`, returning the written path.
pub fn export_type(name: &str) -> Result<PathBuf, String> {
    let cfg = load()?;
    let spec = cfg.worker_type_registry().get(name).cloned().ok_or_else(|| format!("no worker type named {name:?}"))?;
    let snippet = spec.to_toml_snippet().map_err(|e| e.to_string())?;
    let dir = AppConfig::worker_type_exports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let path = export_path(&dir, name)?;
    std::fs::write(&path, snippet).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path)
}

/// `<dir>/<slug(name)>.toml`, numbered past any existing export that
/// belongs to a *different* type — two names that slug to the same string
/// (e.g. "Security Reviewer" and "security-reviewer") must not silently
/// overwrite each other. Re-exporting the same name reuses its own file,
/// same as updating it.
fn export_path(dir: &std::path::Path, name: &str) -> Result<PathBuf, String> {
    let base = slug(name);
    let mut path = dir.join(format!("{base}.toml"));
    let mut n = 2;
    while path.exists() {
        let existing = std::fs::read_to_string(&path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
        match WorkerTypeSpec::from_toml_snippet(&existing) {
            Ok(spec) if spec.name == name => break,
            _ => {}
        }
        path = dir.join(format!("{base}-{n}.toml"));
        n += 1;
    }
    Ok(path)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "worker-type".into() } else { out }
}

/// Template an Import editor buffer opens with.
pub const IMPORT_TEMPLATE: &str = "# Paste a [[worker_types]] TOML snippet below (from Export) and save.\n\n[[worker_types]]\nname = \"\"\n";

/// Import the `[[worker_types]]` entries in an edited Import buffer, the
/// same as `ninox worker-type import` (an existing name is updated).
pub fn import_types(store: &Store, caller: Option<&str>, edited: &str) -> Result<(AppConfig, Vec<String>), String> {
    let names = worker_type_cli::import(store, caller, edited).map_err(|e| e.to_string())?;
    Ok((load()?, names))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_hint_shows_what_a_type_without_a_model_inherits() {
        let mut cfg = AppConfig::default();
        cfg.worker.model = Some("worker-m".into());
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Researcher".into(), model: Some("r".into()), ..WorkerTypeSpec::default() });
        let mut v = WorkerTypesView::default();
        v.reload(Ok(cfg));
        let at = |v: &mut WorkerTypesView, name: &str| {
            let i = v.names.iter().position(|n| n == name).unwrap();
            v.select_list_row(i);
            v.model_hint.clone()
        };
        assert_eq!(at(&mut v, "Implementor"), "inherits Worker default (worker-m)");
        assert_eq!(at(&mut v, "Researcher"), "inherits Worker default (worker-m)", "shown only when the field is cleared");
        let n = v.names.len();
        v.select_list_row(n);
        assert_eq!(v.model_hint, "inherits Worker default (worker-m)");
    }

    fn field<'a>(fs: &'a [Field], id: &FieldId) -> &'a Field {
        fs.iter().find(|f| &f.id == id).unwrap()
    }

    const TYPES: [&str; 2] = ["Implementor", "Researcher"];

    fn types() -> Vec<String> {
        TYPES.iter().map(|s| s.to_string()).collect()
    }

    fn change(spec: &mut WorkerTypeSpec, names: &[String], c: Change) -> Result<String, String> {
        apply(spec, names, &types(), &c)
    }

    fn step(spec: &mut WorkerTypeSpec, names: &[String], id: FieldId, back: bool) -> Result<String, String> {
        change(spec, names, Change::Step { id, back })
    }

    fn set_text(spec: &mut WorkerTypeSpec, names: &[String], id: FieldId, text: &str) -> Result<String, String> {
        change(spec, names, Change::Set { id, text: text.into() })
    }

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    #[test]
    fn fields_cover_the_spec_shape_with_a_row_per_rule_and_per_can_spawn_option() {
        let spec = WorkerTypeSpec { can_spawn: vec!["Researcher".into()], ..WorkerTypeSpec::default() };
        let mut spec = spec;
        spec.permissions.allow = vec!["Read".into(), "Grep".into()];
        let names = vec!["claude-code".to_string(), "codex".to_string()];
        let fs = fields(&spec, &names, &types(), false);
        for id in [FieldId::Name, FieldId::Brief, FieldId::Harness, FieldId::Model, FieldId::PermEnabled, FieldId::CanSpawn, FieldId::CanManageTypes] {
            assert!(fs.iter().any(|f| f.id == id), "{id:?} missing");
        }
        let allow: Vec<_> = fs.iter().filter(|f| matches!(f.id, FieldId::Rule(RuleList::Allow, _))).collect();
        assert_eq!(allow.iter().map(|f| (f.value.as_str(), f.continued)).collect::<Vec<_>>(), vec![("Read", false), ("Grep", true)]);
        assert!(fs.iter().any(|f| f.id == FieldId::Rule(RuleList::Deny, None)), "an empty list keeps one placeholder row");
        assert_eq!(field(&fs, &FieldId::CanSpawn).value, "Researcher");
        assert_eq!(field(&fs, &FieldId::CanSpawnOption("Researcher".into())).value, "on");
        assert_eq!(field(&fs, &FieldId::CanSpawnOption("*".into())).value, "off");
        assert_eq!(field(&fs, &FieldId::Harness).options, names);
        assert!(fields(&spec, &names, &types(), true).into_iter().find(|f| f.id == FieldId::Name).unwrap().locked);
    }

    #[test]
    fn name_is_locked_once_the_type_exists_and_cannot_be_blanked() {
        let mut spec = WorkerTypeSpec { name: "Custom".into(), ..WorkerTypeSpec::default() };
        let names = vec!["claude-code".to_string()];
        assert!(set_text(&mut spec, &names, FieldId::Name, "Renamed").unwrap_err().contains("can't be changed"));
        assert_eq!(spec.name, "Custom");
        let mut blank = WorkerTypeSpec::default();
        assert!(set_text(&mut blank, &names, FieldId::Name, "  ").is_err());
    }

    #[test]
    fn harness_choices_cycle_validate_and_clear_the_model() {
        let mut spec = WorkerTypeSpec { model: Some("claude-opus-5-5".into()), ..WorkerTypeSpec::default() };
        let names = vec!["claude-code".to_string(), "codex".to_string()];
        step(&mut spec, &names, FieldId::Harness, false).unwrap();
        assert_eq!((spec.harness.as_str(), spec.model.as_deref()), ("codex", None), "a model id must not survive a harness switch");
        assert!(set_text(&mut spec, &names, FieldId::Harness, "nope").is_err());
    }

    #[test]
    fn brief_and_model_are_free_text_and_empty_means_default() {
        let mut spec = WorkerTypeSpec::default();
        let names = vec!["claude-code".to_string()];
        set_text(&mut spec, &names, FieldId::Brief, "Task: {prompt}").unwrap();
        assert_eq!(spec.brief_template.as_deref(), Some("Task: {prompt}"));
        set_text(&mut spec, &names, FieldId::Brief, "").unwrap();
        assert_eq!(spec.brief_template, None);
        set_text(&mut spec, &names, FieldId::Model, " o3 ").unwrap();
        assert_eq!(spec.model.as_deref(), Some("o3"));
        assert!(set_text(&mut spec, &names, FieldId::Model, "two words").is_err());
    }

    #[test]
    fn rules_are_added_edited_and_removed_one_row_at_a_time() {
        let mut spec = WorkerTypeSpec::default();
        let names = vec!["claude-code".to_string()];
        step(&mut spec, &names, FieldId::PermEnabled, false).unwrap();
        assert!(spec.permissions.enabled);
        set_text(&mut spec, &names, FieldId::Rule(RuleList::Allow, None), "Read").unwrap();
        change(&mut spec, &names, Change::AddRule { list: RuleList::Allow, text: " Bash(ls:*) ".into() }).unwrap();
        assert_eq!(spec.permissions.allow, vec!["Read", "Bash(ls:*)"], "one rule per row, no comma splitting");
        assert!(change(&mut spec, &names, Change::AddRule { list: RuleList::Allow, text: "Read".into() }).is_err(), "no duplicates");
        assert!(change(&mut spec, &names, Change::AddRule { list: RuleList::Allow, text: "  ".into() }).is_err());
        let edit = |index: usize, rule: &str, text: &str| Change::EditRule { list: RuleList::Allow, index, rule: rule.into(), text: text.into() };
        change(&mut spec, &names, edit(0, "Read", "Grep")).unwrap();
        assert_eq!(spec.permissions.allow, vec!["Grep", "Bash(ls:*)"]);
        let err = change(&mut spec, &names, edit(1, "Bash(ls:*)", "Grep")).unwrap_err();
        assert_eq!(err, "Grep is already in Allow rules", "an edit can't duplicate a rule either");
        change(&mut spec, &names, Change::RemoveRule { list: RuleList::Allow, index: 1, rule: "Bash(ls:*)".into() }).unwrap();
        change(&mut spec, &names, edit(0, "Grep", "")).unwrap();
        assert!(spec.permissions.allow.is_empty(), "emptying a rule's text removes it");
        assert!(change(&mut spec, &names, Change::RemoveRule { list: RuleList::Deny, index: 0, rule: "x".into() }).is_err());
    }

    #[test]
    fn a_rule_change_follows_the_rule_it_was_made_on_if_config_moved_underneath() {
        let mut spec = WorkerTypeSpec::default();
        spec.permissions.deny = vec!["X".into(), "A".into(), "B".into()];
        let names = vec!["claude-code".to_string()];
        // The row showed "B" at index 1, before "X" was added elsewhere.
        change(&mut spec, &names, Change::RemoveRule { list: RuleList::Deny, index: 1, rule: "B".into() }).unwrap();
        assert_eq!(spec.permissions.deny, vec!["X", "A"]);
        let gone = change(&mut spec, &names, Change::EditRule { list: RuleList::Deny, index: 0, rule: "B".into(), text: "C".into() });
        assert!(gone.unwrap_err().contains("isn't there any more"));
        assert_eq!(spec.permissions.deny, vec!["X", "A"], "nothing else changed");
    }

    #[test]
    fn a_can_spawn_entry_that_isnt_a_listed_type_still_gets_a_row_to_turn_it_off() {
        let spec = WorkerTypeSpec { can_spawn: vec!["Gone".into(), "Researcher".into()], ..WorkerTypeSpec::default() };
        let fs = fields(&spec, &[], &types(), false);
        let gone = field(&fs, &FieldId::CanSpawnOption("Gone".into()));
        assert_eq!((gone.value.as_str(), gone.label.contains("not a listed type")), ("on", true));
        let mut spec = spec;
        step(&mut spec, &[], FieldId::CanSpawnOption("Gone".into()), false).unwrap();
        assert_eq!(spec.can_spawn, vec!["Researcher"]);
    }

    #[test]
    fn can_spawn_options_toggle_with_the_wildcard_exclusive_and_can_manage_types_toggles() {
        let mut spec = WorkerTypeSpec::default();
        let names = vec!["claude-code".to_string()];
        step(&mut spec, &names, FieldId::CanSpawnOption("Researcher".into()), false).unwrap();
        step(&mut spec, &names, FieldId::CanSpawnOption("Implementor".into()), false).unwrap();
        assert_eq!(spec.can_spawn, vec!["Researcher", "Implementor"]);
        step(&mut spec, &names, FieldId::CanSpawnOption("*".into()), false).unwrap();
        assert_eq!(spec.can_spawn, vec!["*"]);
        step(&mut spec, &names, FieldId::CanSpawnOption("Researcher".into()), false).unwrap();
        assert_eq!(spec.can_spawn, vec!["Researcher"], "a name clears the wildcard");
        assert!(step(&mut spec, &names, FieldId::CanSpawn, false).is_err(), "the summary row isn't itself toggled");
        step(&mut spec, &names, FieldId::CanManageTypes, false).unwrap();
        assert!(spec.can_manage_types);
    }

    #[test]
    fn only_changed_fields_are_sent_so_an_untouched_can_manage_types_is_never_named() {
        let old = WorkerTypeSpec { name: "T".into(), can_manage_types: true, ..WorkerTypeSpec::default() };
        let mut new = old.clone();
        new.model = Some("o3".into());
        new.permissions.deny = vec![];
        let f = changed_fields(&old, &new);
        assert_eq!(f.model.as_deref(), Some("o3"));
        assert!(f.can_manage_types.is_none() && f.can_spawn.is_none() && f.permissions_enabled.is_none() && f.brief.is_none() && f.harness.is_none());
        assert!(!f.clear_deny && f.deny.is_empty(), "an unchanged (empty) list isn't touched");
        let mut cleared = old.clone();
        cleared.permissions.allow = vec!["Read".into()];
        let f = changed_fields(&cleared, &old);
        assert!(f.clear_allow && f.allow.is_empty(), "removing the last rule clears the list");
    }

    #[test]
    fn reload_lists_user_facing_types_and_marks_custom_and_resettable() {
        let mut v = WorkerTypesView::default();
        let mut cfg = AppConfig::default();
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Tester".into(), ..WorkerTypeSpec::default() });
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Implementation".into(), harness: "codex".into(), ..WorkerTypeSpec::default() });
        v.reload(Ok(cfg));
        // The system `prompt` type (shown as Ninox) is listed last, after the
        // user-facing types, and is never a can_spawn option.
        assert_eq!(v.names, vec!["Evaluator", "Implementor", "Orchestrator", "Researcher", "Tester", "prompt"]);
        assert_eq!(v.custom, HashSet::from(["Tester".to_string(), "Implementor".to_string()]));
        assert!(v.resettable("Implementor") && !v.resettable("Tester") && !v.resettable("Researcher"));
        assert!(v.fields.iter().any(|f| f.id == FieldId::CanSpawnOption("Tester".into())), "custom types are can_spawn options");
        assert!(!v.fields.iter().any(|f| f.id == FieldId::CanSpawnOption("prompt".into())));
    }

    #[test]
    fn reload_keeps_the_selection_by_name() {
        let mut v = WorkerTypesView::default();
        v.reload(Ok(AppConfig::default()));
        v.list_selected = v.names.iter().position(|n| n == "Researcher").unwrap();
        let mut cfg = AppConfig::default();
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Aaa".into(), ..WorkerTypeSpec::default() });
        v.reload(Ok(cfg));
        assert_eq!(v.selected_name(), Some("Researcher"));
        v.reload(Err("bad".into()));
        assert_eq!(v.load_error.as_deref(), Some("bad"));
        assert!(v.names.is_empty());
    }

    #[test]
    fn select_list_row_rebuilds_fields_in_memory_with_no_reload() {
        let mut v = WorkerTypesView::default();
        let mut cfg = AppConfig::default();
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Implementation".into(), harness: "codex".into(), ..WorkerTypeSpec::default() });
        v.reload(Ok(cfg));
        let evaluator_i = v.names.iter().position(|n| n == "Evaluator").unwrap();
        let implementation_i = v.names.iter().position(|n| n == "Implementor").unwrap();
        v.select_list_row(evaluator_i);
        assert_eq!(field(&v.fields, &FieldId::Harness).value, "claude-code");
        v.field_selected = 2;
        v.select_list_row(implementation_i);
        assert_eq!(field(&v.fields, &FieldId::Harness).value, "codex");
        assert_eq!(v.field_selected, 0);
    }

    #[test]
    fn is_new_row_is_the_trailing_index() {
        let mut v = WorkerTypesView::default();
        v.reload(Ok(AppConfig::default()));
        assert!(!v.is_new_row());
        v.list_selected = v.names.len();
        assert!(v.is_new_row());
        assert_eq!(v.selected_name(), None);
    }

    #[test]
    fn slug_handles_spaces_and_punctuation() {
        assert_eq!(slug("Security Reviewer"), "security-reviewer");
        assert_eq!(slug("  "), "worker-type");
    }

    #[test]
    fn save_writes_through_the_shared_backend_and_keeps_other_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let _iso = ninox_core::test_isolation::isolate_with_config(&path);
        let store = store();
        let (cfg, msg) = save(&store, None, "Researcher", &Change::Set { id: FieldId::Model, text: "claude-opus-5-5".into() }).unwrap();
        assert!(msg.contains("claude-opus-5-5"), "{msg}");
        assert_eq!(cfg.worker_type_registry().get("Researcher").unwrap().model.as_deref(), Some("claude-opus-5-5"));

        let mut other = AppConfig::load().unwrap();
        other.zoom = 1.5;
        other.save().unwrap();
        save(&store, None, "Researcher", &Change::Step { id: FieldId::PermEnabled, back: false }).unwrap();
        save(&store, None, "Researcher", &Change::AddRule { list: RuleList::Deny, text: "Bash(rm:*)".into() }).unwrap();
        save(&store, None, "Researcher", &Change::Step { id: FieldId::CanManageTypes, back: false }).unwrap();

        let back = AppConfig::load().unwrap();
        let r = back.worker_type_registry().get("Researcher").cloned().unwrap();
        assert!(r.permissions.enabled && r.can_manage_types);
        assert_eq!(r.permissions.deny, vec!["Bash(rm:*)"]);
        assert_eq!(back.zoom, 1.5);
        assert!(save(&store, None, "NoSuchType", &Change::Set { id: FieldId::Model, text: "x".into() }).is_err());
    }

    #[test]
    fn a_save_that_changes_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let _iso = ninox_core::test_isolation::isolate_with_config(&path);
        let (cfg, msg) = save(&store(), None, "Researcher", &Change::Set { id: FieldId::Rule(RuleList::Allow, None), text: String::new() }).unwrap();
        assert_eq!(msg, "no change");
        assert!(cfg.worker_types.is_empty(), "an untouched built-in must not become a customization");
    }

    #[test]
    fn new_delete_and_reset_go_through_the_shared_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let _iso = ninox_core::test_isolation::isolate_with_config(&path);
        let store = store();
        let cfg = new_type(&store, None, "SecurityReviewer").unwrap();
        assert!(cfg.worker_type_registry().get("SecurityReviewer").is_some());
        assert!(new_type(&store, None, "  ").is_err());
        assert!(new_type(&store, None, "Implementation").is_err(), "an alias of a built-in collides");
        let cfg = delete_type(&store, None, "SecurityReviewer").unwrap();
        assert!(cfg.worker_type_registry().get("SecurityReviewer").is_none());
        assert!(delete_type(&store, None, "Researcher").unwrap_err().contains("reset"), "a built-in is reset, not deleted");
        save(&store, None, "Researcher", &Change::Set { id: FieldId::Model, text: "o3".into() }).unwrap();
        let cfg = reset_type(&store, None, "Researcher").unwrap();
        assert_eq!(cfg.worker_type_registry().get("Researcher").unwrap().model, None, "back to the compiled-in default");
        assert!(reset_type(&store, None, "Researcher").is_err(), "nothing left to reset");
    }

    #[test]
    fn export_then_import_round_trips_through_a_temp_file() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        new_type(&store, None, "Researcher2").unwrap();
        let path = export_type("Researcher2").unwrap();
        let snippet = std::fs::read_to_string(&path).unwrap();
        assert!(snippet.contains("name = \"Researcher2\""));
        let renamed = snippet.replace("Researcher2", "Researcher3");
        let (cfg, imported) = import_types(&store, None, &renamed).unwrap();
        assert_eq!(imported, vec!["Researcher3"]);
        assert!(cfg.worker_type_registry().get("Researcher3").is_some());
        assert!(import_types(&store, None, "not valid toml [").is_err());
    }

    #[test]
    fn export_numbers_past_a_different_type_that_slugs_to_the_same_name() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        new_type(&store, None, "Sec-Rev").unwrap();
        new_type(&store, None, "Sec_Rev").unwrap();
        assert_eq!(slug("Sec-Rev"), slug("Sec_Rev"), "fixture: the two names collide");
        let first = export_type("Sec-Rev").unwrap();
        let second = export_type("Sec_Rev").unwrap();
        assert_ne!(first, second);
        assert_eq!(export_type("Sec-Rev").unwrap(), first, "re-exporting updates its own file");
    }
}
