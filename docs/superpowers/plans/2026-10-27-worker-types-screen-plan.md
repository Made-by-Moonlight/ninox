# Worker Types TUI Screen — Implementation Plan

MLOPS-5310. Implements the "Management UI" section of
`docs/superpowers/specs/2026-10-22-worker-types-backend-design.md` and the
"Worker types" section of
`docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`, as a
`ninox-tui` screen only (no GUI work — out of scope for this ticket). The
worker-type registry, storage and spawn-time resolution already landed in
#208 (`crates/ninox-core/src/worker_type.rs`, merged to `v1`).

Scope is deliberately narrow: a new, independent tab. Does not touch
`tui/graph.rs` or the "ninox prompt" (other workers are on those in
parallel this cycle) — the prompt-driven "draft a type from natural
language" creation path from the vision spec is explicitly left for
whichever follow-up lands prompt support.

## What already exists (don't rebuild)

- `WorkerTypeSpec` / `WorkerTypeRegistry` / `WorkerPermissionsConfig` —
  `crates/ninox-core/src/worker_type.rs`.
- `AppConfig::worker_types: Vec<WorkerTypeSpec>` and
  `AppConfig::worker_type_registry()` — `crates/ninox-core/src/config.rs`.
- `WorkerTypeSpec::to_toml_snippet` / `from_toml_snippet` — export/import
  serialization, already implemented and tested against the TOML format
  this spec's "left to the implementation plan" open question resolves to
  (matching `config.toml`'s own format, per the vision spec's own
  guess — no new format invented).
- The Settings tab's `Field`/`Kind`/editing-buffer pattern
  (`crates/ninox-tui/src/tui/settings.rs`) — reused directly, not
  reimplemented, for the per-type form (toggle/choice/text fields with an
  inline edit buffer, Enter-to-commit, auto-save on every change).
- The Brain tab's suspend-for-`$EDITOR` round trip (`edit_suspended` in
  `tui/mod.rs`, used today for new/edited brain entries) — reused for
  Import.

## Open design choices (spec left these to the implementation plan)

- **List+form interaction model**: two focus regions — a left name list
  (builtins and custom/customized types, alphabetical) and a right field
  form for whichever name is selected — toggled by Tab, mirroring the
  existing Spawn modal's name/prompt Tab toggle
  (`state.rs::spawn_modal_key`). A synthetic trailing "+ New type" list row
  opens a blank draft.
- **Permission "chips"**: the vision spec's GUI mockup calls these
  "toggleable permission chips", but the backend stores `allow`/`deny`/
  `ask` as open-ended Claude-Code rule strings, not a closed set — there's
  nothing to toggle between. The TUI form exposes `enabled` as a toggle
  field and `allow`/`deny`/`ask` as comma-separated text fields (split/join
  on `,`, trimmed, empty segments dropped). Most spec-consistent option
  available given the existing data shape.
- **Auto-save granularity**: every field edit writes through immediately
  (read config → apply → write → reload), exactly like the Settings tab —
  no separate "Save" button, so the two config-writing screens in this app
  behave identically.
- **Delete semantics**: deleting removes the type's `[[worker_types]]`
  config entry. For a customized builtin this reverts it to the compiled-in
  default (still listed); for a pure custom type this removes it from the
  registry entirely. A name with no config entry (an un-customized
  builtin) has nothing to delete — the key is a no-op with a notice, not an
  error, matching `toggle_harness`'s "locked" no-op shape.
- **Export destination**: no prior art for a Ninox TUI export file.
  `AppConfig::worker_type_exports_dir()` follows the existing
  `sessions_dir()`/`ninox_bin_dir()` convention:
  `<config_dir>/ninox/worker-types/<slug(name)>.toml`. The save path is
  shown in the footer notice.
- **Import collision**: rejected with a clear error (no silent overwrite),
  consistent with `WorkerTypeSpec::from_toml_snippet`'s existing
  fail-closed validation style.

## Changes

### `crates/ninox-core/src/config.rs`

- `AppConfig::upsert_worker_type(&mut self, spec: WorkerTypeSpec)` — insert
  or replace by name in `self.worker_types` (mirrors `toggle_harness`'s
  whole-entry-replace).
- `AppConfig::remove_worker_type(&mut self, name: &str) -> bool` — removes
  the config entry if present; `false` (no-op) if absent.
- `AppConfig::worker_type_exports_dir() -> PathBuf`.
- Tests alongside the existing `config.rs` test module.

### `crates/ninox-tui/src/tui/worker_types.rs` (new)

Pure model + I/O, same shape as `settings.rs`:

- `FieldId`: `Name, Brief, Harness, Model, PermEnabled, PermAllow,
  PermDeny, PermAsk`.
- Reuses `settings::Kind` (`Toggle`/`Choice`/`Text`) — no duplicate enum.
- `Field`, `Change` (`Step`/`Set`) — same shape as `settings.rs`.
- `WorkerTypesView`: `names: Vec<String>`, `custom: HashSet<String>`
  (which names have a config entry — drives the list's builtin/custom
  badge), `list_selected: usize` (last index = "+ New type"), `on_list:
  bool` (Tab-toggled focus), `fields: Vec<Field>`, `field_selected: usize`,
  `editing: Option<String>`, `error`, `load_error`, `notice: Option<String>`
  (export path / import result).
- `fields(spec: &WorkerTypeSpec, harness_names: &[String]) -> Vec<Field>`.
- `apply`/`set` — mirrors `settings.rs::apply`/`set`, but keyed off one
  `WorkerTypeSpec` draft rather than the whole `AppConfig`.
- `reload`/`load`/`save` wrappers — read → apply → write → re-derive
  `names`/`custom` from the fresh config, same pattern as
  `settings::{load, save}`.
- `new_type(cfg, name) -> Result<WorkerTypeSpec, String>` — rejects an
  empty or colliding name, else upserts a blank spec.
- `delete_type(cfg, name) -> Result<bool, String>`.
- `export_type(spec) -> Result<PathBuf, String>` — writes
  `to_toml_snippet()` under `worker_type_exports_dir()`.
- `import_type(cfg, src) -> Result<WorkerTypeSpec, String>` — parses via
  `from_toml_snippet`, rejects a colliding name, upserts.
- Unit tests for all of the above against a temp `NINOX_CONFIG`/dir, plus
  pure-function tests for `fields`/`apply` with no disk I/O (mirroring
  `settings.rs`'s split between pure and I/O tests).

### `crates/ninox-tui/src/tui/state.rs`

- `View::WorkerTypes` variant.
- `TuiState.worker_types: worker_types::WorkerTypesView`.
- `switch_view` arm: `Action::LoadWorkerTypes`.
- `tab_key`'s range `'1'..='6'` → `'1'..='7'`; `command()`'s
  `KeyCode::Char('1'..='6')` guard likewise; add a bare `'w'` key in
  `command()` (parity with `p`/`b`/`s`) switching to `View::WorkerTypes`.
- `worker_types_key` / `worker_types_mouse` functions, modeled directly on
  `settings_key`/`settings_mouse` plus the extra list-focus dimension (Tab
  toggles `on_list`; j/k/arrows move the list when `on_list`, the field
  cursor otherwise; `n` new, `D` delete (confirm modal via
  `Pending::DeleteWorkerType(String)`), `x` export, `i` import).
- New `Action` variants: `LoadWorkerTypes`, `SaveWorkerType(Change)`,
  `NewWorkerType(String)`, `DeleteWorkerType(String)`,
  `ExportWorkerType(String)`, `ImportWorkerType`.
- New `Pending::DeleteWorkerType(String)` + its `confirmed()`/
  `confirm_offers_all` arms.
- Extend the `typing` guard (text-entry suppresses tab-switch digits) with
  `st.view == View::WorkerTypes && st.worker_types.editing.is_some()`.
- `HELP` constant: new "Worker types tab (bare keys)" section; update the
  "1-6 (p b s v)" line to "1-7 (p b s v w)".

### `crates/ninox-tui/src/tui/layout.rs`

- `TABS` grows to `[(View, &str); 7]`, adding `(View::WorkerTypes,
  "Worker Types")` — short label, keep the existing narrow-terminal tab
  budget test (`the_graph_tab_does_not_silently_vanish_at_80_to_90_columns`)
  passing; shorten the label further if 80-column width drops it.
- `WorkerTypesLayout` struct (list `Rect`, fields `Rect`, help `Rect`,
  action-bar `Rect` for New/Delete/Export/Import hints) +
  `fn worker_types(body: Rect, st: &TuiState) -> WorkerTypesLayout`,
  modeled on `fn settings()`.
- Wire into the view-dispatch match (next to `View::Settings =>
  lay.settings = ...`) and the `header_tabs_are_laid_out_in_order_without_
  overlap` test's expected `views` array.

### `crates/ninox-tui/src/tui/view.rs`

- `draw_worker_types` (list column + form column + help/notice line),
  modeled on `draw_settings`.
- Header tab label match, footer pill match, footer hints match — new
  `View::WorkerTypes` arms.
- Add `View::WorkerTypes` to the `every_view_renders_at_tiny_and_standard_
  sizes` sweep.

### `crates/ninox-tui/src/tui/mod.rs`

- `Action::LoadWorkerTypes => st.worker_types.reload(worker_types::load())`.
- `Action::SaveWorkerType(change) => ...` — read→apply→write→reload→notify,
  mirroring `Action::SaveSetting`.
- `Action::NewWorkerType` / `DeleteWorkerType` / `ExportWorkerType` /
  `ImportWorkerType` handlers; `ImportWorkerType` suspends the terminal via
  `edit_suspended` on a blank templated buffer (a comment explaining the
  expected `[[worker_types]]` snippet shape), then parses/upserts.

## Testing

- `cargo test -p ninox-core` — `upsert_worker_type`/`remove_worker_type`/
  `worker_type_exports_dir`.
- `cargo test -p ninox-tui` — new `worker_types.rs` unit tests (pure +
  temp-dir I/O), `state.rs` key/mouse reducer tests (list nav, Tab focus
  toggle, field edit/save, delete confirm flow, new-type collision
  rejection), `layout.rs`/`view.rs` tab-budget and render-sweep tests.
- `cargo fmt`, `cargo clippy --workspace --all-targets -D warnings`,
  `cargo test --workspace` must all pass before pushing (not run wholesale
  `cargo fmt` repo-wide — only touched files, see CLAUDE.md).

## Explicitly out of scope (hand to `ninox request-work` if raised)

- GUI (`ninox-gui`) worker-type management screen.
- Natural-language type drafting via the ninox prompt.
- `ninox worker-type` CLI subcommands (export/import/list) — the TUI reads
  and writes `AppConfig` directly.
