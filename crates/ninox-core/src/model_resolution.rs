//! The one rule for which harness and model a new session launches with.
//! Every spawn path builds a [`SpawnModelRequest`] and calls
//! [`effective_agent`]; restore/resume/restart pass the session row's own
//! model as the explicit pick so a session keeps the model it started on.

use serde::Serialize;

use crate::config::{AgentConfig, AppConfig};
use crate::worker_type::WorkerTypeSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Orchestrator,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Explicit,
    Type,
    Setting,
    /// [`HARNESS_ROLE_DEFAULTS`]: ninox's built-in pick for this harness and role.
    BuiltinDefault,
    HarnessDefault,
}

impl ModelSource {
    pub fn label(self) -> &'static str {
        match self {
            ModelSource::Explicit       => "explicit",
            ModelSource::Type           => "type",
            ModelSource::Setting        => "setting",
            ModelSource::BuiltinDefault => "built-in default",
            ModelSource::HarnessDefault => "harness default",
        }
    }
}

pub struct HarnessRoleDefault {
    pub harness: &'static str,
    pub role:    Role,
    pub model:   &'static str,
    /// Human name shown in Settings placeholders.
    pub label:   &'static str,
}

/// Ninox's out-of-the-box model per harness and role, used when neither the
/// worker type nor the user's `[orchestrator]`/`[worker]` setting names one.
/// A harness with no row runs its own default. Bump a model here, nowhere else.
pub const HARNESS_ROLE_DEFAULTS: &[HarnessRoleDefault] = &[
    HarnessRoleDefault { harness: "claude-code", role: Role::Orchestrator, model: "claude-opus-5-5",   label: "Opus 5.5" },
    HarnessRoleDefault { harness: "claude-code", role: Role::Worker,       model: "claude-sonnet-5-5", label: "Sonnet 5.5" },
];

pub fn harness_role_default(harness: &str, role: Role) -> Option<&'static HarnessRoleDefault> {
    HARNESS_ROLE_DEFAULTS.iter().find(|d| d.harness == harness && d.role == role)
}

/// What an unset `[orchestrator]`/`[worker]` model falls back to, e.g.
/// `Sonnet 5.5 (claude-code default)` or `harness default`.
pub fn unset_setting_placeholder(role: Role, config: &AppConfig) -> String {
    let harness = &role_setting(role, config).harness;
    match harness_role_default(harness, role) {
        Some(d) => format!("{} ({harness} default)", d.label),
        None    => "harness default".to_string(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct SpawnModelRequest<'a> {
    pub role:             Option<Role>,
    pub worker_type:      Option<&'a WorkerTypeSpec>,
    pub explicit_harness: Option<&'a str>,
    pub explicit_model:   Option<&'a str>,
}

impl<'a> SpawnModelRequest<'a> {
    pub fn worker(worker_type: Option<&'a WorkerTypeSpec>) -> Self {
        Self { role: Some(Role::Worker), worker_type, ..Self::default() }
    }

    pub fn orchestrator(worker_type: Option<&'a WorkerTypeSpec>) -> Self {
        Self { role: Some(Role::Orchestrator), worker_type, ..Self::default() }
    }

    pub fn with_harness(mut self, harness: Option<&'a str>) -> Self {
        self.explicit_harness = harness.filter(|h| !h.trim().is_empty());
        self
    }

    pub fn with_model(mut self, model: Option<&'a str>) -> Self {
        self.explicit_model = model.filter(|m| !m.trim().is_empty());
        self
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    /// `agent.model` is `None` when the harness default applies, so the
    /// launch command carries no `--model` beyond the spec's own default.
    pub agent:  AgentConfig,
    pub source: ModelSource,
    /// What will actually run: `agent.model`, else the harness spec's
    /// default model. `None` means the harness CLI picks.
    pub display_model: Option<String>,
}

impl ResolvedAgent {
    pub fn describe(&self) -> String {
        match (self.source, self.display_model.as_deref()) {
            (ModelSource::HarnessDefault, None)    => "harness default".to_string(),
            (ModelSource::HarnessDefault, Some(m)) => format!("{m} (harness default)"),
            (ModelSource::BuiltinDefault, model)   => format!("{} ({} default)", model.unwrap_or("harness default"), self.agent.harness),
            (source, model) => format!("{} (from {})", model.unwrap_or("harness default"), source.label()),
        }
    }
}

pub fn role_setting(role: Role, config: &AppConfig) -> &AgentConfig {
    match role {
        Role::Orchestrator => &config.orchestrator,
        Role::Worker       => &config.worker,
    }
}

pub fn effective_agent(req: &SpawnModelRequest<'_>, config: &AppConfig) -> ResolvedAgent {
    let role = req.role.unwrap_or(Role::Worker);
    let setting = role_setting(role, config);

    // An orchestrator's harness is the user's `[orchestrator]` choice; the
    // Orchestrator type's harness is a serde default (claude-code), not a
    // choice, and would silently override it.
    let harness = req.explicit_harness.map(str::to_string).unwrap_or_else(|| match (role, req.worker_type) {
        (Role::Worker, Some(t)) => t.harness.clone(),
        _ => setting.harness.clone(),
    });

    let (model, source) = if let Some(m) = req.explicit_model {
        (Some(m.trim().to_string()), ModelSource::Explicit)
    } else if let Some(m) = req.worker_type.filter(|t| t.harness == harness).and_then(|t| nonempty(&t.model)) {
        (Some(m), ModelSource::Type)
    } else if let Some(m) = (setting.harness == harness).then(|| nonempty(&setting.model)).flatten() {
        (Some(m), ModelSource::Setting)
    } else if let Some(d) = harness_role_default(&harness, role) {
        (Some(d.model.to_string()), ModelSource::BuiltinDefault)
    } else {
        (None, ModelSource::HarnessDefault)
    };

    let display_model = model.clone().or_else(|| config.registry().spec(&harness).model);
    ResolvedAgent { agent: AgentConfig { harness, model }, source, display_model }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRow {
    /// `orchestrator`, `worker`, `type:<name>` or `planning`.
    pub scope:   String,
    pub harness: String,
    pub model:   Option<String>,
    pub source:  ModelSource,
}

/// What `ninox models show` prints: the resolved model for each role and
/// each worker type, plus `[planning]` (one-shot plan runs, not sessions).
pub fn model_table(config: &AppConfig) -> Vec<ModelRow> {
    let row = |scope: String, r: ResolvedAgent| ModelRow { scope, harness: r.agent.harness, model: r.display_model, source: r.source };
    let registry = config.worker_type_registry();
    let mut rows = vec![
        row("orchestrator".into(), effective_agent(&SpawnModelRequest::orchestrator(registry.get("Orchestrator")), config)),
        row("worker".into(), effective_agent(&SpawnModelRequest::worker(None), config)),
    ];
    for name in registry.names() {
        let spec = registry.get(&name).expect("listed name resolves");
        let req = if name == "Orchestrator" { SpawnModelRequest::orchestrator(Some(spec)) } else { SpawnModelRequest::worker(Some(spec)) };
        rows.push(row(format!("type:{name}"), effective_agent(&req, config)));
    }
    let planning_model = nonempty(&config.planning.model);
    rows.push(ModelRow {
        scope:   "planning".into(),
        harness: config.planning.harness.clone(),
        source:  if planning_model.is_some() { ModelSource::Setting } else { ModelSource::HarnessDefault },
        model:   planning_model.or_else(|| config.registry().spec(&config.planning.harness).model),
    });
    rows
}

pub fn format_model_table(rows: &[ModelRow]) -> String {
    let w = rows.iter().map(|r| r.scope.len()).max().unwrap_or(0);
    let hw = rows.iter().map(|r| r.harness.len()).max().unwrap_or(0);
    rows.iter()
        .map(|r| format!(
            "{:<w$}  {:<hw$}  {} ({})",
            r.scope, r.harness, r.model.as_deref().unwrap_or("-"), r.source.label(),
        ))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A Worker Types row's model line: the type's own model, or what it
/// inherits, e.g. `model: inherits Worker default (claude-opus-5-5)`.
pub fn type_model_summary(spec: &WorkerTypeSpec, config: &AppConfig) -> String {
    let (role, label) = if spec.name == "Orchestrator" { (Role::Orchestrator, "Orchestrator") } else { (Role::Worker, "Worker") };
    let req = SpawnModelRequest { role: Some(role), worker_type: Some(spec), ..Default::default() };
    let r = effective_agent(&req, config);
    let shown = r.display_model.as_deref().unwrap_or("the harness picks");
    match (r.source, nonempty(&spec.model)) {
        (ModelSource::Type, Some(m)) => format!("model: {m}"),
        (_, Some(m)) => format!("model: {m} (unused: runs on {}, so {shown})", r.agent.harness),
        (ModelSource::Setting, None) => format!("model: inherits {label} default ({shown})"),
        (ModelSource::BuiltinDefault, None) => format!("model: inherits {label} default ({shown}, {} default)", r.agent.harness),
        _ => format!("model: harness default ({shown})"),
    }
}

/// A restore/resume/restart keeps the model the session started with.
pub fn relaunch_agent(session_harness: &str, session_model: Option<&str>) -> AgentConfig {
    AgentConfig { harness: session_harness.to_string(), model: session_model.map(str::to_string) }
}

fn nonempty(m: &Option<String>) -> Option<String> {
    m.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(orch: (&str, Option<&str>), worker: (&str, Option<&str>)) -> AppConfig {
        AppConfig {
            orchestrator: AgentConfig { harness: orch.0.into(), model: orch.1.map(Into::into) },
            worker: AgentConfig { harness: worker.0.into(), model: worker.1.map(Into::into) },
            ..AppConfig::default()
        }
    }

    fn ty(harness: &str, model: Option<&str>) -> WorkerTypeSpec {
        WorkerTypeSpec { name: "T".into(), harness: harness.into(), model: model.map(Into::into), ..Default::default() }
    }

    #[test]
    fn explicit_beats_type_and_setting() {
        let c = cfg(("claude-code", Some("o")), ("claude-code", Some("w")));
        let t = ty("claude-code", Some("t"));
        let r = effective_agent(&SpawnModelRequest::worker(Some(&t)).with_model(Some("x")), &c);
        assert_eq!(r.agent.model.as_deref(), Some("x"));
        assert_eq!(r.source, ModelSource::Explicit);
    }

    #[test]
    fn blank_explicit_model_is_ignored() {
        let c = cfg(("claude-code", None), ("claude-code", Some("w")));
        let r = effective_agent(&SpawnModelRequest::worker(None).with_model(Some("  ")), &c);
        assert_eq!(r.agent.model.as_deref(), Some("w"));
        assert_eq!(r.source, ModelSource::Setting);
    }

    #[test]
    fn type_beats_setting() {
        let c = cfg(("claude-code", Some("o")), ("claude-code", Some("w")));
        let t = ty("claude-code", Some("t"));
        let r = effective_agent(&SpawnModelRequest::worker(Some(&t)), &c);
        assert_eq!(r.agent.model.as_deref(), Some("t"));
        assert_eq!(r.source, ModelSource::Type);
    }

    #[test]
    fn type_without_model_inherits_worker_setting() {
        let c = cfg(("claude-code", Some("o")), ("claude-code", Some("w")));
        let t = ty("claude-code", None);
        let r = effective_agent(&SpawnModelRequest::worker(Some(&t)), &c);
        assert_eq!(r.agent.model.as_deref(), Some("w"));
        assert_eq!(r.source, ModelSource::Setting);
    }

    #[test]
    fn orchestrator_role_uses_orchestrator_setting() {
        let c = cfg(("claude-code", Some("o")), ("claude-code", Some("w")));
        let t = ty("claude-code", None);
        let r = effective_agent(&SpawnModelRequest::orchestrator(Some(&t)), &c);
        assert_eq!(r.agent.model.as_deref(), Some("o"));
        assert_eq!(r.source, ModelSource::Setting);
    }

    #[test]
    fn orchestrator_type_model_beats_orchestrator_setting() {
        let c = cfg(("claude-code", Some("o")), ("claude-code", None));
        let t = ty("claude-code", Some("t"));
        let r = effective_agent(&SpawnModelRequest::orchestrator(Some(&t)), &c);
        assert_eq!(r.agent.model.as_deref(), Some("t"));
    }

    #[test]
    fn orchestrator_harness_comes_from_setting_not_type_default() {
        let c = cfg(("codex", Some("o3")), ("claude-code", None));
        let t = ty("claude-code", Some("t"));
        let r = effective_agent(&SpawnModelRequest::orchestrator(Some(&t)), &c);
        assert_eq!(r.agent.harness, "codex");
        assert_eq!(r.agent.model.as_deref(), Some("o3"), "type model is for another harness");
    }

    #[test]
    fn nothing_set_uses_the_builtin_role_default_for_claude_code() {
        let c = cfg(("claude-code", None), ("claude-code", None));
        let w = effective_agent(&SpawnModelRequest::worker(None), &c);
        assert_eq!((w.agent.model.as_deref(), w.source), (Some("claude-sonnet-5-5"), ModelSource::BuiltinDefault));
        let o = effective_agent(&SpawnModelRequest::orchestrator(None), &c);
        assert_eq!(o.agent.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(o.describe(), "claude-opus-5-5 (claude-code default)");
        let t = ty("claude-code", None);
        assert_eq!(effective_agent(&SpawnModelRequest::worker(Some(&t)), &c).agent.model.as_deref(), Some("claude-sonnet-5-5"));
    }

    #[test]
    fn setting_and_type_beat_the_builtin_role_default() {
        let c = cfg(("claude-code", None), ("claude-code", Some("w")));
        assert_eq!(effective_agent(&SpawnModelRequest::worker(None), &c).agent.model.as_deref(), Some("w"));
        let c = cfg(("claude-code", None), ("claude-code", None));
        let t = ty("claude-code", Some("t"));
        assert_eq!(effective_agent(&SpawnModelRequest::worker(Some(&t)), &c).agent.model.as_deref(), Some("t"));
    }

    #[test]
    fn a_harness_without_a_builtin_row_runs_its_own_default() {
        let c = cfg(("claude-code", None), ("codex", None));
        let r = effective_agent(&SpawnModelRequest::worker(None), &c);
        assert_eq!((r.agent.model.clone(), r.source), (None, ModelSource::HarnessDefault));
        assert_eq!(r.describe(), "harness default");
    }

    #[test]
    fn builtin_role_defaults_are_unique_and_known_to_their_harness() {
        let reg = AppConfig::default().registry();
        for (i, d) in HARNESS_ROLE_DEFAULTS.iter().enumerate() {
            assert!(HARNESS_ROLE_DEFAULTS[..i].iter().all(|e| (e.harness, e.role) != (d.harness, d.role)), "duplicate row");
            assert!(reg.spec(d.harness).known_models.iter().any(|m| m == d.model), "{} not in {}'s known_models", d.model, d.harness);
        }
    }

    #[test]
    fn unset_setting_placeholder_names_the_builtin_default() {
        let c = cfg(("claude-code", None), ("codex", None));
        assert_eq!(unset_setting_placeholder(Role::Orchestrator, &c), "Opus 5.5 (claude-code default)");
        assert_eq!(unset_setting_placeholder(Role::Worker, &c), "harness default");
    }

    #[test]
    fn setting_for_another_harness_is_skipped() {
        let c = cfg(("claude-code", None), ("claude-code", Some("claude-opus-5-5")));
        let t = ty("codex", None);
        let r = effective_agent(&SpawnModelRequest::worker(Some(&t)), &c);
        assert_eq!(r.agent.harness, "codex");
        assert_eq!(r.agent.model, None);
        assert_eq!(r.source, ModelSource::HarnessDefault);
    }

    #[test]
    fn explicit_harness_overrides_type_harness() {
        let c = cfg(("claude-code", None), ("codex", Some("o3")));
        let t = ty("claude-code", Some("t"));
        let r = effective_agent(&SpawnModelRequest::worker(Some(&t)).with_harness(Some("codex")), &c);
        assert_eq!(r.agent.harness, "codex");
        assert_eq!(r.agent.model.as_deref(), Some("o3"));
        assert_eq!(r.source, ModelSource::Setting);
    }

    #[test]
    fn untyped_worker_uses_worker_harness() {
        let c = cfg(("claude-code", None), ("codex", Some("o3")));
        let r = effective_agent(&SpawnModelRequest::worker(None), &c);
        assert_eq!(r.agent.harness, "codex");
        assert_eq!(r.agent.model.as_deref(), Some("o3"));
    }

    #[test]
    fn display_model_falls_back_to_the_harness_spec_default() {
        let mut c = cfg(("claude-code", None), ("codex", None));
        c.harnesses.insert("codex".into(), crate::harness::HarnessSpec { model: Some("spec-m".into()), ..Default::default() });
        let r = effective_agent(&SpawnModelRequest::worker(None), &c);
        assert_eq!(r.agent.model, None);
        assert_eq!(r.display_model.as_deref(), Some("spec-m"));
        assert_eq!(r.describe(), "spec-m (harness default)");
    }

    #[test]
    fn describe_names_the_source() {
        let c = cfg(("claude-code", None), ("claude-code", Some("w")));
        assert_eq!(effective_agent(&SpawnModelRequest::worker(None), &c).describe(), "w (from setting)");
        let c = cfg(("claude-code", None), ("codex", None));
        assert_eq!(effective_agent(&SpawnModelRequest::worker(None), &c).describe(), "harness default");
    }

    #[test]
    fn model_table_lists_roles_types_and_planning() {
        let mut c = cfg(("claude-code", Some("o")), ("claude-code", Some("w")));
        c.worker_types.push(WorkerTypeSpec { name: "Researcher".into(), model: Some("r".into()), ..Default::default() });
        let rows = model_table(&c);
        let get = |scope: &str| rows.iter().find(|r| r.scope == scope).unwrap_or_else(|| panic!("{scope}"));
        assert_eq!(get("orchestrator").model.as_deref(), Some("o"));
        assert_eq!(get("worker").model.as_deref(), Some("w"));
        assert_eq!((get("type:Researcher").model.as_deref(), get("type:Researcher").source), (Some("r"), ModelSource::Type));
        assert_eq!((get("type:Implementor").model.as_deref(), get("type:Implementor").source), (Some("w"), ModelSource::Setting));
        assert_eq!(get("type:Orchestrator").model.as_deref(), Some("o"));
        assert_eq!(get("planning").source, ModelSource::HarnessDefault);
        let text = format_model_table(&rows);
        assert!(text.lines().any(|l| l.starts_with("type:Researcher") && l.ends_with("r (type)")), "{text}");
    }

    #[test]
    fn type_model_summary_names_own_inherited_or_harness_default() {
        let c = cfg(("claude-code", Some("o")), ("claude-code", Some("w")));
        assert_eq!(type_model_summary(&ty("claude-code", Some("t")), &c), "model: t");
        assert_eq!(type_model_summary(&ty("claude-code", None), &c), "model: inherits Worker default (w)");
        let orch = WorkerTypeSpec { name: "Orchestrator".into(), ..Default::default() };
        assert_eq!(type_model_summary(&orch, &c), "model: inherits Orchestrator default (o)");
        assert_eq!(type_model_summary(&ty("codex", None), &c), "model: harness default (the harness picks)");
        let c = cfg(("codex", None), ("claude-code", None));
        let orch = WorkerTypeSpec { name: "Orchestrator".into(), model: Some("claude-opus-5-5".into()), ..Default::default() };
        assert_eq!(type_model_summary(&orch, &c), "model: claude-opus-5-5 (unused: runs on codex, so the harness picks)");
    }

    #[test]
    fn relaunch_keeps_the_session_model() {
        let a = relaunch_agent("claude-code", Some("claude-opus-4-8"));
        assert_eq!(a.model.as_deref(), Some("claude-opus-4-8"));
    }
}
