//! Spawn-form pieces shared by the GUI's and TUI's spawn dialogs: what kind
//! of session a spawn creates, and the cost-estimate footer line. Moved out
//! of `ninox-gui` (where only the estimate math — not the rendering — was
//! pure and app-agnostic) so the TUI's own spawn form can reuse them without
//! depending on `ninox-gui`.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpawnKind {
    /// A new orchestrator in its own subdirectory of the orchestrator root.
    #[default]
    Orchestrator,
    /// An interactive, unattached session working in a user-supplied
    /// workspace (isolated via a git worktree when the workspace is a repo).
    Standalone,
}

/// Default worker type for a new spawn — the general-purpose role, per the
/// 29 Oct decision on MLOPS-5416's Type field ("Implementor by default").
pub const DEFAULT_WORKER_TYPE: &str = "Implementor";

/// Minimum number of historical non-zero `cost_usd` samples (for the exact
/// harness/model pairing) required before the footer estimate switches from
/// the static rough-prior range to a data-driven average.
const MIN_HISTORY_SAMPLES: usize = 3;

/// Rough static per-session priors for claude-code's curated models —
/// relative pricing only (fable-5 highest, haiku-4.5 lowest), **not** wired
/// to any pricing API. Everything else has no prior and builds its
/// estimate from filed history.
pub fn static_prior(harness: &str, model: Option<&str>) -> Option<(f64, f64)> {
    if harness != "claude-code" {
        return None;
    }
    match model {
        Some("claude-fable-5-2") | Some("claude-fable-5-1") | Some("claude-fable-5") => Some((4.0, 8.0)),
        Some("claude-opus-5-5")                           => Some((2.0, 4.0)),
        Some("claude-sonnet-5-5")                         => Some((1.0, 2.0)),
        Some("claude-haiku-5-5") | Some("claude-haiku-4-5-20251001") => Some((0.4, 1.2)),
        _                                                 => None,
    }
}

/// Footer cost-estimate text. Falls back to the static prior range until at
/// least `MIN_HISTORY_SAMPLES` non-zero cost samples exist for the exact
/// harness/model pairing (see `Store::cost_samples`, populated by the usage
/// poller — `ninox_core::lifecycle::usage`/`poller::poll_usage`), then shows
/// the historical per-session average. Pure function — no I/O — so it's
/// unit-testable without a live store.
pub fn estimate_text(prior: Option<(f64, f64)>, historical_costs: &[f64]) -> String {
    if historical_costs.len() >= MIN_HISTORY_SAMPLES {
        let avg = historical_costs.iter().sum::<f64>() / historical_costs.len() as f64;
        format!("≈ ${avg:.2} per session · average of {}", historical_costs.len())
    } else if let Some((lo, hi)) = prior {
        format!("est. ${lo:.0}–{hi:.0} / session")
    } else {
        "est. — builds from filed sessions".to_string()
    }
}

/// The agent a Spawn dialog would launch with, under the one model rule
/// (`ninox_core::model_resolution`). `explicit_*` are the user's own picks;
/// pass `None` while the dialog still shows the resolved defaults.
pub fn resolve_spawn_agent(
    config: &ninox_core::config::AppConfig,
    kind: SpawnKind,
    worker_type: &str,
    explicit_harness: Option<&str>,
    explicit_model: Option<&str>,
) -> ninox_core::model_resolution::ResolvedAgent {
    use ninox_core::model_resolution::{effective_agent, SpawnModelRequest};
    let registry = config.worker_type_registry();
    let request = match kind {
        SpawnKind::Orchestrator => SpawnModelRequest::orchestrator(registry.get("Orchestrator")),
        SpawnKind::Standalone   => SpawnModelRequest::worker(registry.get(worker_type)),
    };
    effective_agent(&request.with_harness(explicit_harness).with_model(explicit_model), config)
}

/// What a Spawn dialog launches, given the user's own picks so far.
/// `harness_pick`: a harness the user chose. `model_pick`: `Some(None)` when
/// the user chose "harness default", `Some(Some(m))` for a model they chose,
/// `None` while the model still follows the resolved default. A resolved
/// harness the registry has disabled falls back to the first enabled one,
/// re-resolved for that harness.
pub fn resolve_dialog_agent(
    config: &ninox_core::config::AppConfig,
    kind: SpawnKind,
    worker_type: &str,
    harness_pick: Option<&str>,
    model_pick: Option<Option<&str>>,
) -> ninox_core::model_resolution::ResolvedAgent {
    use ninox_core::model_resolution::ModelSource;
    let registry = config.registry();
    let enabled = registry.enabled_names();
    let mut r = resolve_spawn_agent(config, kind, worker_type, harness_pick, None);
    if harness_pick.is_none() && !enabled.contains(&r.agent.harness) {
        let fallback = enabled.first().cloned().unwrap_or_else(|| ninox_core::config::AppConfig::LOCKED_HARNESS.to_string());
        r = resolve_spawn_agent(config, kind, worker_type, Some(&fallback), None);
    }
    if let Some(model) = model_pick {
        let model = model.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);
        r.source = if model.is_some() { ModelSource::Explicit } else { ModelSource::HarnessDefault };
        r.display_model = model.clone().or_else(|| registry.spec(&r.agent.harness).model);
        r.agent.model = model;
    }
    r
}

/// The dialog's "Model: X (from …)" line.
pub fn effective_model_line(resolved: &ninox_core::model_resolution::ResolvedAgent) -> String {
    format!("Model: {}", resolved.describe())
}

/// The model a confirm would launch with: custom text (trimmed, non-empty)
/// wins over the picker selection, which wins over the spec's default.
pub fn effective_model(custom_model: Option<&str>, model: Option<&str>, spec: &ninox_core::harness::HarnessSpec) -> Option<String> {
    if let Some(c) = custom_model {
        let t = c.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    model.map(str::to_string).or_else(|| spec.model.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_priors_cover_claude_models_only() {
        assert_eq!(static_prior("claude-code", Some("claude-fable-5")),            Some((4.0, 8.0)));
        assert_eq!(static_prior("claude-code", Some("claude-fable-5-1")),          Some((4.0, 8.0)));
        assert_eq!(static_prior("claude-code", Some("claude-opus-5-5")),           Some((2.0, 4.0)));
        assert_eq!(static_prior("claude-code", Some("claude-sonnet-5-5")),         Some((1.0, 2.0)));
        assert_eq!(static_prior("claude-code", Some("claude-haiku-4-5-20251001")), Some((0.4, 1.2)));
        assert_eq!(static_prior("claude-code", None), None);
        assert_eq!(static_prior("codex", Some("gpt-4o")), None);
    }

    #[test]
    fn estimate_text_falls_back_to_static_range_when_no_history() {
        assert_eq!(estimate_text(Some((2.0, 4.0)), &[]), "est. $2–4 / session");
    }

    #[test]
    fn estimate_text_falls_back_below_min_sample_count() {
        // 2 samples — one short of MIN_HISTORY_SAMPLES (3).
        assert_eq!(estimate_text(Some((2.0, 4.0)), &[1.0, 3.0]), "est. $2–4 / session");
    }

    #[test]
    fn estimate_text_uses_historical_average_at_min_sample_count() {
        assert_eq!(estimate_text(Some((4.0, 8.0)), &[3.0, 5.0, 4.0]),
                   "≈ $4.00 per session · average of 3");
        // History wins even with no prior (e.g. a custom harness).
        assert_eq!(estimate_text(None, &[3.0, 5.0, 4.0]),
                   "≈ $4.00 per session · average of 3");
    }

    #[test]
    fn estimate_text_without_prior_or_history_says_so() {
        assert_eq!(estimate_text(None, &[]), "est. — builds from filed sessions");
    }

    #[test]
    fn effective_model_prefers_custom_text() {
        let spec = ninox_core::harness::HarnessSpec { model: Some("spec-m".into()), ..Default::default() };
        assert_eq!(effective_model(Some("  typed  "), Some("picked"), &spec).as_deref(), Some("typed"));
        // blank custom -> picker value
        assert_eq!(effective_model(Some("   "), Some("picked"), &spec).as_deref(), Some("picked"));
        // nothing picked -> spec default
        assert_eq!(effective_model(Some("   "), None, &spec).as_deref(), Some("spec-m"));
    }

    #[test]
    fn spawn_kind_defaults_to_orchestrator() {
        assert_eq!(SpawnKind::default(), SpawnKind::Orchestrator);
    }

    #[test]
    fn default_worker_type_is_implementor() {
        assert_eq!(DEFAULT_WORKER_TYPE, "Implementor");
    }

    fn cfg_with(worker_model: Option<&str>, orch_model: Option<&str>) -> ninox_core::config::AppConfig {
        let mut c = ninox_core::config::AppConfig::default();
        c.worker.model = worker_model.map(Into::into);
        c.orchestrator.model = orch_model.map(Into::into);
        c
    }

    #[test]
    fn spawn_dialog_orchestrator_kind_resolves_the_orchestrator_setting() {
        let c = cfg_with(Some("w"), Some("o"));
        let r = resolve_spawn_agent(&c, SpawnKind::Orchestrator, DEFAULT_WORKER_TYPE, None, None);
        assert_eq!(r.agent.model.as_deref(), Some("o"));
        assert_eq!(effective_model_line(&r), "Model: o (from setting)");
    }

    #[test]
    fn spawn_dialog_standalone_kind_resolves_the_worker_setting() {
        let c = cfg_with(Some("w"), Some("o"));
        let r = resolve_spawn_agent(&c, SpawnKind::Standalone, DEFAULT_WORKER_TYPE, None, None);
        assert_eq!(r.agent.model.as_deref(), Some("w"));
    }

    #[test]
    fn spawn_dialog_standalone_kind_prefers_the_types_model() {
        let mut c = cfg_with(Some("w"), None);
        c.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Researcher".into(), model: Some("t".into()), ..Default::default()
        });
        let r = resolve_spawn_agent(&c, SpawnKind::Standalone, "Researcher", None, None);
        assert_eq!(r.agent.model.as_deref(), Some("t"));
        assert_eq!(effective_model_line(&r), "Model: t (from type)");
    }

    #[test]
    fn spawn_dialog_explicit_pick_wins() {
        let c = cfg_with(Some("w"), Some("o"));
        let r = resolve_spawn_agent(&c, SpawnKind::Standalone, DEFAULT_WORKER_TYPE, Some("claude-code"), Some("x"));
        assert_eq!(r.agent.model.as_deref(), Some("x"));
        assert_eq!(effective_model_line(&r), "Model: x (from explicit)");
    }

    #[test]
    fn dialog_harness_default_pick_launches_without_a_model() {
        let c = cfg_with(Some("w"), Some("o"));
        let r = resolve_dialog_agent(&c, SpawnKind::Orchestrator, DEFAULT_WORKER_TYPE, None, Some(None));
        assert_eq!(r.agent.model, None);
        assert_eq!(effective_model_line(&r), "Model: harness default");
    }

    #[test]
    fn dialog_harness_pick_follows_that_harness_setting_until_a_model_is_picked() {
        let mut c = cfg_with(Some("w"), None);
        c.harnesses.insert("codex".into(), ninox_core::harness::HarnessSpec {
            enabled: true, worker_args: Some(vec![]), ..Default::default()
        });
        c.worker.harness = "codex".into();
        let r = resolve_dialog_agent(&c, SpawnKind::Standalone, DEFAULT_WORKER_TYPE, Some("codex"), None);
        assert_eq!((r.agent.harness.as_str(), r.agent.model.as_deref()), ("codex", Some("w")));
        let r = resolve_dialog_agent(&c, SpawnKind::Standalone, DEFAULT_WORKER_TYPE, Some("codex"), Some(Some("gpt-5")));
        assert_eq!(r.agent.model.as_deref(), Some("gpt-5"));
    }

    #[test]
    fn dialog_disabled_type_harness_falls_back_to_an_enabled_one() {
        let mut c = cfg_with(Some("w"), None);
        c.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Codexer".into(), harness: "codex".into(), model: Some("gpt-5".into()), ..Default::default()
        });
        let r = resolve_dialog_agent(&c, SpawnKind::Standalone, "Codexer", None, None);
        assert_eq!(r.agent.harness, "claude-code", "codex is disabled by default");
        assert_eq!(r.agent.model.as_deref(), Some("w"));
    }
}

/// One catalogue a Spawn dialog's "Brains this tree reads" offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrainChoice {
    pub name: String,
    pub writable: bool,
    /// Preselected: the dialog's default mirrors `BrainSet::default_for` —
    /// every "Include in search" source, writing to `default`.
    pub selected: bool,
}

/// Every configured catalogue, from names and flags only, so opening a
/// dialog never resolves a brain path.
pub fn brain_choices(config: &ninox_core::config::AppConfig) -> Vec<BrainChoice> {
    config
        .catalogue_names()
        .into_iter()
        .map(|name| {
            let cat = config.brain.catalogues.iter().find(|c| c.name == name && name != "default");
            BrainChoice {
                writable: cat.is_none_or(ninox_core::brain_set::is_writable),
                selected: cat.is_none_or(|c| c.enabled),
                name,
            }
        })
        .collect()
}

/// The "Writes to" options for a selection: its writable members, in order.
pub fn write_options(choices: &[BrainChoice], selected: &[String]) -> Vec<String> {
    choices
        .iter()
        .filter(|c| c.writable && selected.iter().any(|s| s == &c.name))
        .map(|c| c.name.clone())
        .collect()
}

/// `current` if it's still a valid write for `selected`, else the first
/// writable selected brain (empty when there is none).
pub fn reconcile_write(choices: &[BrainChoice], selected: &[String], current: &str) -> String {
    let opts = write_options(choices, selected);
    if opts.iter().any(|o| o == current) {
        current.to_string()
    } else {
        opts.into_iter().next().unwrap_or_default()
    }
}

#[cfg(test)]
mod brain_choice_tests {
    use super::*;

    #[test]
    fn choices_mark_read_only_and_preselect_searchable_sources() {
        let mut cfg = ninox_core::config::AppConfig::default();
        let cat = |name: &str, enabled: bool, access: &str| ninox_core::config::CatalogueRef {
            name: name.into(), path: format!("/scratch/{name}").into(), remote: None, endpoint: None,
            region: None, cache_ttl_secs: None, enabled, cadence: "manual".into(), access: access.into(),
            write_rules_override: None,
        };
        cfg.brain.catalogues = vec![cat("wiki", true, "read_only"), cat("old", false, "read_write")];
        let c = brain_choices(&cfg);
        assert_eq!(c.iter().map(|b| (b.name.as_str(), b.writable, b.selected)).collect::<Vec<_>>(),
            vec![("default", true, true), ("wiki", false, true), ("old", true, false)]);
        let sel = vec!["wiki".to_string(), "old".to_string()];
        assert_eq!(write_options(&c, &sel), vec!["old"]);
        assert_eq!(reconcile_write(&c, &sel, "default"), "old");
        assert_eq!(reconcile_write(&c, &["wiki".to_string()], "old"), "");
    }
}
