# Settings Panel ("The Appendix") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the approved "V. Settings — the appendix" design: a data-driven harness registry in ninox-core, a Settings view (theme card, harness toggles, worker default), dynamic harness/model pickers in the Spawn modal, and a per-session Re-file action.

**Architecture:** Harness definitions become DATA (`HarnessSpec` registry in a new `ninox-core/src/harness.rs`): compiled-in default specs for claude-code/codex/opencode/aider/freebuff, overridden or extended by `[harnesses.<name>]` config entries; all launch-command construction resolves through the registry (replacing `AgentConfig::interactive_cmd`/`worker_cmd`). The app gains a `View::Settings` reached from the sidebar footer, model discovery via `models_cmd` (cached per app run), and `Message::RefileSession` which kills + respawns a session with current registry settings.

**Tech Stack:** Rust, iced 0.13 (Elm-style update/view), serde + toml, rusqlite store, tokio, tmux.

## Global Constraints

- Spec source of truth: `docs/design-concepts/field-notes-design.md` §"V. Settings — the appendix" (lines 189–253). Copy quoted rules verbatim from there when in doubt.
- "adding a future harness must require zero Rust changes" — no `match` on harness names anywhere outside builtin spec data.
- "`claude-code` is the locked-on DEFAULT" — the registry forces `claude-code.enabled = true` regardless of config; its Settings toggle is inert.
- "NO model field [in the Harnesses card]: interactive spawns always choose their model in the Spawn modal".
- "Model pickers — here and in the Spawn modal — are selects, not free text", precedence: (1) `models_cmd` output (run on demand, cached per app run, warn-and-fall-through), (2) spec's `known_models`, (3) the currently-configured value. "The LAST entry is always `custom…`".
- "Serde-defaulted throughout — existing configs keep parsing unchanged."
- "exact current launch shapes preserved" for the four known harnesses (see ported cmd tests in Task 1; the only allowed delta is shell-quoting substituted values, which is semantically identical).
- claude-code ships `known_models` = fable-5 / opus-4.8 / sonnet-5 / haiku-4.5 (ids: `claude-fable-5`, `claude-opus-4-8`, `claude-sonnet-5`, `claude-haiku-4-5`).
- freebuff ships as a compiled-in default spec, **disabled**, `interactive_args = ["--model", "{model}"]`, **no** `worker_args` (worker mode unverified). A harness without `worker_args` is selectable for interactive spawns but never offered for the Workers card / CLI worker path.
- Template vars: `{model}`, `{prompt}`. "an arg element containing `{model}` is dropped entirely when no model is set" — and the immediately preceding `-`-prefixed literal flag is dropped with it (else `--model` would dangle).
- Repo style: hand-aligned code, NO rustfmt run (no rustfmt.toml on purpose). Match neighboring comment density and alignment.
- Every task gates on: `cargo test --workspace` green and `cargo clippy --workspace --all-targets -- -D warnings` clean.
- Any test that calls `config.save()` or `AppConfig::load()` MUST redirect via `with_env_override("NINOX_CONFIG", ...)` (helpers exist: `crates/ninox-core/src/config.rs:367` and `crates/ninox-app/src/app.rs:2994`, both guarded by an `ENV_TEST_GUARD` mutex).
- Conventional commits, one commit per task minimum: `feat(core): ...` for ninox-core, `feat(native-app): ...` for ninox-app.

---

### Task 1: Harness registry in ninox-core

**Files:**
- Create: `crates/ninox-core/src/harness.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod harness;`)
- Modify: `crates/ninox-core/src/config.rs` (add `harnesses` field + `registry()`)
- Test: inline `#[cfg(test)] mod tests` in `harness.rs`

**Interfaces:**
- Consumes: `crate::config::AgentConfig` (existing: `{ harness: String, model: Option<String> }`).
- Produces (later tasks rely on these exact signatures):
  - `pub struct HarnessSpec { pub enabled: bool, pub binary: Option<String>, pub model: Option<String>, pub interactive_args: Vec<String>, pub worker_args: Option<Vec<String>>, pub known_models: Vec<String>, pub models_cmd: Option<Vec<String>> }` (all serde-defaulted, `Clone + PartialEq + Debug`)
  - `pub struct HarnessRegistry` with:
    - `pub fn from_config(overrides: &BTreeMap<String, HarnessSpec>) -> Self`
    - `pub fn spec(&self, name: &str) -> HarnessSpec` (unknown name → synthesized verbatim spec: `binary = Some(name)`, empty args)
    - `pub fn names(&self) -> Vec<String>` (claude-code first, rest alphabetical)
    - `pub fn enabled_names(&self) -> Vec<String>` (same order, enabled only)
    - `pub fn interactive_cmd(&self, agent: &AgentConfig) -> String`
    - `pub fn worker_cmd(&self, agent: &AgentConfig, prompt: &str) -> Option<String>` (None ⇔ spec has no `worker_args`)
  - `AppConfig.harnesses: BTreeMap<String, HarnessSpec>` (serde default, placed LAST in the struct so TOML tables serialize after scalars) and `AppConfig::registry(&self) -> HarnessRegistry`.

- [ ] **Step 1: Write the failing tests** (in `harness.rs` under the implementation stubs — or write the whole file test-first and let it fail to compile, the repo's usual TDD shape)

```rust
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
        assert_eq!(reg().interactive_cmd(&agent("claude-code", Some("claude-opus-4-5"))),
                   "claude --model 'claude-opus-4-5'");
    }

    #[test]
    fn interactive_cmd_claude_without_model_is_bare() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", None)), "claude");
    }

    #[test]
    fn worker_cmd_claude_code() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", None), "Fix the bug").unwrap(),
                   "claude --dangerously-skip-permissions -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_claude_code_with_model() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", Some("claude-opus-4-5")), "Fix the bug").unwrap(),
                   "claude --dangerously-skip-permissions --model 'claude-opus-4-5' -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_codex() {
        assert_eq!(reg().worker_cmd(&agent("codex", Some("gpt-4o")), "do the thing").unwrap(),
                   "codex --model 'gpt-4o' -p 'do the thing'");
    }

    #[test]
    fn worker_cmd_aider_uses_message_flag() {
        assert_eq!(reg().worker_cmd(&agent("aider", None), "fix it").unwrap(),
                   "aider --message 'fix it'");
    }

    #[test]
    fn worker_cmd_quotes_single_quotes_in_prompt() {
        assert_eq!(reg().worker_cmd(&agent("codex", None), "don't break").unwrap(),
                   "codex -p 'don'\\''t break'");
    }

    // ── Registry semantics ──────────────────────────────────────────────────

    #[test]
    fn unknown_harness_runs_its_name_verbatim() {
        assert_eq!(reg().interactive_cmd(&agent("mytool", None)), "mytool");
        // No worker_args on a synthesized spec → not worker-capable.
        assert!(reg().worker_cmd(&agent("mytool", None), "p").is_none());
    }

    #[test]
    fn freebuff_ships_disabled_without_worker_args() {
        let r = reg();
        let fb = r.spec("freebuff");
        assert!(!fb.enabled);
        assert!(fb.worker_args.is_none());
        assert_eq!(r.interactive_cmd(&agent("freebuff", Some("fb-large"))),
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
        assert_eq!(r.interactive_cmd(&agent("codex", Some("o3"))), "codex-nightly --model 'o3'");
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
        assert_eq!(r.worker_cmd(&agent("freebuff2", None), "go").unwrap(), "freebuff2 -p 'go'");
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
        assert_eq!(r.interactive_cmd(&agent("codex", Some("chosen"))), "codex --model 'chosen'");
        assert_eq!(r.interactive_cmd(&agent("codex", None)),           "codex --model 'spec-default'");
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core harness 2>&1 | tail -5`
Expected: compile error (module/types don't exist yet).

- [ ] **Step 3: Write the implementation** (`crates/ninox-core/src/harness.rs`)

```rust
//! Data-driven agent-harness registry — harness definitions are DATA, not
//! code (spec: field-notes-design.md §V "Backend (registry, not enum)").
//! Four known harnesses ship as compiled-in default specs; `[harnesses.*]`
//! config entries override (whole-spec replace) or extend the registry, so
//! adding a future harness requires zero Rust changes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;

/// One harness definition. Template vars in args: `{model}`, `{prompt}`.
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
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec![
            "--dangerously-skip-permissions".into(),
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
    pub fn interactive_cmd(&self, agent: &AgentConfig) -> String {
        let spec   = self.spec(&agent.harness);
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        join_cmd(binary, expand_args(&spec.interactive_args, model, None))
    }

    /// Worker launch command, or `None` when the spec has no `worker_args`
    /// (worker mode unverified for this harness).
    pub fn worker_cmd(&self, agent: &AgentConfig, prompt: &str) -> Option<String> {
        let spec   = self.spec(&agent.harness);
        let wargs  = spec.worker_args.as_ref()?;
        let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
        let model  = agent.model.as_deref().or(spec.model.as_deref());
        Some(join_cmd(binary, expand_args(wargs, model, Some(prompt))))
    }
}

fn join_cmd(binary: String, args: Vec<String>) -> String {
    if args.is_empty() { binary } else { format!("{} {}", binary, args.join(" ")) }
}

/// Expand `{model}`/`{prompt}` in an arg list. When `model` is `None`, an
/// element containing `{model}` is dropped — along with the immediately
/// preceding `-`-prefixed literal flag, so a `["--model", "{model}"]` pair
/// vanishes cleanly. Substituted elements are shell-quoted; literals pass
/// through verbatim.
fn expand_args(args: &[String], model: Option<&str>, prompt: Option<&str>) -> Vec<String> {
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
        let had_placeholder = a.contains("{model}") || a.contains("{prompt}");
        let mut s = a.replace("{model}", model.unwrap_or(""));
        if let Some(p) = prompt {
            s = s.replace("{prompt}", p);
        }
        out.push(if had_placeholder { shell_quote(&s) } else { s });
        i += 1;
    }
    out
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
```

- [ ] **Step 4: Wire into lib.rs and AppConfig**

In `crates/ninox-core/src/lib.rs` add `pub mod harness;` next to the other module declarations.

In `crates/ninox-core/src/config.rs`:
- Add `use std::collections::BTreeMap;` and `use crate::harness::{HarnessRegistry, HarnessSpec};`
- Add as the LAST field of `AppConfig` (tables must serialize after scalars):
```rust
    /// Agent-harness registry overrides/extensions (`[harnesses.<name>]`).
    /// Builtin specs for claude-code/codex/opencode/aider/freebuff apply
    /// when a name is absent here. See `crate::harness`.
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessSpec>,
```
- Add `harnesses: BTreeMap::new(),` to `impl Default for AppConfig`.
- Add to `impl AppConfig`:
```rust
    /// The effective harness registry: builtin specs overlaid by this
    /// config's `[harnesses.*]` entries.
    pub fn registry(&self) -> HarnessRegistry {
        HarnessRegistry::from_config(&self.harnesses)
    }
```

- [ ] **Step 5: Run tests, verify pass**

Run: `cargo test -p ninox-core 2>&1 | tail -5`
Expected: all green (old `AgentConfig` cmd tests still present and passing — they're removed in Task 2).

- [ ] **Step 6: Clippy + commit**

Run: `cargo clippy -p ninox-core --all-targets -- -D warnings`
```bash
git add crates/ninox-core/src/harness.rs crates/ninox-core/src/lib.rs crates/ninox-core/src/config.rs
git commit -m "feat(core): data-driven harness registry with compiled-in default specs"
```

---

### Task 2: Resolve all launch commands through the registry

**Files:**
- Modify: `crates/ninox-core/src/config.rs` (delete `AgentConfig::interactive_cmd`, `worker_cmd`, `harness_binary`, `shell_quote` + their tests at `config.rs:333-360`)
- Modify: `crates/ninox-app/src/spawn_util.rs` (params gain `base_cmd`)
- Modify: `crates/ninox-app/src/app.rs` (`SpawnFormConfirm` computes `base_cmd`)
- Modify: `crates/ninox-app/src/main.rs` (`run_spawn` resolves via registry; worker-incapable harness is a hard error)

**Interfaces:**
- Consumes: `AppConfig::registry()`, `HarnessRegistry::interactive_cmd/worker_cmd` (Task 1).
- Produces: `InteractiveSpawnParams` gains `pub base_cmd: String` (the resolved interactive launch command; `spawn_util` no longer resolves commands itself). `run_spawn(store, config: AppConfig, prompt, workspace, name, orchestrator_id)` (was `agent: AgentConfig`).

- [ ] **Step 1: spawn_util** — in `InteractiveSpawnParams` add after `agent`:
```rust
    /// Resolved interactive launch command (registry-resolved by the
    /// caller: `AppConfig::registry().interactive_cmd(&agent)`). Kept
    /// separate from `agent` so this module needs no registry access;
    /// `agent` remains for session-record stamping (agent_type/model).
    pub base_cmd:        String,
```
Replace `let base_cmd = p.agent.interactive_cmd();` (spawn_util.rs:75) with `let base_cmd = p.base_cmd;`. Update the two `#[ignore]` probes and any test constructing `InteractiveSpawnParams` to pass `base_cmd: "claude".into()` (or resolve via a default registry: `ninox_core::config::AppConfig::default().registry().interactive_cmd(&AgentConfig::default())`).

- [ ] **Step 2: app.rs SpawnFormConfirm** — right after `agent` is built (~app.rs:783), add:
```rust
                let base_cmd = state.config.registry().interactive_cmd(&agent);
```
and pass `base_cmd: base_cmd.clone()` (move into each async block alongside `agent`) in BOTH `InteractiveSpawnParams` literals (standalone ~:903, orchestrator ~:1015).

- [ ] **Step 3: main.rs run_spawn** — change signature to take `config: AppConfig` (call site `main.rs:112-114` becomes `run_spawn(store, config, prompt, workspace, name, orchestrator_id).await`). Replace `let cmd_base = agent.worker_cmd(&effective_prompt);` (main.rs:215) with:
```rust
    let agent = config.worker.clone();
    let registry = config.registry();
    let Some(cmd_base) = registry.worker_cmd(&agent, &effective_prompt) else {
        anyhow::bail!(
            "harness '{}' has no verified worker mode (no worker_args in its spec) — \
             pick a worker-capable harness in Settings or add worker_args under \
             [harnesses.{}] in config.toml",
            agent.harness, agent.harness,
        );
    };
```
(keep the existing `agent.harness.clone()` / `agent.model.clone()` session stamping).

- [ ] **Step 4: Delete the old resolution code** — remove `AgentConfig::interactive_cmd`/`worker_cmd` impl block, `harness_binary`, `shell_quote` from `config.rs`, plus tests `interactive_cmd_with_model`, `worker_cmd_codex`, `worker_cmd_claude_code`, `worker_cmd_claude_code_with_model` (ported to harness.rs in Task 1). Grep to prove no stragglers:

Run: `grep -rn "interactive_cmd\|worker_cmd\|harness_binary" crates/ --include='*.rs' | grep -v harness.rs | grep -v base_cmd`
Expected: only the registry call sites added above.

- [ ] **Step 5: Full test + clippy + commit**

Run: `cargo test --workspace 2>&1 | tail -5` and `cargo clippy --workspace --all-targets -- -D warnings`
```bash
git add -A crates/
git commit -m "feat(core): resolve all agent launch commands through the harness registry"
```

---

### Task 3: Model options + `models_cmd` discovery

**Files:**
- Create: `crates/ninox-app/src/models.rs` (+ `mod models;` in `main.rs`)
- Modify: `crates/ninox-app/src/app.rs` (cache field, message, trigger helper)
- Test: inline in `models.rs`, update-level in `app.rs`

**Interfaces:**
- Produces:
  - `pub const CUSTOM_SENTINEL: &str = "custom…";`
  - `pub fn model_options(spec: &HarnessSpec, discovered: Option<&[String]>, configured: Option<&str>) -> Vec<String>` — precedence per spec; always ends with `CUSTOM_SENTINEL`.
  - `pub fn parse_models_output(stdout: &str) -> Vec<String>` (trimmed non-empty lines)
  - `pub async fn run_models_cmd(cmd: Vec<String>) -> Option<Vec<String>>` (warn-and-fall-through: `None` on spawn failure / non-zero exit / empty output)
  - `App.model_lists: HashMap<String, Option<Vec<String>>>` (key: harness name; `Some(None)` entry = attempted & failed → fall through to known_models)
  - `Message::ModelListLoaded { harness: String, models: Option<Vec<String>> }`
  - `App::ensure_models(state: &App, harness: &str) -> Task<Message>` (no-op when cached or no `models_cmd`)

- [ ] **Step 1: failing tests** (`models.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::harness::HarnessSpec;

    fn spec(known: &[&str]) -> HarnessSpec {
        HarnessSpec { known_models: known.iter().map(|s| s.to_string()).collect(),
                      ..HarnessSpec::default() }
    }

    #[test]
    fn discovered_models_win_over_known() {
        let d = vec!["live-1".to_string(), "live-2".to_string()];
        assert_eq!(model_options(&spec(&["k-1"]), Some(&d), None),
                   vec!["live-1", "live-2", CUSTOM_SENTINEL]);
    }

    #[test]
    fn empty_discovery_falls_through_to_known_models() {
        assert_eq!(model_options(&spec(&["k-1", "k-2"]), Some(&[]), None),
                   vec!["k-1", "k-2", CUSTOM_SENTINEL]);
        assert_eq!(model_options(&spec(&["k-1"]), None, None),
                   vec!["k-1", CUSTOM_SENTINEL]);
    }

    #[test]
    fn configured_value_is_always_present() {
        assert_eq!(model_options(&spec(&["k-1"]), None, Some("mine")),
                   vec!["k-1", "mine", CUSTOM_SENTINEL]);
        // ...but not duplicated when already listed
        assert_eq!(model_options(&spec(&["k-1"]), None, Some("k-1")),
                   vec!["k-1", CUSTOM_SENTINEL]);
    }

    #[test]
    fn custom_is_always_last_even_with_nothing_else() {
        assert_eq!(model_options(&spec(&[]), None, None), vec![CUSTOM_SENTINEL]);
    }

    #[test]
    fn parse_models_output_takes_trimmed_nonempty_lines() {
        assert_eq!(parse_models_output("  a-model \n\nb-model\n"),
                   vec!["a-model", "b-model"]);
    }
}
```

- [ ] **Step 2: run, verify fail** — `cargo test -p ninox-app models 2>&1 | tail -3` → compile error.

- [ ] **Step 3: implement `models.rs`**

```rust
//! Model-picker options + live `models_cmd` discovery (spec §V "Model
//! pickers ... are selects, not free text", fed in precedence order:
//! discovery → known_models → configured value, then `custom…` last).

use ninox_core::harness::HarnessSpec;

/// The picker's escape hatch — selecting it reveals a mono free-text input.
pub const CUSTOM_SENTINEL: &str = "custom…";

pub fn model_options(
    spec:       &HarnessSpec,
    discovered: Option<&[String]>,
    configured: Option<&str>,
) -> Vec<String> {
    let mut opts: Vec<String> = match discovered {
        Some(d) if !d.is_empty() => d.to_vec(),
        _                        => spec.known_models.clone(),
    };
    if let Some(c) = configured {
        if !c.is_empty() && !opts.iter().any(|o| o == c) {
            opts.push(c.to_string());
        }
    }
    opts.push(CUSTOM_SENTINEL.to_string());
    opts
}

pub fn parse_models_output(stdout: &str) -> Vec<String> {
    stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Run a spec's `models_cmd` argv. Warn-and-fall-through: any failure
/// (missing binary, non-zero exit, empty output) returns `None` and the
/// picker falls back to `known_models`.
pub async fn run_models_cmd(cmd: Vec<String>) -> Option<Vec<String>> {
    let (bin, args) = cmd.split_first()?;
    let out = match tokio::process::Command::new(bin).args(args).output().await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("models_cmd {bin} failed to launch: {e}");
            return None;
        }
    };
    if !out.status.success() {
        tracing::warn!("models_cmd {bin} exited with {}", out.status);
        return None;
    }
    let models = parse_models_output(&String::from_utf8_lossy(&out.stdout));
    (!models.is_empty()).then_some(models)
}
```

- [ ] **Step 4: app wiring** — `main.rs`: `mod models;`. `app.rs`:
  - Field on `App` (near `spawn_modal`): `pub model_lists: std::collections::HashMap<String, Option<Vec<String>>>,` — init `HashMap::new()` in `App::new` AND in the test constructors `base`/`base_with_brain` (~app.rs:1982).
  - Message variant: `ModelListLoaded { harness: String, models: Option<Vec<String>> },` — handler:
```rust
            Message::ModelListLoaded { harness, models } => {
                state.model_lists.insert(harness, models);
                Task::none()
            }
```
  - Helper on `impl App` (private, near other helpers):
```rust
    /// Kick off `models_cmd` discovery for a harness (cached per app run —
    /// including failures, which fall through to known_models).
    fn ensure_models(state: &App, harness: &str) -> Task<Message> {
        if state.model_lists.contains_key(harness) {
            return Task::none();
        }
        let Some(cmd) = state.config.registry().spec(harness).models_cmd else {
            return Task::none();
        };
        let h = harness.to_string();
        Task::future(async move {
            let models = crate::models::run_models_cmd(cmd).await;
            Message::ModelListLoaded { harness: h, models }
        })
    }
```
  - Update-level test: `ModelListLoaded` populates the cache; a spec-less harness makes `ensure_models` return without a task (assert `state.model_lists` untouched after driving the message path — construct via `m.update(...)`).

- [ ] **Step 5: test + clippy + commit**

```bash
git add crates/ninox-app/src/models.rs crates/ninox-app/src/main.rs crates/ninox-app/src/app.rs
git commit -m "feat(native-app): model-picker options with cached models_cmd discovery"
```

---

### Task 4: Spawn modal — dynamic harness chips + model picker

**Files:**
- Modify: `crates/ninox-app/src/components/spawn_modal.rs` (delete `AGENT_PRESETS`/`AgentPreset`; rework form + agent field)
- Modify: `crates/ninox-app/src/app.rs` (messages, `SpawnSession` preselect, `SpawnFormConfirm`, remembered preselection persistence, tests)

**Interfaces:**
- Consumes: `HarnessRegistry` (Task 1), `model_options`/`CUSTOM_SENTINEL`/`ensure_models` (Task 3).
- Produces:
  - `SpawnForm { kind, name, workspace, harness: String, model: Option<String>, custom_model: Option<String>, catalogue_idx, error }` with a manual `Default` (`harness: "claude-code".into()`).
  - Messages: `SpawnFormHarness(String)` (replaces `SpawnFormAgent(usize)`), `SpawnFormModel(String)`, `SpawnFormCustomModel(String)`.
  - `pub fn effective_model(form: &SpawnForm, spec: &HarnessSpec) -> Option<String>` — custom text (trimmed, non-empty) → that; else `form.model`; else `spec.model`.
  - `pub fn static_prior(harness: &str, model: Option<&str>) -> Option<(f64, f64)>` and reworked `pub fn estimate_text(prior: Option<(f64, f64)>, historical_costs: &[f64]) -> String`.

- [ ] **Step 1: failing tests** — replace the `estimate_text` test block in `spawn_modal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_priors_cover_claude_models_only() {
        assert_eq!(static_prior("claude-code", Some("claude-fable-5")),   Some((4.0, 8.0)));
        assert_eq!(static_prior("claude-code", Some("claude-opus-4-8")),  Some((2.0, 4.0)));
        assert_eq!(static_prior("claude-code", Some("claude-sonnet-5")),  Some((1.0, 2.0)));
        assert_eq!(static_prior("claude-code", Some("claude-haiku-4-5")), Some((0.4, 1.2)));
        assert_eq!(static_prior("claude-code", None), None);
        assert_eq!(static_prior("codex", Some("gpt-4o")), None);
    }

    #[test]
    fn estimate_text_falls_back_to_static_range_when_no_history() {
        assert_eq!(estimate_text(Some((2.0, 4.0)), &[]), "est. $2–4 / session");
    }

    #[test]
    fn estimate_text_falls_back_below_min_sample_count() {
        assert_eq!(estimate_text(Some((2.0, 4.0)), &[1.0, 3.0]), "est. $2–4 / session");
    }

    #[test]
    fn estimate_text_uses_historical_average_at_min_sample_count() {
        assert_eq!(estimate_text(Some((4.0, 8.0)), &[3.0, 5.0, 4.0]),
                   "≈ $4.00 / session · from 3 filed");
        // History wins even with no prior (e.g. a custom harness).
        assert_eq!(estimate_text(None, &[3.0, 5.0, 4.0]),
                   "≈ $4.00 / session · from 3 filed");
    }

    #[test]
    fn estimate_text_without_prior_or_history_says_so() {
        assert_eq!(estimate_text(None, &[]), "est. — builds from filed sessions");
    }

    fn form() -> SpawnForm { SpawnForm::default() }

    #[test]
    fn effective_model_prefers_custom_text() {
        let spec = ninox_core::harness::HarnessSpec { model: Some("spec-m".into()), ..Default::default() };
        let mut f = form();
        f.model = Some("picked".into());
        f.custom_model = Some("  typed  ".into());
        assert_eq!(effective_model(&f, &spec).as_deref(), Some("typed"));
        f.custom_model = Some("   ".into());          // blank custom → picker value
        assert_eq!(effective_model(&f, &spec).as_deref(), Some("picked"));
        f.model = None;                                // nothing picked → spec default
        assert_eq!(effective_model(&f, &spec).as_deref(), Some("spec-m"));
    }

    #[test]
    fn default_form_selects_claude_code() {
        assert_eq!(form().harness, "claude-code");
    }
}
```

- [ ] **Step 2: run, verify fail** — `cargo test -p ninox-app spawn_modal 2>&1 | tail -3`.

- [ ] **Step 3: implement form + pure fns** in `spawn_modal.rs`:
  - Delete `AgentPreset` and `AGENT_PRESETS`.
  - `SpawnForm`: replace `agent_idx: usize` with `harness: String`, `model: Option<String>`, `custom_model: Option<String>`; drop `#[derive(Default)]`, add:
```rust
impl Default for SpawnForm {
    fn default() -> Self {
        Self {
            kind:          SpawnKind::default(),
            name:          String::new(),
            workspace:     String::new(),
            harness:       "claude-code".to_string(),
            model:         None,
            custom_model:  None,
            catalogue_idx: 0,
            error:         None,
        }
    }
}
```
  - Pure fns:
```rust
/// Rough static per-session priors for claude-code's curated models —
/// relative pricing only, not wired to any pricing API. Everything else
/// has no prior and builds its estimate from filed history.
pub fn static_prior(harness: &str, model: Option<&str>) -> Option<(f64, f64)> {
    if harness != "claude-code" {
        return None;
    }
    match model {
        Some("claude-fable-5")   => Some((4.0, 8.0)),
        Some("claude-opus-4-8")  => Some((2.0, 4.0)),
        Some("claude-sonnet-5")  => Some((1.0, 2.0)),
        Some("claude-haiku-4-5") => Some((0.4, 1.2)),
        _                        => None,
    }
}

pub fn estimate_text(prior: Option<(f64, f64)>, historical_costs: &[f64]) -> String {
    if historical_costs.len() >= MIN_HISTORY_SAMPLES {
        let avg = historical_costs.iter().sum::<f64>() / historical_costs.len() as f64;
        format!("≈ ${avg:.2} / session · from {} filed", historical_costs.len())
    } else if let Some((lo, hi)) = prior {
        format!("est. ${lo:.0}–{hi:.0} / session")
    } else {
        "est. — builds from filed sessions".to_string()
    }
}

pub fn effective_model(form: &SpawnForm, spec: &ninox_core::harness::HarnessSpec) -> Option<String> {
    if let Some(c) = &form.custom_model {
        let t = c.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    form.model.clone().or_else(|| spec.model.clone())
}
```
  - Rework the `agent_field` render block (replacing spawn_modal.rs:233-243): two labeled columns in one row — "Agent" chips (one `chip(...)` per `registry.enabled_names()`, selected = `name == form.harness`, `Message::SpawnFormHarness(name)`), and "Model" as a `pick_list` over `crate::models::model_options(&spec, discovered, configured)` styled with the existing `pick_style(s)`:
```rust
    let registry = state.config.registry();
    let spec = registry.spec(&form.harness);
    let agent_chips = row(registry.enabled_names().into_iter().map(|name| {
        let selected = name == form.harness;
        chip(s, name.clone(), selected, Message::SpawnFormHarness(name))
    }))
    .spacing(8);

    let discovered = state.model_lists.get(&form.harness).and_then(|m| m.as_deref());
    let configured = form.model.clone().or_else(|| spec.model.clone());
    let options = crate::models::model_options(&spec, discovered, configured.as_deref());
    let picker_selected = if form.custom_model.is_some() {
        Some(crate::models::CUSTOM_SENTINEL.to_string())
    } else {
        configured
    };
    let model_picker = pick_list(options, picker_selected, Message::SpawnFormModel)
        .placeholder("harness default")
        .font(MONO)
        .text_size(12)
        .padding([6, 10])
        .style(pick_style(s));

    let mut agent_col = column![micro_label("Agent", s.ink_2), Space::new(0, 8), agent_chips].spacing(0);
    let mut model_col = column![micro_label("Model", s.ink_2), Space::new(0, 8), model_picker].spacing(0);
    if form.custom_model.is_some() {
        model_col = model_col.push(Space::new(0, 6)).push(
            text_input("model id", form.custom_model.as_deref().unwrap_or(""))
                .on_input(Message::SpawnFormCustomModel)
                .font(MONO)
                .size(12)
                .padding([4, 2])
                .style(style::underlined_input_style(s)),
        );
    }
    let agent_field = row![agent_col, Space::new(16, 0), model_col].align_y(Alignment::Start);
```
  - Footer estimate wiring (replacing :282-286):
```rust
    let est_model = effective_model(form, &spec);
    let historical_costs = state.engine.store
        .cost_samples(&form.harness, est_model.as_deref())
        .unwrap_or_default();
    let estimate = estimate_text(static_prior(&form.harness, est_model.as_deref()), &historical_costs);
```

- [ ] **Step 4: app.rs message plumbing**
  - Replace `SpawnFormAgent(usize)` variant with `SpawnFormHarness(String)`; add `SpawnFormModel(String)`, `SpawnFormCustomModel(String)`. Handlers:
```rust
            Message::SpawnFormHarness(h) => {
                let task = Self::ensure_models(state, &h);
                if let Some(f) = &mut state.spawn_modal {
                    f.harness = h;
                    f.model = None;
                    f.custom_model = None;
                    f.error = None;
                }
                task
            }

            Message::SpawnFormModel(v) => {
                if let Some(f) = &mut state.spawn_modal {
                    if v == crate::models::CUSTOM_SENTINEL {
                        f.custom_model = Some(String::new());
                    } else {
                        f.custom_model = None;
                        f.model = Some(v);
                    }
                    f.error = None;
                }
                Task::none()
            }

            Message::SpawnFormCustomModel(v) => {
                if let Some(f) = &mut state.spawn_modal { f.custom_model = Some(v); f.error = None; }
                Task::none()
            }
```
  - `Message::SpawnSession` handler (app.rs:627-640) — preselect the remembered `[orchestrator]` agent when its harness is enabled, else the first enabled harness; kick discovery:
```rust
            Message::SpawnSession => {
                let enabled = state.config.registry().enabled_names();
                let (harness, model) = if enabled.contains(&state.orchestrator_agent.harness) {
                    (state.orchestrator_agent.harness.clone(), state.orchestrator_agent.model.clone())
                } else {
                    (enabled.first().cloned().unwrap_or_else(|| "claude-code".into()), None)
                };
                let task = Self::ensure_models(state, &harness);
                state.spawn_modal = Some(SpawnForm { harness, model, ..SpawnForm::default() });
                task
            }
```
  - `Message::SpawnFormConfirm` (app.rs:767+): replace the preset lookup (:768, :782-786) with:
```rust
                let registry = state.config.registry();
                let spec = registry.spec(&form.harness);
                let agent = ninox_core::config::AgentConfig {
                    harness: form.harness.clone(),
                    model:   crate::components::spawn_modal::effective_model(&form, &spec),
                };
```
    and after the name guard passes (both kinds spawn), persist the remembered preselection — insert right before the `match form.kind`:
```rust
                // "[orchestrator] stays only as the Spawn modal's remembered
                // preselection" — remember the last confirmed agent choice.
                state.orchestrator_agent = agent.clone();
                state.config.orchestrator = agent.clone();
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save remembered agent preselection: {e}");
                }
```
    NOTE: this runs after the early-return guards (empty name / duplicate id / missing workspace), so a refused confirm does not save. It writes config → the update-level tests covering `SpawnFormConfirm` (app.rs:2074+, :3021, :3158) must run under `with_env_override("NINOX_CONFIG", ...)`; extend the existing ones that don't.
  - Fix the two existing preset-based tests (app.rs:634-637 handler + any test referencing `AGENT_PRESETS`) and add: confirm persists `[orchestrator]` (load `AppConfig` from the overridden path and assert `orchestrator.harness/model` match the form).

- [ ] **Step 5: full test + clippy + commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): spawn modal picks harness and model from the registry"
```

---

### Task 5: Settings view skeleton + Theme card + sidebar footer

**Files:**
- Create: `crates/ninox-app/src/components/settings_panel.rs` (+ entry in `components/mod.rs`)
- Modify: `crates/ninox-app/src/app.rs` (`View::Settings`, `NavigateSettings`, view dispatch)
- Modify: `crates/ninox-app/src/components/sidebar.rs` (footer becomes `Settings ▸`; theme dots move out)

**Interfaces:**
- Consumes: `folio_scaffold` (`components/folio.rs:52`), `card_style`/`micro_label`/`hline`/`MONO`/`SERIF`/`SERIF_ITALIC` (style.rs), `Message::SwitchTheme` (existing, already persists config at app.rs:1193-1204).
- Produces: `View::Settings` variant; `Message::NavigateSettings`; `pub fn settings_panel(app: &App) -> Element<Message>`; `SettingsState { pub worker_custom: Option<String> }` + `App.settings: SettingsState` (used by Task 7).

- [ ] **Step 1: failing test** (app.rs tests):
```rust
    #[test]
    fn navigate_settings_switches_view() {
        let m = base(test_engine());
        let (m, _) = m.update(Message::NavigateSettings);
        assert!(matches!(m.view, View::Settings));
        // and back out via the TOC
        let (m, _) = m.update(Message::NavigateFleet { scope: None });
        assert!(matches!(m.view, View::FleetBoard { .. }));
    }
```

- [ ] **Step 2: run, verify fail** — `cargo test -p ninox-app navigate_settings 2>&1 | tail -3`.

- [ ] **Step 3: app plumbing** — add `Settings` to `View` (app.rs:83-88); `NavigateSettings` message + handler (`state.view = View::Settings; Task::none()`); dispatch arm in `iced_view` (:1702-1707): `View::Settings => settings_panel(state),` (import alongside the other component fns at app.rs:15). Add `pub settings: crate::components::settings_panel::SettingsState,` to `App`, init `SettingsState::default()` in `App::new` and the test constructors `base`/`base_with_brain`.

- [ ] **Step 4: settings_panel.rs** — skeleton + Theme card:

```rust
//! Settings — "The appendix" (spec §V): a single narrow column of cards
//! reached from the sidebar footer. Theme dots live here (relocated from
//! the footer); harness registry toggles and the worker default follow in
//! their own cards.

use ninox_core::config::ThemeVariant;
use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Element, Length,
};

use crate::{
    app::{App, Message},
    components::folio::folio_scaffold,
    style::{self, card_style, hline, micro_label, MONO, SERIF, SERIF_ITALIC},
};

/// Settings-view UI state (custom-model input text for the Workers card).
#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    /// `Some` while the Workers model picker is in `custom…` mode.
    pub worker_custom: Option<String>,
}

/// Column width — "a single narrow column (~720px) of cards".
const COLUMN_W: f32 = 720.0;

pub fn settings_panel(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let folio = folio_scaffold(
        app,
        move || {
            row![
                text("The ").size(34).font(SERIF).color(s.ink),
                text("appendix").size(34).font(SERIF_ITALIC).color(s.ink),
            ]
            .align_y(Alignment::End)
            .into()
        },
        move || vec![micro_label("Settings", s.faint).size(10.0).into()],
    );

    let cards = column![
        theme_card(app),
        // Task 6 inserts harnesses_card(app) here.
        // Task 7 inserts workers_card(app) here.
    ]
    .spacing(18)
    .width(Length::Fixed(COLUMN_W));

    column![
        folio,
        hline(s.ink, 2.0),
        scrollable(container(cards).width(Length::Fill).center_x(Length::Fill).padding([24, 28]))
            .height(Length::Fill),
    ]
    .width(Length::Fill)
    .into()
}

/// Shared card scaffold: micro-label heading over a rule, then the body.
fn card<'a>(app: &'a App, label: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
    let s = &app.scheme;
    container(
        column![
            micro_label(label, s.ink_2),
            Space::new(0, 10),
            hline(s.rule_dark, 1.0),
            Space::new(0, 14),
            body,
        ],
    )
    .padding([18, 22])
    .width(Length::Fill)
    .style(move |_theme| card_style(s))
    .into()
}

/// Theme card: the light/dark/ninox dots (relocated from the sidebar
/// footer) + a mono pointer to the active theme file.
fn theme_card(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let mut dots = row![].spacing(8).align_y(Alignment::Center);
    for variant in [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox] {
        let selected = app.active_variant == variant;
        let fill = match variant {
            ThemeVariant::Light => crate::theme::light().paper,
            ThemeVariant::Dark | ThemeVariant::Ninox => crate::theme::dark().paper,
        };
        let label = match variant {
            ThemeVariant::Light => "light",
            ThemeVariant::Dark  => "dark",
            ThemeVariant::Ninox => "ninox",
        };
        dots = dots.push(
            button(
                row![
                    container(Space::new(0, 0)).width(14).height(Length::Fixed(14.0)).style(
                        move |_| container::Style {
                            background: Some(Background::Color(fill)),
                            border: Border {
                                color:  if selected { s.accent } else { s.ink },
                                width:  if selected { 2.0 } else { 1.5 },
                                radius: 7.0.into(),
                            },
                            ..Default::default()
                        },
                    ),
                    Space::new(6, 0),
                    text(label).size(11).font(crate::style::SANS)
                        .color(if selected { s.ink } else { s.ink_2 }),
                ]
                .align_y(Alignment::Center),
            )
            .on_press(Message::SwitchTheme(variant))
            .style(|_t, _st| button::Style { background: None, border: Border::default(), ..Default::default() })
            .padding([2, 4]),
        );
    }

    let theme_file = app.config.theme_file.clone()
        .unwrap_or_else(|| "themes/field-notes.toml".to_string());

    card(app, "Theme", column![
        dots,
        Space::new(0, 12),
        text(theme_file).size(10).font(MONO).color(s.faint),
    ]
    .spacing(0)
    .into())
}
```

- [ ] **Step 5: sidebar footer** — in `sidebar.rs`, replace `theme_dots_footer` (:384-423) with a `Settings ▸` row and delete the dots fn (its logic now lives in `settings_panel::theme_card`; drop the now-unused `ThemeVariant` import):

```rust
/// Footer: `Settings ▸` row — opens The Appendix (theme dots live there now).
fn settings_footer(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let active = matches!(app.view, View::Settings);
    button(
        row![
            micro_label("Settings", if active { s.ink } else { s.ink_2 }).size(10.0),
            Space::new(Length::Fill, 0),
            text("▸").size(11).color(if active { s.accent } else { s.faint }),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(Message::NavigateSettings)
    .padding([12, 18])
    .width(Length::Fill)
    .style(move |_t, status| button::Style {
        background: (active || matches!(status, button::Status::Hovered))
            .then_some(Background::Color(s.card)),
        text_color: s.ink_2,
        border: Border::default(),
        ..Default::default()
    })
    .into()
}
```
and change the call site (:234) to `let footer = settings_footer(app);`.

- [ ] **Step 6: full test + clippy + commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): settings view (the appendix) with relocated theme card"
```

---

### Task 6: Harnesses card

**Files:**
- Modify: `crates/ninox-app/src/components/settings_panel.rs` (add `harnesses_card`)
- Modify: `crates/ninox-app/src/app.rs` (`SettingsToggleHarness` message + handler + tests)

**Interfaces:**
- Consumes: `registry.names()`, `registry.spec()` (Task 1), `card(...)` scaffold (Task 5).
- Produces: `Message::SettingsToggleHarness(String)`; writes the FULL effective spec (with `enabled` flipped) into `config.harnesses` and saves.

- [ ] **Step 1: failing tests** (app.rs; write-through requires `with_env_override`):
```rust
    #[test]
    fn toggling_a_harness_enables_it_and_persists() {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml").to_string_lossy().to_string();
        with_env_override("NINOX_CONFIG", &config_path, || {
            let m = base(test_engine());
            assert!(!m.config.registry().enabled_names().contains(&"codex".to_string()));
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            assert!(m.config.registry().enabled_names().contains(&"codex".to_string()));
            // persisted: a fresh load sees it too
            let loaded = ninox_core::config::AppConfig::load().unwrap();
            assert!(loaded.registry().enabled_names().contains(&"codex".to_string()));
            // toggling back disables
            let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
            assert!(!m.config.registry().enabled_names().contains(&"codex".to_string()));
        });
    }

    #[test]
    fn claude_code_toggle_is_inert() {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml").to_string_lossy().to_string();
        with_env_override("NINOX_CONFIG", &config_path, || {
            let m = base(test_engine());
            let (m, _) = m.update(Message::SettingsToggleHarness("claude-code".into()));
            assert!(m.config.registry().enabled_names().contains(&"claude-code".to_string()));
            assert!(m.config.harnesses.is_empty(), "inert toggle must not write config");
        });
    }
```
NOTE: check how existing tests reference the guard — `with_env_override` at app.rs:2994 already takes the `ENV_TEST_GUARD` lock internally (see `t_toggles_light_dark` :3012). Match the existing pattern exactly; drop the explicit `_guard` lines above if the helper already locks.

- [ ] **Step 2: run, verify fail.**

- [ ] **Step 3: handler** (app.rs):
```rust
            Message::SettingsToggleHarness(name) => {
                // claude-code is the locked-on default — inert by design.
                if name == "claude-code" {
                    return Task::none();
                }
                let mut spec = state.config.registry().spec(&name);
                spec.enabled = !spec.enabled;
                state.config.harnesses.insert(name.clone(), spec);
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save config after toggling harness {name}: {e}");
                }
                Task::none()
            }
```

- [ ] **Step 4: card render** (settings_panel.rs) — insert `harnesses_card(app)` into the column in `settings_panel`:
```rust
/// Harnesses card: one row per registry harness — ink-fill toggle, serif
/// name, mono binary, `workers ✓/–` marker. No model field here by design.
fn harnesses_card(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let registry = app.config.registry();
    let mut rows = column![].spacing(10);
    for name in registry.names() {
        let spec = registry.spec(&name);
        let locked = name == "claude-code";
        let enabled = spec.enabled;
        let binary = spec.binary.clone().unwrap_or_else(|| name.clone());
        let workers = if spec.worker_args.is_some() { "workers ✓" } else { "workers –" };

        let toggle = button(Space::new(0, 0))
            .on_press_maybe((!locked).then(|| Message::SettingsToggleHarness(name.clone())))
            .width(Length::Fixed(30.0))
            .height(Length::Fixed(16.0))
            .padding(0)
            .style(move |_t, status| button::Style {
                background: enabled.then_some(Background::Color(s.ink)),
                text_color: s.ink,
                border: Border {
                    color: if matches!(status, button::Status::Hovered) && !locked { s.accent } else { s.ink },
                    width: 1.5,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });

        let name_label = text(name.clone()).size(14).font(SERIF)
            .color(if enabled { s.ink } else { s.ink_2 });
        let suffix: Element<Message> = if locked {
            text("default").size(9).font(MONO).color(s.faint).into()
        } else {
            Space::new(0, 0).into()
        };

        rows = rows.push(
            row![
                toggle,
                Space::new(12, 0),
                name_label,
                Space::new(8, 0),
                suffix,
                Space::new(Length::Fill, 0),
                text(binary).size(10).font(MONO).color(s.faint),
                Space::new(14, 0),
                text(workers).size(10).font(MONO)
                    .color(if spec.worker_args.is_some() { s.ink_2 } else { s.faint }),
            ]
            .align_y(Alignment::Center),
        );
    }
    card(app, "Harnesses", rows.into())
}
```

- [ ] **Step 5: full test + clippy + commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): harness registry toggles in settings"
```

---

### Task 7: Workers card (the one unmanned decision)

**Files:**
- Modify: `crates/ninox-app/src/components/settings_panel.rs` (add `workers_card`)
- Modify: `crates/ninox-app/src/app.rs` (messages + handlers + tests)

**Interfaces:**
- Consumes: `model_options`/`CUSTOM_SENTINEL`/`ensure_models` (Task 3), `SettingsState.worker_custom` (Task 5), `config.worker: AgentConfig`.
- Produces: `Message::SettingsWorkerHarness(String)`, `SettingsWorkerModel(String)`, `SettingsWorkerCustomModel(String)`, `SettingsWorkerCustomCommit`.

- [ ] **Step 1: failing tests** (app.rs, `with_env_override` pattern as Task 6):
```rust
    #[test]
    fn worker_harness_change_persists_and_clears_model() {
        // (env-override boilerplate as in toggling_a_harness_enables_it_and_persists)
        let m = base(test_engine());
        let (m, _) = m.update(Message::SettingsToggleHarness("codex".into()));
        let (m, _) = m.update(Message::SettingsWorkerHarness("codex".into()));
        assert_eq!(m.config.worker.harness, "codex");
        assert!(m.config.worker.model.is_none(), "stale model must not leak across harnesses");
        let loaded = ninox_core::config::AppConfig::load().unwrap();
        assert_eq!(loaded.worker.harness, "codex");
    }

    #[test]
    fn worker_model_pick_persists_and_custom_commits() {
        // (env-override boilerplate)
        let m = base(test_engine());
        let (m, _) = m.update(Message::SettingsWorkerModel("claude-haiku-4-5".into()));
        assert_eq!(m.config.worker.model.as_deref(), Some("claude-haiku-4-5"));

        // custom… opens the input without touching config
        let (m, _) = m.update(Message::SettingsWorkerModel(crate::models::CUSTOM_SENTINEL.into()));
        assert!(m.settings.worker_custom.is_some());
        assert_eq!(m.config.worker.model.as_deref(), Some("claude-haiku-4-5"));

        // typing + commit writes and closes custom mode
        let (m, _) = m.update(Message::SettingsWorkerCustomModel("my-model".into()));
        let (m, _) = m.update(Message::SettingsWorkerCustomCommit);
        assert_eq!(m.config.worker.model.as_deref(), Some("my-model"));
        assert!(m.settings.worker_custom.is_none());
        let loaded = ninox_core::config::AppConfig::load().unwrap();
        assert_eq!(loaded.worker.model.as_deref(), Some("my-model"));
    }
```

- [ ] **Step 2: run, verify fail.**

- [ ] **Step 3: handlers** (app.rs):
```rust
            Message::SettingsWorkerHarness(h) => {
                let task = Self::ensure_models(state, &h);
                state.config.worker.harness = h;
                state.config.worker.model = None;
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save worker harness: {e}");
                }
                task
            }

            Message::SettingsWorkerModel(v) => {
                if v == crate::models::CUSTOM_SENTINEL {
                    state.settings.worker_custom =
                        Some(state.config.worker.model.clone().unwrap_or_default());
                    return Task::none();
                }
                state.settings.worker_custom = None;
                state.config.worker.model = Some(v);
                if let Err(e) = state.config.save() {
                    tracing::warn!("failed to save worker model: {e}");
                }
                Task::none()
            }

            Message::SettingsWorkerCustomModel(v) => {
                state.settings.worker_custom = Some(v);
                Task::none()
            }

            Message::SettingsWorkerCustomCommit => {
                if let Some(v) = state.settings.worker_custom.take() {
                    let t = v.trim();
                    state.config.worker.model = (!t.is_empty()).then(|| t.to_string());
                    if let Err(e) = state.config.save() {
                        tracing::warn!("failed to save worker model: {e}");
                    }
                }
                Task::none()
            }
```

- [ ] **Step 4: card render** (settings_panel.rs; insert `workers_card(app)` after `harnesses_card`):
```rust
/// Workers card — what `ninox spawn` launches when orchestrator agents
/// spawn workers: harness picker (enabled, worker-capable only) + model
/// picker (select with a `custom…` escape hatch). Maps to config `[worker]`.
fn workers_card(app: &App) -> Element<'_, Message> {
    use iced::widget::{pick_list, text_input};
    let s = &app.scheme;
    let registry = app.config.registry();

    let harness_opts: Vec<String> = registry.enabled_names().into_iter()
        .filter(|n| registry.spec(n).worker_args.is_some())
        .collect();
    let harness_sel = harness_opts.iter()
        .position(|n| n == &app.config.worker.harness)
        .map(|i| harness_opts[i].clone());
    let harness_pick = pick_list(harness_opts, harness_sel, Message::SettingsWorkerHarness)
        .font(MONO).text_size(12).padding([6, 10]).style(pick_style(s));

    let spec = registry.spec(&app.config.worker.harness);
    let discovered = app.model_lists.get(&app.config.worker.harness).and_then(|m| m.as_deref());
    let configured = app.config.worker.model.clone().or_else(|| spec.model.clone());
    let model_opts = crate::models::model_options(&spec, discovered, configured.as_deref());
    let model_sel = if app.settings.worker_custom.is_some() {
        Some(crate::models::CUSTOM_SENTINEL.to_string())
    } else {
        configured
    };
    let model_pick = pick_list(model_opts, model_sel, Message::SettingsWorkerModel)
        .placeholder("harness default")
        .font(MONO).text_size(12).padding([6, 10]).style(pick_style(s));

    let mut body = column![
        row![
            column![micro_label("Harness", s.faint), Space::new(0, 6), harness_pick].spacing(0),
            Space::new(20, 0),
            column![micro_label("Model", s.faint), Space::new(0, 6), model_pick].spacing(0),
        ]
        .align_y(Alignment::Start),
    ]
    .spacing(0);
    if let Some(v) = &app.settings.worker_custom {
        body = body.push(Space::new(0, 10)).push(
            text_input("model id", v)
                .on_input(Message::SettingsWorkerCustomModel)
                .on_submit(Message::SettingsWorkerCustomCommit)
                .font(MONO).size(12).padding([4, 2])
                .style(style::underlined_input_style(s)),
        );
    }
    card(app, "Workers", body.into())
}
```
`pick_style` currently lives in `spawn_modal.rs` (:100-108) — move it to `style.rs` as `pub fn pick_style<'a>(s: &'a ColorScheme) -> impl Fn(&iced::Theme, pick_list::Status) -> pick_list::Style + 'a` and re-point spawn_modal's uses (avoids duplicating; matches the ledgered "shared helpers" directive from T11).
Also: opening Settings should kick model discovery for the worker harness — in the `Message::NavigateSettings` handler return `Self::ensure_models(state, &state.config.worker.harness.clone())` instead of `Task::none()`.

- [ ] **Step 5: full test + clippy + commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): worker-default harness and model pickers in settings"
```

---

### Task 8: Re-file — respawn a session onto current settings

**Files:**
- Modify: `crates/ninox-core/src/store.rs` (migration + `catalogue_path` column)
- Modify: `crates/ninox-core/src/types.rs` (Session field)
- Modify: `crates/ninox-app/src/spawn_util.rs` (record catalogue on spawn)
- Modify: `crates/ninox-app/src/main.rs` (`run_spawn` records `NINOX_BRAIN` as catalogue_path)
- Modify: `crates/ninox-app/src/app.rs` (`RefileSession` message/handler, `refile_plan` pure helper + tests, Session literals gain the field)
- Modify: `crates/ninox-app/src/components/session_detail.rs` (Re-file button)

**Interfaces:**
- Consumes: `tmux::kill_session` (`crates/ninox-core/src/tmux.rs:287`), `spawn_interactive_session`, `HarnessRegistry::interactive_cmd`.
- Produces: `Session.catalogue_path: Option<String>`; `Message::RefileSession(SessionId)`; pure `fn refile_plan(...) -> Option<RefilePlan>`.

- [ ] **Step 1: store migration + Session field.** In `store.rs` extend the idempotent migration list (:44-55): add `("catalogue_path", "ALTER TABLE sessions ADD COLUMN catalogue_path TEXT"),`. Add `pub catalogue_path: Option<String>,` to `Session` in `types.rs` (after `context_tokens`). Update `upsert_session` INSERT column list + params, and the row-mapping in `get_session`/`list_sessions` (mirror exactly how `model` was added in PR #18 — same files, same shape). Fix every `Session { ... }` literal across the workspace (compiler-driven: `cargo check --workspace` lists them; app.rs ×2 spawn paths use `Some(catalogue_path.clone())`, spawn_util.rs `updated` uses `(!p.catalogue_path.is_empty()).then(|| p.catalogue_path.clone())`, main.rs run_spawn uses `std::env::var("NINOX_BRAIN").ok().filter(|s| !s.is_empty())`, tests/probes use `None`). Store round-trip test: upsert a session with `catalogue_path: Some("/brains/x".into())`, `get_session`, assert it survives.

- [ ] **Step 2: failing tests for the plan helper** (app.rs):
```rust
    #[test]
    fn refile_plan_resolves_agent_through_current_registry() {
        let mut cfg = ninox_core::config::AppConfig::default();
        // simulate a spec change since the session was filed
        cfg.harnesses.insert("claude-code".to_string(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary:  Some("claude-nightly".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..Default::default()
        });
        let session = ninox_core::types::Session {
            id: "s1".into(), orchestrator_id: None, name: "s1".into(), repo: String::new(),
            status: ninox_core::types::SessionStatus::Terminated, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/tmp/ws".into()), pid: None,
            model: Some("claude-opus-4-8".into()), context_tokens: None,
            catalogue_path: Some("/brains/b".into()),
        };
        let plan = refile_plan(&session, false, &cfg).expect("plan");
        assert_eq!(plan.base_cmd, "claude-nightly --model 'claude-opus-4-8'");
        assert_eq!(plan.workspace, "/tmp/ws");
        assert_eq!(plan.catalogue_path, "/brains/b");
        assert!(plan.extra_env.is_empty());
    }

    #[test]
    fn refile_plan_orchestrator_gets_caller_env_and_no_workspace_means_no_plan() {
        let cfg = ninox_core::config::AppConfig::default();
        let mut session = ninox_core::types::Session {
            id: "o1".into(), orchestrator_id: None, name: "o1".into(), repo: String::new(),
            status: ninox_core::types::SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/tmp/o1".into()), pid: None,
            model: None, context_tokens: None, catalogue_path: None,
        };
        let plan = refile_plan(&session, true, &cfg).expect("plan");
        assert!(plan.extra_env.iter().any(|(k, v)| k == "NINOX_ORCHESTRATOR_ID" && v == "o1"));
        assert!(plan.extra_env.iter().any(|(k, _)| k == "ATHENE_CALLER_TYPE"));
        // no recorded catalogue → default brain path
        assert!(!plan.catalogue_path.is_empty());

        session.workspace_path = None;
        assert!(refile_plan(&session, true, &cfg).is_none());
    }
```

- [ ] **Step 3: run, verify fail.**

- [ ] **Step 4: implement** (app.rs):
```rust
/// Everything a Re-file needs, resolved against the CURRENT registry —
/// settings changes apply to new spawns immediately and to existing
/// sessions on Re-file; no silent in-place swaps (spec §V "Updating
/// running sessions"). Pure so it's unit-testable without tmux.
pub struct RefilePlan {
    pub agent:          ninox_core::config::AgentConfig,
    pub base_cmd:       String,
    pub workspace:      String,
    pub catalogue_path: String,
    pub extra_env:      Vec<(String, String)>,
}

pub fn refile_plan(
    session: &Session,
    is_orchestrator: bool,
    config: &ninox_core::config::AppConfig,
) -> Option<RefilePlan> {
    let workspace = session.workspace_path.clone()?;
    let agent = ninox_core::config::AgentConfig {
        harness: session.agent_type.clone(),
        model:   session.model.clone(),
    };
    let base_cmd = config.registry().interactive_cmd(&agent);
    let catalogue_path = session.catalogue_path.clone()
        .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());
    let extra_env = if is_orchestrator {
        vec![
            ("NINOX_ORCHESTRATOR_ID".to_string(), session.id.clone()),
            ("AO_CALLER_TYPE".to_string(),        "orchestrator".to_string()),
            ("ATHENE_CALLER_TYPE".to_string(),    "orchestrator".to_string()),
        ]
    } else {
        Vec::new()
    };
    Some(RefilePlan { agent, base_cmd, workspace, catalogue_path, extra_env })
}
```
Message variant `RefileSession(SessionId)` + handler:
```rust
            Message::RefileSession(id) => {
                let Some(session) = state.sessions.get(&id).cloned() else { return Task::none(); };
                let is_orch = state.orchestrators.iter().any(|o| o.id == id);
                let Some(plan) = refile_plan(&session, is_orch, &state.config) else {
                    tracing::warn!("refile {id}: no workspace recorded, cannot respawn");
                    return Task::none();
                };
                // Drop the live client/grid first so the old PTY doesn't
                // fight the respawn for the same tmux session name.
                state.clients.remove(&id);
                state.terminals.remove(&id);

                let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
                let engine = state.engine.clone();
                let name   = session.name.clone();
                let repo   = session.repo.clone();
                let orch_id = session.orchestrator_id.clone();
                Task::future(async move {
                    // Ignore kill errors — a Terminated husk has no tmux
                    // session, and Re-file on one "just spawns".
                    let _ = ninox_core::tmux::kill_session(&id).await;
                    let attach = crate::spawn_util::spawn_interactive_session(
                        engine,
                        crate::spawn_util::InteractiveSpawnParams {
                            session_id:      id.clone(),
                            name,
                            workspace:       plan.workspace,
                            repo,
                            orchestrator_id: orch_id,
                            agent:           plan.agent,
                            base_cmd:        plan.base_cmd,
                            catalogue_path:  plan.catalogue_path,
                            extra_env:       plan.extra_env,
                            started_at:      ts,
                        },
                    )
                    .await;
                    match attach {
                        Some(argv) => Message::ClientAttach { session_id: id, argv },
                        None       => Message::Noop,
                    }
                })
            }
```
(Behavioral note, documented in the handler comment if not obvious: `spawn_interactive_session` upserts the record with `cost_usd: 0.0`; the usage poller re-ingests real spend from the workspace transcript, so cost repopulates.)

- [ ] **Step 5: Re-file button** (session_detail.rs, header row :263-277) — rendered for ALL sessions (this also covers the respawn-over-Terminated-husk gap; orchestrators get Re-file even though they have no Kill):
```rust
    let refile_btn: Element<Message> = {
        let sid = session_id.to_string();
        button(crate::style::micro_label("Re-file", s.ink_2).size(10.0))
            .on_press(Message::RefileSession(sid))
            .padding([6, 16])
            .style(move |_theme, status| {
                let hovered = matches!(status, button::Status::Hovered);
                button::Style {
                    background: hovered.then_some(Background::Color(s.ink)),
                    text_color: if hovered { s.card } else { s.ink_2 },
                    border: Border { color: s.ink_2, width: 1.5, radius: 2.0.into() },
                    shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
                }
            })
            .into()
    };
```
Insert into the header row before `kill_btn`: `refile_btn, Space::new(10, 0), kill_btn` (`kill_btn` remains non-orchestrator-only; `refile_btn` unconditional).

- [ ] **Step 6: full test + clippy + commit**

```bash
git add crates/
git commit -m "feat(native-app): per-session re-file action respawning onto current settings"
```

---

### Task 9: Finalize — docs, whole-branch verify

**Files:**
- Modify: `docs/design-concepts/field-notes-design.md` (§V status: mark implemented, note any deliberate deltas)
- Modify: `.superpowers/sdd/progress.md` (ledger entry)

- [ ] **Step 1:** `cargo test --workspace 2>&1 | tail -5` → all green.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings` → clean.
- [ ] **Step 3:** Add a one-line "Implemented 2026-07-05 (feat/settings-harnesses)" annotation under the §V heading; record deliberate deltas (e.g. shell-quoted substituted args; re-filed workers respawn interactively without their original prompt; config `[harnesses.*]` entries replace builtin specs wholesale).
- [ ] **Step 4:** Ledger entry in `.superpowers/sdd/progress.md` (task completions + carried minors).
- [ ] **Step 5:** Commit docs:
```bash
git add docs/ .superpowers/
git commit -m "docs(design): mark settings appendix implemented, record deltas"
```
Then: launch the app from the worktree (`cargo build && NINOX_CONFIG=<scratch> ./target/debug/ninox`) for a visual pass of the settings view + spawn modal + Re-file before PR (per repo sign-off convention); PR + reviewer follow the repo workflow (no test plan, no Claude annotation, `now-playing` footer).

---

## Self-Review (done at planning time)

- **Spec coverage:** registry-as-data ✓ (T1), zero-Rust-changes-per-harness ✓ (T1: config extends), exact launch shapes ✓ (T1 ported tests), call sites resolve through registry ✓ (T2), worker-incapable harness excluded from CLI worker path ✓ (T2 bail + T7 filter), model precedence + custom… ✓ (T3/T4/T7), settings view + theme relocation ✓ (T5), harness toggles w/ locked claude-code + workers ✓/– ✓ (T6), workers card = the one unmanned decision ✓ (T7), `[orchestrator]` as remembered preselection only ✓ (T4 persists on confirm; no settings surface), Re-file incl. Terminated-husk ✓ (T8), serde-defaulted ✓ (T1).
- **Known deliberate deltas:** substituted args shell-quoted (semantically identical); re-filed workers respawn interactively (original worker prompt is not stored); config harness entries replace builtins wholesale (documented in T1 doc comment + T9 design-doc note).
- **Type consistency check:** `base_cmd: String` (T2) used by T4/T8 ✓; `model_lists: HashMap<String, Option<Vec<String>>>` (T3) read by T4/T7 ✓; `SettingsState.worker_custom` (T5) used by T7 ✓; `Session.catalogue_path: Option<String>` (T8) written by all three spawn paths ✓; `CUSTOM_SENTINEL` shared by T4/T7 ✓.
