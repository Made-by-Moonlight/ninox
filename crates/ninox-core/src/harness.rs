//! Data-driven agent-harness registry — harness definitions are DATA, not
//! code (spec: field-notes-design.md §V "Backend (registry, not enum)").
//! Four known harnesses ship as compiled-in default specs; `[harnesses.*]`
//! config entries override (whole-spec replace) or extend the registry, so
//! adding a future harness requires zero Rust changes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;

/// One harness definition. Template vars in args: `{model}`, `{prompt}`,
/// `{session_id}`.
/// An element containing `{model}` is dropped entirely when no model is
/// set — together with the immediately preceding `-`-prefixed literal flag
/// (so `["--model", "{model}"]` vanishes as a pair).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HarnessSpec {
    /// Offered in the Spawn modal / Workers picker. Off by default for
    /// everything except claude-code (which is forced on — see
    /// [`HarnessRegistry::from_config`]).
    #[serde(default)]
    pub enabled: bool,
    /// Binary to run. Defaults to the harness name.
    #[serde(default)]
    pub binary: Option<String>,
    /// Spec-level default model, used when `AgentConfig.model` is unset.
    #[serde(default)]
    pub model: Option<String>,
    /// Args for interactive sessions (orchestrator / standalone).
    #[serde(default)]
    pub interactive_args: Vec<String>,
    /// Args for one-shot worker spawns. `None` = worker mode unverified for
    /// this harness: selectable interactively, never offered for workers.
    #[serde(default)]
    pub worker_args: Option<Vec<String>>,
    /// Curated fallback for the model picker.
    #[serde(default)]
    pub known_models: Vec<String>,
    /// Optional live model discovery command (argv). Run on demand by the
    /// app, cached per app run, warn-and-fall-through on failure.
    #[serde(default)]
    pub models_cmd: Option<Vec<String>>,
}

/// The effective harness set: builtin defaults overlaid by config entries.
#[derive(Debug, Clone)]
pub struct HarnessRegistry {
    specs: BTreeMap<String, HarnessSpec>,
}

/// Compiled-in default specs. Launch shapes preserve the exact commands the
/// old `AgentConfig::interactive_cmd`/`worker_cmd` built (regression-tested
/// below); the only delta is that substituted values are shell-quoted.
fn builtin_specs() -> BTreeMap<String, HarnessSpec> {
    let mut m = BTreeMap::new();
    m.insert("claude-code".to_string(), HarnessSpec {
        enabled:          true,
        binary:           Some("claude".into()),
        interactive_args: vec!["--session-id".into(), "{session_id}".into(), "--model".into(), "{model}".into()],
        worker_args:      Some(vec![
            "--dangerously-skip-permissions".into(),
            "--session-id".into(), "{session_id}".into(),
            "--model".into(), "{model}".into(),
            "--".into(), "{prompt}".into(),
        ]),
        known_models:     vec![
            "claude-fable-5".into(),
            "claude-opus-4-8".into(),
            "claude-sonnet-5".into(),
            "claude-haiku-4-5".into(),
        ],
        ..HarnessSpec::default()
    });
    m.insert("codex".to_string(), HarnessSpec {
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec!["--model".into(), "{model}".into(), "-p".into(), "{prompt}".into()]),
        ..HarnessSpec::default()
    });
    m.insert("opencode".to_string(), HarnessSpec {
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec!["--model".into(), "{model}".into(), "-p".into(), "{prompt}".into()]),
        models_cmd:       Some(vec!["opencode".into(), "models".into()]),
        ..HarnessSpec::default()
    });
    m.insert("aider".to_string(), HarnessSpec {
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec!["--model".into(), "{model}".into(), "--message".into(), "{prompt}".into()]),
        // aider's `--list-models` needs a search-term argument, so no
        // reliable bare models_cmd ships by default; supply one in config.
        ..HarnessSpec::default()
    });
    m.insert("freebuff".to_string(), HarnessSpec {
        // Researched 2026-07-05: worker/one-shot mode UNVERIFIED — no
        // worker_args until someone supplies them in config.
        interactive_args: vec!["--model".into(), "{model}".into()],
        ..HarnessSpec::default()
    });
    m
}

impl HarnessRegistry {
    /// Builtins overlaid by config `[harnesses.*]` entries (a config entry
    /// replaces the builtin spec wholesale). claude-code is the locked-on
    /// default and cannot be disabled.
    pub fn from_config(overrides: &BTreeMap<String, HarnessSpec>) -> Self {
        let mut specs = builtin_specs();
        for (name, spec) in overrides {
            specs.insert(name.clone(), spec.clone());
        }
        if let Some(cc) = specs.get_mut("claude-code") {
            cc.enabled = true;
        }
        Self { specs }
    }

    /// Effective spec for a harness name. Unknown names run verbatim as the
    /// binary with no args (spec: "unknown harnesses run their name
    /// verbatim as the binary").
    pub fn spec(&self, name: &str) -> HarnessSpec {
        self.specs.get(name).cloned().unwrap_or_else(|| HarnessSpec {
            enabled: true,
            binary:  Some(name.to_string()),
            ..HarnessSpec::default()
        })
    }

    /// All registered harness names: claude-code first, rest alphabetical.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.specs.keys().cloned().collect();
        if let Some(pos) = v.iter().position(|n| n == "claude-code") {
            let cc = v.remove(pos);
            v.insert(0, cc);
        }
        v
    }

    pub fn enabled_names(&self) -> Vec<String> {
        self.names().into_iter().filter(|n| self.specs[n].enabled).collect()
    }

    /// Interactive launch command (orchestrator / standalone sessions).
    /// `claude_session_id` is the UUID ninox generated for this spawn (see
    /// `new_claude_session_id`) — always required so every claude-code
    /// session is resumable from birth.
    pub fn interactive_cmd(&self, agent: &AgentConfig, claude_session_id: &str) -> String {
        let spec   = self.spec(&agent.harness);
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        join_cmd(binary, expand_args(&spec.interactive_args, model, None, claude_session_id))
    }

    /// Worker launch command, or `None` when the spec has no `worker_args`
    /// (worker mode unverified for this harness).
    pub fn worker_cmd(&self, agent: &AgentConfig, prompt: &str, claude_session_id: &str) -> Option<String> {
        let spec   = self.spec(&agent.harness);
        let wargs  = spec.worker_args.as_ref()?;
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        Some(join_cmd(binary, expand_args(wargs, model, Some(prompt), claude_session_id)))
    }
}

fn join_cmd(binary: String, args: Vec<String>) -> String {
    if args.is_empty() { binary } else { format!("{} {}", binary, args.join(" ")) }
}

/// Expand `{model}`/`{prompt}`/`{session_id}` in an arg list. When `model` is
/// `None`, an element containing `{model}` is dropped — along with the
/// immediately preceding `-`-prefixed literal flag, so a `["--model",
/// "{model}"]` pair vanishes cleanly. Substituted elements are shell-quoted;
/// literals pass through verbatim.
fn expand_args(args: &[String], model: Option<&str>, prompt: Option<&str>, claude_session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let flag_for_dropped_model = model.is_none()
            && a.starts_with('-')
            && !a.contains('{')
            && args.get(i + 1).is_some_and(|n| n.contains("{model}"));
        if flag_for_dropped_model {
            i += 2;
            continue;
        }
        if a.contains("{model}") && model.is_none() {
            i += 1;
            continue;
        }
        let had_placeholder = a.contains("{model}") || a.contains("{prompt}") || a.contains("{session_id}");
        let mut s = a.replace("{model}", model.unwrap_or(""));
        if let Some(p) = prompt {
            s = s.replace("{prompt}", p);
        }
        s = s.replace("{session_id}", claude_session_id);
        out.push(if had_placeholder { shell_quote(&s) } else { s });
        i += 1;
    }
    out
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use std::collections::BTreeMap;

    fn reg() -> HarnessRegistry { HarnessRegistry::from_config(&BTreeMap::new()) }

    fn agent(harness: &str, model: Option<&str>) -> AgentConfig {
        AgentConfig { harness: harness.into(), model: model.map(Into::into) }
    }

    // ── Ported launch-shape regression tests (from config.rs) ──────────────
    // Substituted values are now shell-quoted; semantically identical.

    #[test]
    fn interactive_cmd_claude_with_model() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", Some("claude-opus-4-5")), "sess-1"),
                   "claude --session-id 'sess-1' --model 'claude-opus-4-5'");
    }

    #[test]
    fn interactive_cmd_claude_without_model_is_bare() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", None), "sess-1"),
                   "claude --session-id 'sess-1'");
    }

    #[test]
    fn worker_cmd_claude_code() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", None), "Fix the bug", "sess-1").unwrap(),
                   "claude --dangerously-skip-permissions --session-id 'sess-1' -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_claude_code_with_model() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", Some("claude-opus-4-5")), "Fix the bug", "sess-1").unwrap(),
                   "claude --dangerously-skip-permissions --session-id 'sess-1' --model 'claude-opus-4-5' -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_codex() {
        assert_eq!(reg().worker_cmd(&agent("codex", Some("gpt-4o")), "do the thing", "sess-1").unwrap(),
                   "codex --model 'gpt-4o' -p 'do the thing'");
    }

    #[test]
    fn worker_cmd_aider_uses_message_flag() {
        assert_eq!(reg().worker_cmd(&agent("aider", None), "fix it", "sess-1").unwrap(),
                   "aider --message 'fix it'");
    }

    #[test]
    fn worker_cmd_quotes_single_quotes_in_prompt() {
        assert_eq!(reg().worker_cmd(&agent("codex", None), "don't break", "sess-1").unwrap(),
                   "codex -p 'don'\\''t break'");
    }

    // ── Registry semantics ──────────────────────────────────────────────────

    #[test]
    fn unknown_harness_runs_its_name_verbatim() {
        assert_eq!(reg().interactive_cmd(&agent("mytool", None), "sess-1"), "mytool");
        // No worker_args on a synthesized spec → not worker-capable.
        assert!(reg().worker_cmd(&agent("mytool", None), "p", "sess-1").is_none());
    }

    #[test]
    fn freebuff_ships_disabled_without_worker_args() {
        let r = reg();
        let fb = r.spec("freebuff");
        assert!(!fb.enabled);
        assert!(fb.worker_args.is_none());
        assert_eq!(r.interactive_cmd(&agent("freebuff", Some("fb-large")), "sess-1"),
                   "freebuff --model 'fb-large'");
    }

    #[test]
    fn only_claude_code_enabled_by_default() {
        assert_eq!(reg().enabled_names(), vec!["claude-code".to_string()]);
    }

    #[test]
    fn names_lists_claude_code_first_then_alphabetical() {
        assert_eq!(reg().names(),
                   vec!["claude-code", "aider", "codex", "freebuff", "opencode"]
                       .into_iter().map(String::from).collect::<Vec<_>>());
    }

    #[test]
    fn config_entry_replaces_builtin_spec() {
        let mut over = BTreeMap::new();
        over.insert("codex".to_string(), HarnessSpec {
            enabled: true,
            binary:  Some("codex-nightly".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        assert_eq!(r.interactive_cmd(&agent("codex", Some("o3")), "sess-1"), "codex-nightly --model 'o3'");
        assert!(r.enabled_names().contains(&"codex".to_string()));
    }

    #[test]
    fn config_entry_extends_registry_with_new_harness() {
        let mut over = BTreeMap::new();
        over.insert("freebuff2".to_string(), HarnessSpec {
            enabled: true,
            worker_args: Some(vec!["--model".into(), "{model}".into(), "-p".into(), "{prompt}".into()]),
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        // binary defaults to the harness name
        assert_eq!(r.worker_cmd(&agent("freebuff2", None), "go", "sess-1").unwrap(), "freebuff2 -p 'go'");
    }

    #[test]
    fn claude_code_cannot_be_disabled_via_config() {
        let mut over = BTreeMap::new();
        over.insert("claude-code".to_string(),
                    HarnessSpec { enabled: false, ..HarnessSpec::default() });
        let r = HarnessRegistry::from_config(&over);
        assert!(r.enabled_names().contains(&"claude-code".to_string()));
    }

    #[test]
    fn agent_model_overrides_spec_model() {
        let mut over = BTreeMap::new();
        over.insert("codex".to_string(), HarnessSpec {
            model: Some("spec-default".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        assert_eq!(r.interactive_cmd(&agent("codex", Some("chosen")), "sess-1"), "codex --model 'chosen'");
        assert_eq!(r.interactive_cmd(&agent("codex", None), "sess-1"),           "codex --model 'spec-default'");
    }

    #[test]
    fn harness_spec_round_trips_through_toml() {
        let toml_src = r#"
            [harnesses.freebuff]
            enabled = true
            binary  = "freebuff"
            model   = "fb-large"
            interactive_args = ["--model", "{model}"]
            worker_args      = ["--model", "{model}", "-p", "{prompt}"]
            known_models     = ["fb-large", "fb-mini"]
            models_cmd       = ["freebuff", "models"]
        "#;
        #[derive(serde::Deserialize)]
        struct Wrap { harnesses: BTreeMap<String, HarnessSpec> }
        let w: Wrap = toml::from_str(toml_src).unwrap();
        let fb = &w.harnesses["freebuff"];
        assert!(fb.enabled);
        assert_eq!(fb.known_models, vec!["fb-large", "fb-mini"]);
        assert_eq!(fb.models_cmd.as_deref(), Some(&["freebuff".to_string(), "models".to_string()][..]));
    }
}
