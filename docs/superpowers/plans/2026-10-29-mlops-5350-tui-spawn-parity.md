# MLOPS-5350 (Worker A, item 1): TUI spawn dialog parity — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the TUI's spawn modal (`n`) the same fields as the GUI's `Spawn.dc.html`-driven dialog — Kind, Name, Workspace (Standalone only), Worker type, Agent, Model, Brain catalogue, Initial prompt, cost estimate line — spawning through the same core path the GUI uses, extended to carry `--type`.

**Architecture:** Lift the GUI's pure cost-estimate helpers into `ninox-ui-shared` (first, its own commit) so both apps share them. Rebuild the TUI's `SpawnModal` as a `Field`/`FieldId` list (the same shape `worker_types.rs`/`settings.rs` already use), replacing the current 2-field (name/prompt) struct. Extend `Action::Spawn` to carry the whole form. For the Orchestrator kind, extend the already-shared `spawn_orchestrator_common` with optional agent/catalogue overrides (defaulting to today's exact behavior). For the Standalone kind, add a new `spawn_standalone_worker` next to `run_spawn` in `ninox-ui-shared/src/cli.rs`, reusing `run_spawn`'s private type-resolution helpers but launching via `spawn_util::spawn_interactive_session` (the same engine the GUI's Standalone path already calls), so the result is an attached, interactive session — not a headless `ninox spawn` worker — with `worker_type`/`success_criteria` now stamped correctly.

**Tech Stack:** Rust, ratatui (`ninox-tui`), existing `ninox-ui-shared`/`ninox-core` session+spawn plumbing.

**Spec:** `/Users/ethan.brodie/proj/ninox/.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md` (item 1), `BRIEF-common.md` §7, `Spawn.dc.html`.

## Global Constraints

- Branch from a fresh `origin/v1`, PR targets `v1`. PR title: `MLOPS-5350: feat(tui): <item>`.
- Spawn through the same core path as the GUI; pass `--type`.
- Tab/Shift-Tab move between fields, `ctrl+enter` spawns, Esc cancels.
- Do not copy GUI-only logic into the TUI — move it to `ninox-ui-shared` and share it.
- The GUI has no Worker Type field yet (that lands separately, ticket G4) — this PR's Worker Type field is TUI-only; do not touch the GUI's `SpawnForm` fields, only the helpers it already shares.
- Orchestrator approval (29 Oct): `m` stays the fleet compact-toggle (unaffected by this PR); `ctrl+r`/restart-all changes belong to item 3, not this PR.
- `cargo fmt`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace` must pass before pushing. TDD throughout.

## Review Focus

1. Typing a Name that collides with an existing session/orchestrator id must show the GUI's "already exists" error and keep the modal open, not upsert over the existing record.
2. A Standalone workspace path that doesn't exist but sits under a git repo must create a worktree there (GUI's fallback), not just fail.
3. Picking a Standalone spawn with worker type `Orchestrator` (or any hidden type) must be rejected before any side effect (no upsert, no tmux call).
4. Whitespace-only Name (either kind) or whitespace-only Workspace (Standalone) on `ctrl+enter` must keep the modal open, not submit.
5. Switching Kind from Standalone back to Orchestrator must not let a previously-typed Workspace/Worker-type value leak into the submitted `Action::Spawn` — those fields must read back `None` for an Orchestrator submit even if the user had filled them in before switching.

---

### Task 1: Move spawn cost-estimate helpers to `ninox-ui-shared`

**Files:**
- Create: `crates/ninox-ui-shared/src/spawn_form.rs`
- Modify: `crates/ninox-ui-shared/src/lib.rs` (add `pub mod spawn_form;`)
- Modify: `crates/ninox-gui/src/components/spawn_modal.rs:24-31,42-82,403-454` (remove the moved items, import from `ninox_ui_shared::spawn_form`, keep `SpawnForm`/rendering GUI-local)

**Interfaces:**
- Produces (used by Tasks 2 and 7): `pub enum SpawnKind { Orchestrator, Standalone }` (`#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]`, `Orchestrator` is `#[default]`); `pub fn static_prior(harness: &str, model: Option<&str>) -> Option<(f64, f64)>`; `pub fn estimate_text(prior: Option<(f64, f64)>, historical_costs: &[f64]) -> String`; `pub fn effective_model(custom_model: Option<&str>, model: Option<&str>, spec: &ninox_core::harness::HarnessSpec) -> Option<String>` (note the signature decouples from GUI's `SpawnForm` struct — takes the two raw fields instead).

- [ ] **Step 1:** Write failing tests in `crates/ninox-ui-shared/src/spawn_form.rs` porting the five existing GUI tests (`static_priors_cover_claude_models_only`, `estimate_text_falls_back_to_static_range_when_no_history`, `estimate_text_falls_back_below_min_sample_count`, `estimate_text_uses_historical_average_at_min_sample_count`, `estimate_text_without_prior_or_history_says_so`, `effective_model_prefers_custom_text`) against the new free-function signatures.
- [ ] **Step 2:** Run `cargo test -p ninox-ui-shared spawn_form` — expect FAIL (module doesn't exist).
- [ ] **Step 3:** Implement `spawn_form.rs` by moving the bodies of `static_prior`/`estimate_text`/`effective_model`/`SpawnKind`/`MIN_HISTORY_SAMPLES` verbatim from `ninox-gui/src/components/spawn_modal.rs`, adjusting `effective_model`'s signature as above.
- [ ] **Step 4:** Update `ninox-gui/src/components/spawn_modal.rs`: delete the moved items, `use ninox_ui_shared::spawn_form::{SpawnKind, static_prior, estimate_text, effective_model};`, change `SpawnForm.kind`'s type to the imported `SpawnKind`, and change the one call site (`effective_model(form, &spec)`) to `effective_model(form.custom_model.as_deref(), form.model.as_deref(), &spec)`.
- [ ] **Step 5:** Run `cargo test -p ninox-ui-shared -p ninox-gui` — expect PASS, no duplicate test names left in `spawn_modal.rs`.
- [ ] **Step 6:** `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 7: Commit** as its own commit: `git add crates/ninox-ui-shared/src/spawn_form.rs crates/ninox-ui-shared/src/lib.rs crates/ninox-gui/src/components/spawn_modal.rs && git commit -m "refactor(ui-shared): move spawn cost-estimate helpers out of the GUI"`.

### Task 2: TUI spawn form as a `Field`/`FieldId` list

**Files:**
- Create: `crates/ninox-tui/src/tui/spawn.rs`
- Modify: `crates/ninox-tui/src/tui/mod.rs` (add `mod spawn;`)

**Interfaces:**
- Consumes: `ninox_ui_shared::spawn_form::SpawnKind` (Task 1); `super::settings::Kind` (existing: `Toggle | Choice | Text | Action`); `super::worker_types::Field`-shaped pattern (not imported — mirrored locally since `FieldId` differs).
- Produces (used by Tasks 3, 7): 
  - `pub enum FieldId { Kind, Name, Workspace, WorkerType, Harness, Model, CustomModel, Catalogue, Prompt }`
  - `pub struct Field { pub id: FieldId, pub label: String, pub value: String, pub kind: super::settings::Kind, pub options: Vec<String> }`
  - `pub struct SpawnForm { pub fields: Vec<Field>, pub focus: usize, pub error: Option<String> }` (replaces the old `state::SpawnModal`)
  - `pub fn build(config: &AppConfig) -> SpawnForm` — populates `Harness` options from `config.registry().enabled_names()`, `WorkerType` options from `config.worker_type_registry().user_facing_names()` (default value `"Implementor"`), `Catalogue` options from `config.catalogue_options()` (names only; default index 0), `Kind` options `["Orchestrator", "Standalone"]` (default `"Orchestrator"`), `Model` options from `ninox_ui_shared::models::model_options(&registry.spec(&harness), None, None)` recomputed whenever `Harness` changes (Step 4 below).
  - `pub fn kind(form: &SpawnForm) -> ninox_ui_shared::spawn_form::SpawnKind` — reads the `Kind` field's value.
  - `pub fn visible(form: &SpawnForm, id: &FieldId) -> bool` — `Workspace`/`WorkerType` are hidden (not focusable, not submitted) when `kind(form) == Orchestrator`; `CustomModel` hidden unless `Model`'s value is `ninox_ui_shared::models::CUSTOM_SENTINEL`.
  - `pub fn next_focus(form: &SpawnForm, back: bool) -> usize` — cycles `focus` across only `visible` field indices.
  - `pub fn step(form: &mut SpawnForm, back: bool)` — on the focused `Choice` field, steps `options` (wrapping); stepping `Kind` or `Harness` resets dependent fields (`Model`→index 0 and recomputes its `options` when `Harness` changes; `Workspace`/`WorkerType` values are left in place but become non-`visible`/non-submitted per the Review Focus item).
  - `pub fn push_char(form: &mut SpawnForm, c: char)` / `pub fn backspace(form: &mut SpawnForm)` — edits the focused `Text` field's `value` (`Name`, `Workspace`, `CustomModel`, `Prompt`).

- [ ] **Step 1: Write the failing tests** in `crates/ninox-tui/src/tui/spawn.rs`:
  ```rust
  #[test]
  fn build_lists_every_field_in_order() {
      let f = build(&AppConfig::default());
      assert_eq!(f.fields.iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
          vec![FieldId::Kind, FieldId::Name, FieldId::Workspace, FieldId::WorkerType,
               FieldId::Harness, FieldId::Model, FieldId::CustomModel, FieldId::Catalogue, FieldId::Prompt]);
  }
  #[test]
  fn worker_type_defaults_to_implementor() {
      let f = build(&AppConfig::default());
      assert_eq!(f.fields.iter().find(|x| x.id == FieldId::WorkerType).unwrap().value, "Implementor");
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
      assert_eq!(f.fields[n].id, FieldId::WorkerType); // skips Workspace, both hidden
  }
  #[test]
  fn switching_kind_back_to_orchestrator_keeps_typed_workspace_but_hides_it() {
      let mut f = build(&AppConfig::default());
      let ws_idx = f.fields.iter().position(|x| x.id == FieldId::Workspace).unwrap();
      f.focus = f.fields.iter().position(|x| x.id == FieldId::Kind).unwrap();
      step(&mut f, false); // -> Standalone
      f.focus = ws_idx;
      push_char(&mut f, '~'); push_char(&mut f, '/'); push_char(&mut f, 'x');
      f.focus = f.fields.iter().position(|x| x.id == FieldId::Kind).unwrap();
      step(&mut f, true); // back -> Orchestrator
      assert!(!visible(&f, &FieldId::Workspace));
      assert_eq!(f.fields[ws_idx].value, "~/x"); // still held, just not submitted (Task 3 reads only visible fields)
  }
  #[test]
  fn changing_harness_resets_model_options() {
      let mut cfg = AppConfig::default();
      // two enabled harnesses with distinct known_models, as fixtures already do in worker_types.rs tests
      let mut f = build(&cfg);
      f.focus = f.fields.iter().position(|x| x.id == FieldId::Harness).unwrap();
      step(&mut f, false);
      let model_field = f.fields.iter().find(|x| x.id == FieldId::Model).unwrap();
      assert_eq!(model_field.value, model_field.options[0]);
  }
  ```
  (Use `WorkerTypeSpec`/harness fixtures the way `crates/ninox-tui/src/tui/worker_types.rs`'s existing tests build them — check that file for the exact fixture-construction helper before writing these.)
- [ ] **Step 2:** `cargo test -p ninox-tui spawn::` — expect FAIL (module/types don't exist).
- [ ] **Step 3:** Implement `spawn.rs` per the Interfaces block above.
- [ ] **Step 4:** Run tests — expect PASS.
- [ ] **Step 5:** `cargo fmt && cargo clippy -p ninox-tui --all-targets -- -D warnings`.
- [ ] **Step 6: Commit:** `git add crates/ninox-tui/src/tui/spawn.rs crates/ninox-tui/src/tui/mod.rs && git commit -m "feat(tui): spawn form field model (Kind/Name/Workspace/Type/Agent/Model/Catalogue/Prompt)"`.

### Task 3: Wire the new form into `state.rs` (`Modal::Spawn`, keys, `Action::Spawn`)

**Files:**
- Modify: `crates/ninox-tui/src/tui/state.rs:167-172` (replace `struct SpawnModal`), `:235-247` (`Modal::Spawn` variant), `:483` (`Action::Spawn`), `:1463` (`n` key), `:2335-2374` (`spawn_modal_key`), `:4023-4377` (existing spawn tests — update to the new shape)

**Interfaces:**
- Consumes: `spawn::{SpawnForm, FieldId, build, kind, visible, next_focus, step, push_char, backspace}` (Task 2); `ninox_ui_shared::spawn_form::SpawnKind` (Task 1).
- Produces (used by Task 6): `Action::Spawn { kind: SpawnKind, name: String, workspace: Option<String>, worker_type: Option<String>, harness: String, model: Option<String>, catalogue_idx: usize, prompt: Option<String> }` — `workspace`/`worker_type` are `None` whenever `kind == Orchestrator` regardless of the form's held values (Review Focus item 5); `model` is `None` when the form's effective model (Task 1's `effective_model`, fed the `Model`/`CustomModel` field values) is `None`.

- [ ] **Step 1: Write the failing tests** (replacing the four existing spawn tests at `state.rs:4023-4062,4377`):
  ```rust
  #[test]
  fn spawn_modal_opens_on_n() { /* n -> Modal::Spawn(form) with FieldId::Kind focused, value "Orchestrator" */ }
  #[test]
  fn ctrl_enter_with_empty_name_keeps_modal_open_with_error() { /* ... */ }
  #[test]
  fn ctrl_enter_standalone_with_empty_workspace_keeps_modal_open_with_error() { /* ... */ }
  #[test]
  fn plain_enter_on_prompt_field_inserts_newline_not_submit() { /* ... */ }
  #[test]
  fn ctrl_enter_orchestrator_submits_with_workspace_and_type_none() {
      // fill Name; leave Kind at default Orchestrator
      // assert_eq!(handle_key(...), Action::Spawn { kind: SpawnKind::Orchestrator, name: "demo".into(),
      //     workspace: None, worker_type: None, harness: .., model: .., catalogue_idx: 0, prompt: None })
  }
  #[test]
  fn ctrl_enter_standalone_submits_with_workspace_and_type() { /* ... */ }
  #[test]
  fn esc_cancels_and_clears_modal() { /* existing coverage, updated to the new struct */ }
  #[test]
  fn tab_and_shift_tab_cycle_focus_through_visible_fields_only() { /* ... */ }
  ```
- [ ] **Step 2:** `cargo test -p ninox-tui state::` — expect FAIL.
- [ ] **Step 3:** Implement: replace `SpawnModal` with `spawn::SpawnForm` in the `Modal::Spawn` variant; `n` key now calls `spawn::build(&st.config)` (check what config accessor `state.rs` already uses elsewhere, e.g. `st.config` vs a loaded `AppConfig` — match the existing pattern used by `w` → Worker Types / `s` → Settings); rewrite `spawn_modal_key` to route `Tab`/`BackTab` → `next_focus`, `Left`/`Right`/`h`/`l` → `step` (match whichever convention `worker_types_key` uses for its own Choice fields — mirror it exactly), `Backspace` → `backspace`, `Char(c)` → `push_char` (with `Enter` producing a `\n` push when focused on `Prompt`, not a field-advance), `(ctrl, Enter)` → validate (non-empty trimmed `Name`; non-empty trimmed `Workspace` when `kind == Standalone`) and emit `Action::Spawn` or set `form.error` and stay open, `Esc` → cancel.
- [ ] **Step 4:** Run tests — expect PASS.
- [ ] **Step 5:** `cargo fmt && cargo clippy -p ninox-tui --all-targets -- -D warnings`.
- [ ] **Step 6: Commit:** `git add crates/ninox-tui/src/tui/state.rs && git commit -m "feat(tui): wire the spawn form into Modal::Spawn and Action::Spawn"`.

### Task 4: Extend `spawn_orchestrator_common` with optional agent/catalogue overrides

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs:2505-2620` (`spawn_orchestrator_common` signature + body), its three call sites (`run_spawn_orchestrator`, `run_spawn_sub_orchestrator`, and the existing TUI call in `mod.rs:825` — updated in Task 6, not here)

**Interfaces:**
- Produces (used by Task 6): `pub async fn spawn_orchestrator_common(store: &Store, config: &AppConfig, name: &str, prompt: Option<String>, parent_orchestrator_id: Option<&str>, agent_override: Option<ninox_core::config::AgentConfig>, catalogue_path_override: Option<String>) -> anyhow::Result<SpawnedOrchestrator>` — `None, None` must reproduce today's exact behavior byte-for-byte (existing tests at `cli.rs` covering this function must pass unmodified except for the two new trailing `None` args).

- [ ] **Step 1: Write the failing test:**
  ```rust
  #[tokio::test]
  async fn spawn_orchestrator_common_applies_agent_and_catalogue_overrides() {
      // call with Some(AgentConfig{harness:"codex", model:Some("x")}), Some("/tmp/other-brain")
      // assert the resulting session.agent_type == "codex", session.model == Some("x"),
      // session.catalogue_path == Some("/tmp/other-brain")
  }
  ```
- [ ] **Step 2:** `cargo test -p ninox-ui-shared spawn_orchestrator_common` — expect FAIL (signature mismatch).
- [ ] **Step 3:** Implement: add the two params; `let agent = agent_override.unwrap_or_else(|| config.orchestrator.clone());` and `let catalogue_path = catalogue_path_override.unwrap_or_else(|| /* existing NINOX_BRAIN-or-config fallback */);`; update the two existing call sites (`run_spawn_orchestrator`, `run_spawn_sub_orchestrator`) to pass `None, None`.
- [ ] **Step 4:** Run `cargo test -p ninox-ui-shared` — expect all PASS (existing + new).
- [ ] **Step 5:** `cargo fmt && cargo clippy -p ninox-ui-shared --all-targets -- -D warnings`.
- [ ] **Step 6: Commit:** `git add crates/ninox-ui-shared/src/cli.rs && git commit -m "feat(ui-shared): let spawn_orchestrator_common take an agent/catalogue override"`.

### Task 5: `spawn_standalone_worker` — Standalone kind, with `--type`

**Files:**
- Modify: `crates/ninox-ui-shared/src/cli.rs` (new `pub async fn` placed directly after `run_spawn`, reusing its sibling private helpers)

**Interfaces:**
- Consumes: `WorkerTypeSpec::{hidden, name, compose_prompt, resolved_agent, allows_spawning}` (`ninox_core::worker_type`, already `pub`); `resolve_success_criteria` (private fn already in `cli.rs`, same crate — reusable as-is); `create_worker_worktree`, `repo_from_workspace`, `seed_worker_skills` (private fns already in `cli.rs`/`spawn_util.rs`, same crate); `ninox_ui_shared::spawn_util::{spawn_interactive_session, InteractiveSpawnParams}`.
- Produces (used by Task 6): 
  ```rust
  pub async fn spawn_standalone_worker(
      engine: Arc<Engine>,
      store: &Store,
      config: &AppConfig,
      name: String,
      workspace: String,
      worker_type: Option<String>,
      agent: ninox_core::config::AgentConfig,
      catalogue_path: String,
      prompt: String,
  ) -> anyhow::Result<Option<Vec<String>>>
  ```
  Returns `Err` for every validation failure (unknown/hidden/`Orchestrator` type, reserved/duplicate id, bad workspace with no enclosing repo) BEFORE any store write — mirrors `run_spawn`'s ordering. Returns `Ok(attach_argv)` on success (the same shape `spawn_interactive_session` returns), after upserting a `Session` row with `worker_type`/`success_criteria`/`agent_type`/`model`/`catalogue_path` set, exactly like `run_spawn`'s session stamp but attached instead of headless.

- [ ] **Step 1: Write the failing tests** (new `#[cfg(test)] mod spawn_standalone_tests` near `run_spawn`'s own tests, reusing their store/config fixtures):
  ```rust
  #[tokio::test]
  async fn rejects_hidden_or_orchestrator_type_before_any_side_effect() { /* assert store has no new session row after the Err */ }
  #[tokio::test]
  async fn rejects_unknown_type_name() { /* ... */ }
  #[tokio::test]
  async fn rejects_duplicate_session_name() { /* ... */ }
  #[tokio::test]
  async fn valid_type_stamps_worker_type_and_success_criteria_on_the_session() { /* ... */ }
  #[tokio::test]
  async fn no_type_defaults_to_implementor_like_run_spawn_does() { /* ... */ }
  #[tokio::test]
  async fn missing_workspace_under_a_git_repo_creates_a_worktree() { /* reuse run_spawn's fixture for this case if one exists */ }
  ```
- [ ] **Step 2:** `cargo test -p ninox-ui-shared spawn_standalone` — expect FAIL (function doesn't exist).
- [ ] **Step 3:** Implement, structuring the body as: (a) resolve+validate the type exactly as `run_spawn`'s lines 1720–1786 do (lookup, hidden/Orchestrator rejection — no `can_spawn` check here, since there is no caller session for a human/TUI-initiated standalone spawn); (b) reserved/duplicate id guard (mirrors `run_spawn`'s and the GUI Standalone path's); (c) workspace resolution — reuse the GUI's exact fallback (`spawn_util::expand_tilde` + `find_repo_root` + `create_worktree_at`) rather than `run_spawn`'s `create_worker_worktree` (the latter assumes the dir already exists), since a human-facing Standalone spawn must support typing a not-yet-created path; (d) build+upsert the `Session` (mirror GUI's Standalone `Session` literal, but set `worker_type`/`success_criteria` from the resolved type instead of hardcoding `None`/`Vec::new()`); (e) `seed_worker_skills`; (f) call `spawn_interactive_session` with `InteractiveSpawnParams{ orchestrator_id: None, extra_env: Vec::new(), .. }` (unattached, like GUI's Standalone).
- [ ] **Step 4:** Run tests — expect PASS.
- [ ] **Step 5:** `cargo fmt && cargo clippy -p ninox-ui-shared --all-targets -- -D warnings`.
- [ ] **Step 6: Commit:** `git add crates/ninox-ui-shared/src/cli.rs && git commit -m "feat(ui-shared): spawn_standalone_worker — an interactive standalone spawn with --type"`.

### Task 6: Wire `perform()`'s `Action::Spawn` arm

**Files:**
- Modify: `crates/ninox-tui/src/tui/mod.rs:759-781,820-827`

**Interfaces:**
- Consumes: `Action::Spawn` (Task 3); `spawn_orchestrator_common` (Task 4); `spawn_standalone_worker` (Task 5).

- [ ] **Step 1: Write the failing test** — a `perform()`-level test exercising both branches if `mod.rs` already has a test harness for `perform()` (check for existing `Action::Connect`/`Action::Kill` tests in `mod.rs` or its test module first and mirror that harness exactly; if none exists at this level — i.e. `perform()` is only exercised indirectly today — skip a dedicated test here and rely on Tasks 2–5's unit coverage plus the PR's manual/screenshot check, noting that explicitly in the PR body rather than inventing a new test harness for one call site).
- [ ] **Step 2:** Implement the match arm:
  ```rust
  Action::Spawn { kind, name, workspace, worker_type, harness, model, catalogue_idx, prompt } => {
      st.notify(Level::Info, format!("spawning {name}…"));
      let store = Arc::clone(store);
      let engine = Arc::clone(engine);
      lp.pending_spawns.push(tokio::spawn(async move {
          let config = AppConfig::load().unwrap_or_default();
          let catalogue_path = config.catalogue_options().into_iter().nth(catalogue_idx)
              .map(|c| c.path.to_string_lossy().to_string())
              .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());
          match kind {
              SpawnKind::Orchestrator => {
                  let agent = ninox_core::config::AgentConfig { harness, model };
                  ninox_ui_shared::cli::spawn_orchestrator_common(&store, &config, &name, prompt, None, Some(agent), Some(catalogue_path)).await.map(|_| ())
              }
              SpawnKind::Standalone => {
                  let agent = ninox_core::config::AgentConfig { harness, model };
                  ninox_ui_shared::cli::spawn_standalone_worker(engine, &store, &config, name, workspace.unwrap_or_default(), worker_type, agent, catalogue_path, prompt.unwrap_or_default()).await.map(|_| ())
              }
          }
      }));
  }
  ```
  (Match `drain_finished_spawns`'s existing `Result<(), E>`-folding shape — adjust the closure's return type to whatever `SpawnHandle`/`pending_spawns` already expects; check `SpawnHandle`'s definition before finalizing this signature.)
- [ ] **Step 3:** `cargo build -p ninox-tui` — resolve any type mismatches against `SpawnHandle`.
- [ ] **Step 4:** `cargo test --workspace`.
- [ ] **Step 5:** `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 6: Commit:** `git add crates/ninox-tui/src/tui/mod.rs && git commit -m "feat(tui): dispatch Action::Spawn to the orchestrator/standalone core paths"`.

### Task 7: Render the new spawn modal

**Files:**
- Modify: `crates/ninox-tui/src/tui/view.rs` (locate the current `Modal::Spawn` draw arm — likely near `draw_inspector`/other modal-draw functions; grep `Modal::Spawn` in `view.rs` first)

**Interfaces:**
- Consumes: `spawn::{SpawnForm, Field, FieldId, visible}` (Task 2); `ninox_ui_shared::spawn_form::{static_prior, estimate_text}`, `Store::cost_samples` (already used elsewhere in `view.rs`/`mod.rs` — match its existing call shape).

- [ ] **Step 1:** Identify the current spawn-modal draw function and its signature (it likely takes `&SpawnModal` or similar — check callers in the main `draw`/`render` dispatcher).
- [ ] **Step 2:** Implement: render each `visible` field as a `label: value` row (highlighting the focused row), a `Choice` field showing `‹ value ›` with sibling options hinted in the help line; the cost line beneath the Prompt field using `estimate_text(static_prior(&harness_value, model.as_deref()), &store.cost_samples(&harness_value, model.as_deref()).unwrap_or_default())`; `form.error`, if set, shown in the footer in the same style existing error modals use (check `AddRemoteModal`'s error rendering in `view.rs` for the exact style to match).
- [ ] **Step 3:** `cargo build -p ninox-tui` (no new unit test for layout — ratatui rendering here isn't otherwise unit-tested per the existing codebase pattern; verified instead by this PR's required kitty screenshots).
- [ ] **Step 4:** `cargo fmt && cargo clippy -p ninox-tui --all-targets -- -D warnings`.
- [ ] **Step 5: Commit:** `git add crates/ninox-tui/src/tui/view.rs && git commit -m "feat(tui): render the spawn form's new fields and cost-estimate line"`.

### Task 8: Final verification and screenshots

- [ ] **Step 1:** `cargo test --workspace`.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 3:** Rebase onto the latest `origin/v1` (`git fetch origin v1 && git rebase origin/v1`), re-run Steps 1–2 after rebasing.
- [ ] **Step 4:** Follow `BRIEF-common.md` §7's isolation + kitty capture procedure (fresh empty `--db`, scratch `config.toml`, `env -u` every `NINOX_*`, isolated `TMUX_TMPDIR`, no `GITHUB_TOKEN`) to capture the spawn modal (both Kind states) at 200x55 and 120x40, and diff against `Spawn.dc.html`.
- [ ] **Step 5:** Open the PR (title `MLOPS-5350: feat(tui): spawn dialog parity`, body Why/Summary/Test plan, noting the architecture decisions above — the GUI-has-no-Type-field gap, and that `spawn_standalone_worker` is a new interactive (not headless) `--type` path).
