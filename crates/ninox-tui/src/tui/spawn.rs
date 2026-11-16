//! The Spawn modal (`n`): Kind/Name/Workspace/Worker type/Agent/Model/
//! Brains/Writes to/Prompt as a `Field` list, mirroring `worker_types.rs`'s and
//! `settings.rs`'s shape so Tab/Shift-Tab, h/l stepping and text editing all
//! behave the same way across every TUI modal. See `BRIEF-MLOPS-5350-tui-
//! parity.md` item 1.

use ninox_core::config::AppConfig;
use ninox_ui_shared::models::{model_options, CUSTOM_SENTINEL};
use ninox_ui_shared::spawn_form::SpawnKind;

pub use super::settings::Kind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldId {
    Kind,
    Name,
    Workspace,
    WorkerType,
    Harness,
    Model,
    CustomModel,
    /// Multi-select: Left/Right move the cursor (`value`), Space toggles.
    Brains,
    BrainWrite,
    Prompt,
}

/// One catalogue offered by the Brains multi-select.
#[derive(Clone, Debug, PartialEq)]
pub struct BrainOption {
    pub name: String,
    pub writable: bool,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub id: FieldId,
    pub label: String,
    pub value: String,
    pub kind: Kind,
    pub options: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SpawnForm {
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    /// Captured at `build()` time so `step` can recompute `Model`'s options
    /// and the resolved default without the live `AppConfig` threaded back
    /// in.
    config: AppConfig,
    registry: ninox_core::harness::HarnessRegistry,
    pub brains: Vec<BrainOption>,
    /// Set once the user picks an Agent / a Model. Until then that field
    /// mirrors the resolved default for the chosen kind and type, and a
    /// submit resolves afresh instead of pinning it as an explicit pick.
    harness_picked: bool,
    model_picked: bool,
}

/// The Model option meaning "no `--model`; the harness picks".
pub const HARNESS_DEFAULT_OPTION: &str = "harness default";

fn model_choices(registry: &ninox_core::harness::HarnessRegistry, harness: &str, current: Option<&str>) -> Vec<String> {
    let mut opts = vec![HARNESS_DEFAULT_OPTION.to_string()];
    opts.extend(model_options(&registry.spec(harness), None, current));
    opts
}

impl Default for SpawnForm {
    fn default() -> Self {
        build(&AppConfig::default())
    }
}

/// Reads `config.toml` for a fresh `build()` — same `Result<AppConfig,
/// String>` shape as `settings::load()`/`worker_types::load()`, for
/// `Action::LoadSpawnForm` to apply via `TuiState::spawn_form`.
pub fn load() -> Result<AppConfig, String> {
    AppConfig::load().map_err(|e| format!("{} doesn't parse: {e}", AppConfig::config_path().display()))
}

fn field(id: FieldId, label: &str, value: String, kind: Kind, options: Vec<String>) -> Field {
    Field { id, label: label.to_string(), value, kind, options }
}

/// Builds a fresh form from the current config: Kind defaults to
/// Orchestrator, Harness/Model/Catalogue/WorkerType default to the
/// registries' first/preferred entries.
pub fn build(config: &AppConfig) -> SpawnForm {
    let registry = config.registry();
    let harness_names = registry.enabled_names();

    // Orchestrator is user-facing (not hidden) but can't be picked for a
    // Standalone spawn — spawn_standalone_worker rejects it after the fact;
    // exclude it here so the picker never offers a choice that always fails.
    let type_names: Vec<String> = config.worker_type_registry().user_facing_names()
        .into_iter().filter(|n| n != "Orchestrator").collect();
    let default_type = if type_names.iter().any(|n| n == "Implementor") {
        "Implementor".to_string()
    } else {
        type_names.first().cloned().unwrap_or_default()
    };

    let brains: Vec<BrainOption> = ninox_ui_shared::spawn_form::brain_choices(config)
        .into_iter()
        .map(|c| BrainOption { name: c.name, writable: c.writable, selected: c.selected })
        .collect();
    let brain_names: Vec<String> = brains.iter().map(|b| b.name.clone()).collect();
    let cursor = brain_names.first().cloned().unwrap_or_else(|| "default".to_string());

    let fields = vec![
        field(FieldId::Kind, "Kind", "Orchestrator".to_string(), Kind::Choice,
              vec!["Orchestrator".to_string(), "Standalone".to_string()]),
        field(FieldId::Name, "Name", String::new(), Kind::Text, Vec::new()),
        field(FieldId::Workspace, "Workspace", String::new(), Kind::Text, Vec::new()),
        field(FieldId::WorkerType, "Worker type", default_type, Kind::Choice, type_names),
        field(FieldId::Harness, "Agent", String::new(), Kind::Choice, harness_names),
        field(FieldId::Model, "Model", String::new(), Kind::Choice, Vec::new()),
        field(FieldId::CustomModel, "Model id", String::new(), Kind::Text, Vec::new()),
        field(FieldId::Brains, "Brains this tree reads", cursor, Kind::Choice, brain_names),
        field(FieldId::BrainWrite, "Writes to", String::new(), Kind::Choice, Vec::new()),
        field(FieldId::Prompt, "Initial prompt", String::new(), Kind::Text, Vec::new()),
    ];
    let mut form = SpawnForm { fields, focus: 0, error: None, config: config.clone(), registry, brains, harness_picked: false, model_picked: false };
    sync_brain_write(&mut form, Some("default"));
    sync_to_resolved(&mut form);
    form
}

/// The agent a submit would launch with, under the one model rule.
pub fn resolved(form: &SpawnForm) -> ninox_core::model_resolution::ResolvedAgent {
    let worker_type = get(form, &FieldId::WorkerType).value.clone();
    let harness = get(form, &FieldId::Harness).value.clone();
    let model = picked_model(form);
    ninox_ui_shared::spawn_form::resolve_dialog_agent(
        &form.config, kind(form), &worker_type,
        form.harness_picked.then_some(harness.as_str()),
        form.model_picked.then_some(model.as_deref()),
    )
}

/// Points whichever of Agent/Model the user hasn't picked at the resolved
/// default, and refreshes Model's options for the current harness.
fn sync_to_resolved(form: &mut SpawnForm) {
    let r = resolved(form);
    if !form.harness_picked {
        get_mut(form, &FieldId::Harness).value = r.agent.harness.clone();
    }
    let harness = get(form, &FieldId::Harness).value.clone();
    let current = if form.model_picked {
        Some(get(form, &FieldId::Model).value.clone()).filter(|v| v != HARNESS_DEFAULT_OPTION && v != CUSTOM_SENTINEL)
    } else {
        r.agent.model.clone()
    };
    let opts = model_choices(&form.registry, &harness, current.as_deref());
    let model_picked = form.model_picked;
    let m = get_mut(form, &FieldId::Model);
    if !model_picked {
        m.value = r.agent.model.unwrap_or_else(|| HARNESS_DEFAULT_OPTION.to_string());
    }
    m.options = opts;
}

fn get<'a>(form: &'a SpawnForm, id: &FieldId) -> &'a Field {
    form.fields.iter().find(|f| &f.id == id).expect("field always present")
}

fn get_mut<'a>(form: &'a mut SpawnForm, id: &FieldId) -> &'a mut Field {
    form.fields.iter_mut().find(|f| &f.id == id).expect("field always present")
}

pub fn kind(form: &SpawnForm) -> SpawnKind {
    match get(form, &FieldId::Kind).value.as_str() {
        "Standalone" => SpawnKind::Standalone,
        _ => SpawnKind::Orchestrator,
    }
}

/// An Orchestrator has no workspace of its own and no worker type (it's
/// always a hidden `Orchestrator`-type spawn under the hood) — so those two
/// fields exist only to collect input for a Standalone spawn.
pub fn visible(form: &SpawnForm, id: &FieldId) -> bool {
    match id {
        FieldId::Workspace | FieldId::WorkerType => kind(form) == SpawnKind::Standalone,
        FieldId::CustomModel => get(form, &FieldId::Model).value == CUSTOM_SENTINEL,
        _ => true,
    }
}

fn visible_indices(form: &SpawnForm) -> Vec<usize> {
    form.fields.iter().enumerate().filter(|(_, f)| visible(form, &f.id)).map(|(i, _)| i).collect()
}

pub fn next_focus(form: &SpawnForm, back: bool) -> usize {
    let vis = visible_indices(form);
    if vis.is_empty() {
        return form.focus;
    }
    let pos = vis.iter().position(|&i| i == form.focus).unwrap_or(0);
    let n = vis.len();
    vis[if back { (pos + n - 1) % n } else { (pos + 1) % n }]
}

/// Steps the focused `Choice` field's options (wrapping). Stepping `Harness`
/// recomputes `Model`'s options for the newly selected harness and resets
/// `Model` to its first option.
pub fn step(form: &mut SpawnForm, back: bool) {
    let id = form.fields[form.focus].id.clone();
    let f = &mut form.fields[form.focus];
    if f.kind != Kind::Choice || f.options.is_empty() {
        return;
    }
    let pos = f.options.iter().position(|o| o == &f.value).unwrap_or(0);
    let n = f.options.len();
    f.value = f.options[if back { (pos + n - 1) % n } else { (pos + 1) % n }].clone();

    match id {
        FieldId::Harness => {
            form.harness_picked = true;
            form.model_picked = false;
            sync_to_resolved(form);
        }
        FieldId::Model => form.model_picked = true,
        FieldId::Kind | FieldId::WorkerType => sync_to_resolved(form),
        _ => {}
    }
}

fn touch_if_custom_model(form: &mut SpawnForm) {
    if form.fields[form.focus].id == FieldId::CustomModel {
        form.model_picked = true;
    }
}

/// Recomputes "Writes to" from the writable selected brains, keeping the
/// current pick (or `prefer`) when it's still eligible.
fn sync_brain_write(form: &mut SpawnForm, prefer: Option<&str>) {
    let opts: Vec<String> = form.brains.iter().filter(|b| b.selected && b.writable).map(|b| b.name.clone()).collect();
    let f = get_mut(form, &FieldId::BrainWrite);
    let keep = [Some(f.value.as_str()), prefer].into_iter().flatten().find(|v| opts.iter().any(|o| o == v)).map(str::to_string);
    f.value = keep.or_else(|| opts.first().cloned()).unwrap_or_default();
    f.options = opts;
}

/// Space on the Brains field: toggles the brain under the cursor. The last
/// selected brain can't be deselected — a session always reads something.
pub fn toggle_brain(form: &mut SpawnForm) {
    if form.fields[form.focus].id != FieldId::Brains {
        return;
    }
    let cursor = form.fields[form.focus].value.clone();
    let selected = form.brains.iter().filter(|b| b.selected).count();
    if let Some(b) = form.brains.iter_mut().find(|b| b.name == cursor) {
        if !(b.selected && selected == 1) {
            b.selected = !b.selected;
        }
    }
    sync_brain_write(form, None);
}

/// Appends `c` to the focused `Text` field (`Prompt` included — a plain
/// Enter on the prompt field pushes `'\n'` rather than submitting, now that
/// `ctrl+enter` owns submit).
pub fn push_char(form: &mut SpawnForm, c: char) {
    touch_if_custom_model(form);
    let f = &mut form.fields[form.focus];
    if f.kind == Kind::Text {
        f.value.push(c);
    }
}

pub fn backspace(form: &mut SpawnForm) {
    touch_if_custom_model(form);
    let f = &mut form.fields[form.focus];
    if f.kind == Kind::Text {
        f.value.pop();
    }
}

pub fn paste(form: &mut SpawnForm, text: &str) {
    touch_if_custom_model(form);
    let f = &mut form.fields[form.focus];
    if f.kind == Kind::Text {
        f.value.push_str(text);
    }
}

/// The user's own Model pick. `CustomModel`'s text only counts while
/// `Model` is actually parked on the `custom…` sentinel (matching
/// `visible`'s own gating) — otherwise a value typed earlier and left
/// behind after stepping back to a real option would silently outrank it,
/// and a blank custom field under the sentinel would otherwise submit the
/// literal string `"custom…"` as the model id.
fn picked_model(form: &SpawnForm) -> Option<String> {
    let model_value = get(form, &FieldId::Model).value.clone();
    if model_value == HARNESS_DEFAULT_OPTION {
        return None;
    }
    if model_value == CUSTOM_SENTINEL {
        let custom = get(form, &FieldId::CustomModel).value.trim().to_string();
        return Some(custom).filter(|c| !c.is_empty());
    }
    Some(model_value).filter(|m| !m.is_empty())
}

/// The dialog's "Model: X (from …)" line.
pub fn model_line(form: &SpawnForm) -> String {
    ninox_ui_shared::spawn_form::effective_model_line(&resolved(form))
}

/// What `ctrl+enter` hands off to `Action::Spawn` once a form validates.
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnSubmission {
    pub kind: SpawnKind,
    pub name: String,
    pub workspace: Option<String>,
    pub worker_type: Option<String>,
    pub harness: String,
    pub model: Option<String>,
    /// Catalogue names, not indices — the submit handler reloads
    /// `AppConfig` and resolves them with `BrainSet::select`, so a
    /// config.toml edit while the modal was open can't shift them.
    pub brain_reads: Vec<String>,
    pub brain_write: String,
    pub prompt: Option<String>,
}

/// Validates the form and builds a `SpawnSubmission`, or returns the
/// human-readable refusal reason to show in `form.error` (the modal stays
/// open either way — the caller decides that). `workspace`/`worker_type`
/// are only read (and only required) for a Standalone spawn — an
/// Orchestrator submission carries `None` for both regardless of what was
/// typed before a kind switch.
pub fn try_submit(form: &SpawnForm) -> Result<SpawnSubmission, String> {
    let name = get(form, &FieldId::Name).value.trim().to_string();
    if name.is_empty() {
        return Err("name is required".to_string());
    }
    let spawn_kind = kind(form);
    let (workspace, worker_type) = if spawn_kind == SpawnKind::Standalone {
        let workspace = get(form, &FieldId::Workspace).value.trim().to_string();
        if workspace.is_empty() {
            return Err("workspace is required for a standalone session".to_string());
        }
        (Some(workspace), Some(get(form, &FieldId::WorkerType).value.clone()))
    } else {
        (None, None)
    };
    let agent = resolved(form).agent;
    let (harness, model) = (agent.harness, agent.model);
    let brain_reads: Vec<String> = form.brains.iter().filter(|b| b.selected).map(|b| b.name.clone()).collect();
    let brain_write = get(form, &FieldId::BrainWrite).value.clone();
    if brain_write.is_empty() {
        return Err("pick at least one writable brain to write to".to_string());
    }
    let prompt = Some(get(form, &FieldId::Prompt).value.clone()).filter(|p| !p.trim().is_empty());

    Ok(SpawnSubmission { kind: spawn_kind, name, workspace, worker_type, harness, model, brain_reads, brain_write, prompt })
}

/// The cost-estimate footer's inputs: the effective model (if any) feeds
/// `static_prior`/`Store::cost_samples`, read with the focused harness.
pub fn estimate_inputs(form: &SpawnForm) -> (String, Option<String>) {
    let r = resolved(form);
    (r.agent.harness, r.display_model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::harness::HarnessSpec;

    fn two_harness_config() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.harnesses.insert(
            "codex".to_string(),
            HarnessSpec { enabled: true, known_models: vec!["gpt-5".to_string()], ..HarnessSpec::default() },
        );
        cfg
    }

    #[test]
    fn build_lists_every_field_in_order() {
        let f = build(&AppConfig::default());
        assert_eq!(
            f.fields.iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
            vec![
                FieldId::Kind, FieldId::Name, FieldId::Workspace, FieldId::WorkerType,
                FieldId::Harness, FieldId::Model, FieldId::CustomModel, FieldId::Brains, FieldId::BrainWrite,
                FieldId::Prompt,
            ]
        );
    }

    #[test]
    fn worker_type_defaults_to_implementor() {
        let f = build(&AppConfig::default());
        assert_eq!(get(&f, &FieldId::WorkerType).value, "Implementor");
    }

    #[test]
    fn workspace_and_worker_type_hidden_for_orchestrator_kind() {
        let f = build(&AppConfig::default());
        assert!(!visible(&f, &FieldId::Workspace));
        assert!(!visible(&f, &FieldId::WorkerType));
    }

    #[test]
    fn tab_skips_hidden_fields() {
        let mut f = build(&AppConfig::default());
        f.focus = f.fields.iter().position(|x| x.id == FieldId::Name).unwrap();
        let n = next_focus(&f, false);
        assert_eq!(f.fields[n].id, FieldId::Harness); // skips Workspace + WorkerType, both hidden
    }

    #[test]
    fn switching_kind_back_to_orchestrator_keeps_typed_workspace_but_hides_it() {
        let mut f = build(&AppConfig::default());
        let ws_idx = f.fields.iter().position(|x| x.id == FieldId::Workspace).unwrap();

        f.focus = f.fields.iter().position(|x| x.id == FieldId::Kind).unwrap();
        step(&mut f, false); // -> Standalone
        assert_eq!(kind(&f), SpawnKind::Standalone);

        f.focus = ws_idx;
        push_char(&mut f, '~');
        push_char(&mut f, '/');
        push_char(&mut f, 'x');

        f.focus = f.fields.iter().position(|x| x.id == FieldId::Kind).unwrap();
        step(&mut f, true); // back -> Orchestrator

        assert_eq!(kind(&f), SpawnKind::Orchestrator);
        assert!(!visible(&f, &FieldId::Workspace));
        assert_eq!(f.fields[ws_idx].value, "~/x");
    }

    #[test]
    fn changing_harness_resets_model_options() {
        let cfg = two_harness_config();
        let mut f = build(&cfg);
        f.focus = f.fields.iter().position(|x| x.id == FieldId::Harness).unwrap();
        step(&mut f, false);
        assert_eq!(get(&f, &FieldId::Harness).value, "codex");
        let model = get(&f, &FieldId::Model);
        assert_eq!(model.value, HARNESS_DEFAULT_OPTION);
        assert_eq!(model.options, vec![HARNESS_DEFAULT_OPTION.to_string(), "gpt-5".to_string(), CUSTOM_SENTINEL.to_string()]);
    }

    #[test]
    fn custom_model_field_visible_only_when_model_is_custom_sentinel() {
        let mut f = build(&AppConfig::default());
        assert!(!visible(&f, &FieldId::CustomModel));
        let model = get_mut(&mut f, &FieldId::Model);
        model.value = CUSTOM_SENTINEL.to_string();
        assert!(visible(&f, &FieldId::CustomModel));
    }

    fn set_value(f: &mut SpawnForm, id: FieldId, value: &str) {
        get_mut(f, &id).value = value.to_string();
    }

    #[test]
    fn worker_type_options_exclude_orchestrator() {
        let f = build(&AppConfig::default());
        assert!(!get(&f, &FieldId::WorkerType).options.iter().any(|n| n == "Orchestrator"));
    }

    #[test]
    fn picked_model_is_none_when_custom_sentinel_is_blank() {
        let mut f = build(&AppConfig::default());
        set_value(&mut f, FieldId::Model, CUSTOM_SENTINEL);
        assert_ne!(picked_model(&f).as_deref(), Some(CUSTOM_SENTINEL), "must never submit the literal sentinel as a model id");
    }

    #[test]
    fn picked_model_uses_custom_text_only_while_on_the_sentinel() {
        let mut f = build(&AppConfig::default());
        set_value(&mut f, FieldId::Model, CUSTOM_SENTINEL);
        set_value(&mut f, FieldId::CustomModel, "my-custom-model");
        assert_eq!(picked_model(&f).as_deref(), Some("my-custom-model"));

        // Stepping back to a real option must drop the stale custom text,
        // not let it keep outranking the now-visible real selection.
        set_value(&mut f, FieldId::Model, "claude-opus-5-5");
        assert_eq!(picked_model(&f).as_deref(), Some("claude-opus-5-5"));
    }

    fn models_config() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.orchestrator.model = Some("orch-m".into());
        cfg.worker.model = Some("worker-m".into());
        cfg.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Researcher".into(), model: Some("research-m".into()), ..Default::default()
        });
        cfg
    }

    fn step_field(f: &mut SpawnForm, id: FieldId, back: bool) {
        f.focus = f.fields.iter().position(|x| x.id == id).unwrap();
        step(f, back);
    }

    #[test]
    fn build_preselects_the_orchestrator_setting() {
        let f = build(&models_config());
        assert_eq!(get(&f, &FieldId::Model).value, "orch-m");
        assert_eq!(model_line(&f), "Model: orch-m (from setting)");
    }

    #[test]
    fn nothing_configured_preselects_the_builtin_role_default() {
        let mut f = build(&AppConfig::default());
        assert_eq!(get(&f, &FieldId::Model).value, "claude-opus-5-5");
        assert_eq!(model_line(&f), "Model: claude-opus-5-5 (claude-code default)");
        set_value(&mut f, FieldId::Name, "demo");
        assert_eq!(try_submit(&f).unwrap().model.as_deref(), Some("claude-opus-5-5"));
        step_field(&mut f, FieldId::Kind, false);
        assert_eq!(get(&f, &FieldId::Model).value, "claude-sonnet-5-5");
    }

    #[test]
    fn kind_and_type_changes_follow_the_resolved_default() {
        let mut f = build(&models_config());
        step_field(&mut f, FieldId::Kind, false); // -> Standalone
        assert_eq!(get(&f, &FieldId::Model).value, "worker-m");
        set_value(&mut f, FieldId::WorkerType, "Implementor");
        f.focus = f.fields.iter().position(|x| x.id == FieldId::WorkerType).unwrap();
        // Implementor -> Researcher, wherever it sorts.
        while get(&f, &FieldId::WorkerType).value != "Researcher" {
            step(&mut f, false);
        }
        assert_eq!(get(&f, &FieldId::Model).value, "research-m");
        assert_eq!(model_line(&f), "Model: research-m (from type)");
    }

    #[test]
    fn a_user_model_pick_is_explicit_and_survives_kind_changes() {
        let mut f = build(&models_config());
        set_value(&mut f, FieldId::Name, "demo");
        step_field(&mut f, FieldId::Model, true);
        let picked = get(&f, &FieldId::Model).value.clone();
        assert_ne!(picked, "orch-m");
        step_field(&mut f, FieldId::Kind, false);
        assert_eq!(get(&f, &FieldId::Model).value, picked);
        assert_eq!(resolved(&f).source, ninox_core::model_resolution::ModelSource::Explicit);
    }

    #[test]
    fn try_submit_untouched_form_carries_the_resolved_model() {
        let mut f = build(&models_config());
        set_value(&mut f, FieldId::Name, "demo");
        let sub = try_submit(&f).unwrap();
        assert_eq!((sub.harness.as_str(), sub.model.as_deref()), ("claude-code", Some("orch-m")));
    }

    #[test]
    fn picking_harness_default_launches_without_a_model_even_with_a_setting() {
        let mut f = build(&models_config());
        set_value(&mut f, FieldId::Name, "demo");
        f.focus = f.fields.iter().position(|x| x.id == FieldId::Model).unwrap();
        while get(&f, &FieldId::Model).value != HARNESS_DEFAULT_OPTION {
            step(&mut f, false);
        }
        assert_eq!(try_submit(&f).unwrap().model, None);
        assert_eq!(model_line(&f), "Model: harness default");
    }

    #[test]
    fn picking_a_harness_shows_and_launches_that_harness_resolved_model() {
        let mut cfg = two_harness_config();
        cfg.orchestrator.model = Some("orch-m".into());
        let mut f = build(&cfg);
        set_value(&mut f, FieldId::Name, "demo");
        step_field(&mut f, FieldId::Harness, false); // -> codex: [orchestrator] is claude-code
        assert_eq!(get(&f, &FieldId::Model).value, HARNESS_DEFAULT_OPTION);
        assert_eq!(try_submit(&f).unwrap().model, None);
        step_field(&mut f, FieldId::Harness, true); // back -> claude-code
        assert_eq!(get(&f, &FieldId::Model).value, "orch-m", "the picker shows what will launch");
        assert_eq!(try_submit(&f).unwrap().model.as_deref(), Some("orch-m"));
    }

    #[test]
    fn a_type_on_a_disabled_harness_falls_back_but_later_changes_still_follow_defaults() {
        let mut cfg = models_config();
        cfg.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Codexer".into(), harness: "codex".into(), model: Some("gpt-5".into()), ..Default::default()
        });
        let mut f = build(&cfg);
        step_field(&mut f, FieldId::Kind, false);
        f.focus = f.fields.iter().position(|x| x.id == FieldId::WorkerType).unwrap();
        while get(&f, &FieldId::WorkerType).value != "Codexer" {
            step(&mut f, false);
        }
        assert_eq!(get(&f, &FieldId::Harness).value, "claude-code");
        while get(&f, &FieldId::WorkerType).value != "Researcher" {
            step(&mut f, false);
        }
        assert_eq!(get(&f, &FieldId::Model).value, "research-m");
    }

    #[test]
    fn try_submit_rejects_empty_name() {
        let f = build(&AppConfig::default());
        assert_eq!(try_submit(&f), Err("name is required".to_string()));
    }

    #[test]
    fn try_submit_rejects_empty_workspace_for_standalone() {
        let mut f = build(&AppConfig::default());
        set_value(&mut f, FieldId::Name, "demo");
        set_value(&mut f, FieldId::Kind, "Standalone");
        assert_eq!(try_submit(&f), Err("workspace is required for a standalone session".to_string()));
    }

    #[test]
    fn try_submit_orchestrator_ignores_workspace_and_type_even_if_filled() {
        let mut f = build(&AppConfig::default());
        set_value(&mut f, FieldId::Name, "demo");
        set_value(&mut f, FieldId::Kind, "Standalone");
        set_value(&mut f, FieldId::Workspace, "~/x");
        set_value(&mut f, FieldId::WorkerType, "Researcher");
        set_value(&mut f, FieldId::Kind, "Orchestrator");
        let sub = try_submit(&f).unwrap();
        assert_eq!(sub.kind, SpawnKind::Orchestrator);
        assert_eq!(sub.workspace, None);
        assert_eq!(sub.worker_type, None);
    }

    #[test]
    fn try_submit_standalone_carries_workspace_and_type() {
        let mut f = build(&AppConfig::default());
        set_value(&mut f, FieldId::Name, "demo");
        set_value(&mut f, FieldId::Kind, "Standalone");
        set_value(&mut f, FieldId::Workspace, "~/x");
        set_value(&mut f, FieldId::WorkerType, "Researcher");
        let sub = try_submit(&f).unwrap();
        assert_eq!(sub.kind, SpawnKind::Standalone);
        assert_eq!(sub.workspace.as_deref(), Some("~/x"));
        assert_eq!(sub.worker_type.as_deref(), Some("Researcher"));
        assert_eq!(sub.brain_reads, vec!["default"]);
        assert_eq!(sub.brain_write, "default");
    }

    #[test]
    fn try_submit_empty_prompt_is_none() {
        let mut f = build(&AppConfig::default());
        set_value(&mut f, FieldId::Name, "demo");
        assert_eq!(try_submit(&f).unwrap().prompt, None);
        set_value(&mut f, FieldId::Prompt, "do x");
        assert_eq!(try_submit(&f).unwrap().prompt, Some("do x".to_string()));
    }

    fn multi_brain_config() -> AppConfig {
        let mut cfg = AppConfig::default();
        let cat = |name: &str, enabled: bool, access: &str| ninox_core::config::CatalogueRef {
            name: name.into(), path: format!("/scratch/{name}").into(), remote: None, endpoint: None,
            region: None, cache_ttl_secs: None, enabled, cadence: "manual".into(), access: access.into(),
        };
        cfg.brain.catalogues = vec![cat("team", true, "read_write"), cat("wiki", true, "read_only"), cat("old", false, "read_write")];
        cfg
    }

    #[test]
    fn brains_default_to_every_searchable_source_writing_default() {
        let f = build(&multi_brain_config());
        let picked: Vec<_> = f.brains.iter().filter(|b| b.selected).map(|b| b.name.as_str()).collect();
        assert_eq!(picked, vec!["default", "team", "wiki"]);
        let w = get(&f, &FieldId::BrainWrite);
        assert_eq!(w.value, "default");
        assert_eq!(w.options, vec!["default", "team"], "read-only wiki is never a write option");
    }

    #[test]
    fn toggling_brains_keeps_writes_to_within_the_writable_selection() {
        let mut f = build(&multi_brain_config());
        f.focus = f.fields.iter().position(|x| x.id == FieldId::Brains).unwrap();
        toggle_brain(&mut f); // deselect default
        assert_eq!(get(&f, &FieldId::BrainWrite).value, "team");
        step(&mut f, false); // cursor -> team
        toggle_brain(&mut f); // deselect team: only read-only wiki left
        let sub = { set_value(&mut f, FieldId::Name, "demo"); try_submit(&f) };
        assert_eq!(sub, Err("pick at least one writable brain to write to".to_string()));
        step(&mut f, false); // cursor -> wiki
        toggle_brain(&mut f); // last selected brain can't be dropped
        assert!(f.brains.iter().any(|b| b.name == "wiki" && b.selected));
        step(&mut f, false); // cursor -> old
        toggle_brain(&mut f);
        let sub = try_submit(&f).unwrap();
        assert_eq!(sub.brain_reads, vec!["wiki", "old"]);
        assert_eq!(sub.brain_write, "old");
    }

    #[test]
    fn backspace_only_affects_text_fields() {
        let mut f = build(&AppConfig::default());
        f.focus = f.fields.iter().position(|x| x.id == FieldId::Name).unwrap();
        push_char(&mut f, 'a');
        push_char(&mut f, 'b');
        backspace(&mut f);
        assert_eq!(get(&f, &FieldId::Name).value, "a");

        f.focus = f.fields.iter().position(|x| x.id == FieldId::Kind).unwrap();
        backspace(&mut f); // Choice field — no-op
        assert_eq!(get(&f, &FieldId::Kind).value, "Orchestrator");
    }
}
