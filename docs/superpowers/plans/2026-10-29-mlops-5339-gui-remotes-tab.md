# MLOPS-5339 follow-up: GUI Remotes tab (PR 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the GUI's `View::Remotes` placeholder with full TUI parity: a list of configured machines with health, an "Add remote" flow that drives the real interactive `ninox machine add` subprocess inside an embedded terminal, and a "Remove remote" flow with confirm — in v1 dark/sans styling.

**Architecture:** Move the TUI's inline `RemoteRow`/`collect` join logic into `ninox-ui-shared` so the GUI can read the same data `ninox_ui_shared::machine::remove` already lives in. Reuse the GUI's existing session-detail terminal machinery (`TerminalState`, `AttachedClient`, `TerminalWidget`, the generic `Message::TerminalWrite`/`Event::ClientClosed` handlers) to host `machine add` as a real child process inside an iced canvas, keyed by a synthetic session id instead of a real session's id — this is the only flow in the app that needs a live PTY without a backing ninox session.

**Tech stack:** Rust, iced, the existing `alacritty_terminal`-backed `TerminalState`, `ninox-core`, `ninox-ui-shared`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-common.md` (process, testing rules) and `BRIEF-MLOPS-5339-gui-parity.md` item 1 ("Remotes tab").

## Global Constraints

- v1 dark/sans `shell::tokens` styling only (brief: "the shipped v1 screens... Folio/Field Notes styling... must not appear anywhere you touch") — match `worker_types.rs`'s token set, not `spawn_modal.rs`/`settings_panel.rs`'s older `crate::style` system.
- Never auto-accept an SSH host-key prompt (orchestrator-approved read-back) — the embedded terminal must pass the real `ssh`/`machine add` prompts through untouched.
- PR #276 (leftover workspaces) is open, not merged, as of branch creation — don't build its chip/list; don't lay out the row in a way that would need rework to add it later (leave a trailing-content slot).
- **Deviation from the brief, flagged via `ninox request-work`:** the brief's item 1 asks for "version and last-seen" columns, citing them as "the same as the TUI's `view.rs:1664-1697`". As of this branch's `origin/v1` base, neither field exists anywhere in that range, in `RemoteRow`, `MachineProfile`, or `Store::machine_health()` — the TUI's actual row is health dot + label + host + remote session only, and no last-seen timestamp or probed-version string is persisted by the health poller (`poll_remote_machine_health` tracks only internal backoff state). This plan ports the row that actually exists; adding version/last-seen would mean new core persistence (a poller-maintained `last_seen`/`version` per machine), which is out of this task's scope per the worker brief's "discover additional work -> `ninox request-work`, don't do it here" rule.
- Branch `v1-mlops-5339-gui-parity-1` off a fresh `origin/v1`; PR targets `v1`; title `MLOPS-5339: feat(gui): remotes tab`; body `## Why` / `## Summary` / `## Test plan`.
- TDD; `cargo fmt` only on touched files (never wholesale); `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace` must pass before push.
- Window-only screenshots from an isolated instance (fresh `--db`, scratch config/brain, `env -u` every `NINOX_*` var) before reporting done, per `BRIEF-common.md` §7.

## Review Focus

1. **Empty machine list** — must render the TUI's "No remote machines configured" equivalent, not a blank panel.
2. **`machine add` subprocess exits immediately** (bad host, no network) — the pane must still show the process's output and the tab must not get stuck believing the add is still running.
3. **Removing a stale id** (a profile removed by a concurrent edit before the confirm fires) — `machine::remove` returns `Err`; the GUI must surface it, not panic or silently no-op.
4. **Navigating away from Remotes mid-`machine add`** — the spawned subprocess must be killed (not orphaned) and its terminal state dropped.
5. **Submitting "Add remote" with an empty/whitespace host** — rejected client-side with an inline error, mirroring the TUI's `add_remote_modal_key` validation, never spawns a subprocess.

---

## Task 1: Move `RemoteRow`/`collect` into `ninox-ui-shared`

**Files:**
- Modify: `crates/ninox-ui-shared/src/machine.rs` (add `RemoteRow` struct + `collect_rows` fn)
- Modify: `crates/ninox-tui/src/tui/remotes.rs` (delete the moved code, re-export)
- Test: `crates/ninox-ui-shared/src/machine.rs` (new `#[cfg(test)]` module, or existing one)

**Interfaces:**
- Produces: `pub struct RemoteRow { pub id: String, pub label: String, pub ssh_target: String, pub remote_session: String, pub enabled: bool, pub health: Option<ninox_core::types::MachineHealth> }` and `pub fn collect_rows(cfg: &AppConfig, health: &std::collections::HashMap<String, MachineHealth>) -> Vec<RemoteRow>` in `ninox_ui_shared::machine` — Task 2 imports both.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn collect_rows_joins_health_by_id_and_preserves_config_order() {
    let mut cfg = AppConfig::default();
    cfg.remote_machines.machines = vec![
        MachineProfile { id: "a".into(), label: "A".into(), ssh_target: "a@host".into(), remote_session: "default".into(), enabled: true },
        MachineProfile { id: "b".into(), label: "B".into(), ssh_target: "b@host".into(), remote_session: "default".into(), enabled: false },
    ];
    let mut health = std::collections::HashMap::new();
    health.insert("a".to_string(), MachineHealth::Connected);

    let rows = collect_rows(&cfg, &health);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "a");
    assert_eq!(rows[0].health, Some(MachineHealth::Connected));
    assert_eq!(rows[1].id, "b");
    assert_eq!(rows[1].health, None);
    assert!(!rows[1].enabled);
}
```

(`MachineHealth` needs `PartialEq` for this assertion — add it to the derive list in `ninox-core/src/types.rs:631` if not already present; it currently derives `Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq` per the existing source, so no change should be needed — confirm before writing the test.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-ui-shared collect_rows_joins_health_by_id_and_preserves_config_order`
Expected: FAIL (`collect_rows`/`RemoteRow` not defined in this crate).

- [ ] **Step 3: Implement `RemoteRow` and `collect_rows` in `ninox-ui-shared/src/machine.rs`**

Move the struct and function bodies verbatim from `ninox-tui/src/tui/remotes.rs`'s `RemoteRow`/`collect` (join `cfg.remote_machines.machines` against `health` by `m.id`, same field mapping), renaming `collect` to `collect_rows`. Add the needed `use ninox_core::types::MachineHealth;` and `use std::collections::HashMap;` to this file.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox-ui-shared collect_rows_joins_health_by_id_and_preserves_config_order`
Expected: PASS

- [ ] **Step 5: Re-export from the TUI and confirm its own tests still pass**

In `crates/ninox-tui/src/tui/remotes.rs`, delete the moved `RemoteRow` struct and `collect` fn, replacing them with `pub use ninox_ui_shared::machine::{RemoteRow, collect_rows as collect};` — the `RemotesView` selection-state struct stays in this file (it's TUI-local UI state, not shared data logic). No call-site changes are needed elsewhere in `ninox-tui` since the re-export keeps the name `collect`.

Run: `cargo test -p ninox-tui`
Expected: PASS (existing remotes-related tests, if any, keep passing against the re-exported path).

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-ui-shared/src/machine.rs crates/ninox-tui/src/tui/remotes.rs
git commit -m "refactor(ui-shared): move RemoteRow/collect_rows for GUI reuse"
```

---

## Task 2: GUI Remotes list view (read-only)

**Files:**
- Create: `crates/ninox-gui/src/components/remotes.rs`
- Modify: `crates/ninox-gui/src/app.rs` (new `RemotesState` field, `View::Remotes` dispatch, `NavigateRemotes` handler, new `RemotesRefresh` message, `refresh_remotes` helper)
- Test: `crates/ninox-gui/src/app.rs` test module

**Interfaces:**
- Consumes: `ninox_ui_shared::machine::{RemoteRow, collect_rows}` (Task 1), `state.engine.store.machine_health() -> Result<HashMap<String, MachineHealth>>`, `ninox_core::config::AppConfig::load()`.
- Produces: `pub struct RemotesState { pub rows: Vec<RemoteRow>, pub remove_confirm: Option<String>, pub add: Option<AddRemoteForm>, pub notice: Option<Result<String, String>> }` (Tasks 3/4 fill in `remove_confirm`/`add`; leave them `None`-only here), `pub fn remotes_view(app: &App) -> Element<'_, Message>`, `App::refresh_remotes(state: &mut App)` (private helper) — Tasks 3-5 call this after mutating config.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn navigating_to_remotes_populates_rows_from_config() {
    let e = test_engine();
    let mut cfg = AppConfig::load().unwrap_or_default();
    cfg.remote_machines.machines = vec![MachineProfile {
        id: "a".into(), label: "A".into(), ssh_target: "a@host".into(),
        remote_session: "default".into(), enabled: true,
    }];
    AppConfig::update(|c| { c.remote_machines.machines = cfg.remote_machines.machines.clone(); Some(()) }).unwrap();

    let m = base(e);
    let (m, _) = m.update(Message::NavigateRemotes);

    assert!(matches!(m.view, View::Remotes));
    assert_eq!(m.remotes.rows.len(), 1);
    assert_eq!(m.remotes.rows[0].id, "a");
}
```

Use whatever existing test fixture this file already uses for an isolated `AppConfig` (check the top of the test module for how other config-touching tests avoid the real user config path — e.g. a scratch `NINOX_CONFIG`/temp dir fixture — and follow that, not the real one).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-gui navigating_to_remotes_populates_rows_from_config`
Expected: FAIL (`m.remotes` field doesn't exist yet).

- [ ] **Step 3: Implement `RemotesState`, `remotes_view`, and wire navigation**

In `components/remotes.rs`: define `RemotesState` (per Interfaces above, `#[derive(Debug, Clone, Default)]`, `AddRemoteForm` can be a unit placeholder struct for now — Task 4 fills it in) and `pub fn remotes_view(app: &App) -> Element<'_, Message>` following `worker_types_view`'s structure and token set: a toolbar (title + "+ Add remote" button, wired to a `Message::RemoteAddOpen` that Task 4 implements — add the message variant now so this compiles, with a no-op `Task::none()` handler for now if Task 4 hasn't landed its real one yet — actually: since this is one PR landing sequentially task-by-task in one branch, just leave the button's `on_press` unwired (disabled or omitted) until Task 4 adds it, to avoid a dangling message variant with no handler) and a list of rows: health dot + `label` (+ `(disabled)` suffix when `!enabled`) + `ssh_target` + `remote_session`, or the empty-state text `"No remote machines configured"` when `rows.is_empty()`. Reuse whatever existing `shell::tokens` constant the Fleet/CI views already use for connected/reconnecting/unreachable status colors (grep for the token names used by existing health/status rendering before inventing new ones) — `None` health renders faint/muted, matching the TUI's `○` vs `●` distinction (TUI: `view.rs:1650-1656`).

In `app.rs`: add `pub remotes: crate::components::remotes::RemotesState,` to `App`'s struct and its `Default`/test-constructor sites; add `Message::RemotesRefresh`; change the `View::Remotes => placeholder(...)` arm to `View::Remotes => remotes_view(state)`; add a private `fn refresh_remotes(state: &mut App)` that does `state.remotes.rows = ninox_ui_shared::machine::collect_rows(&AppConfig::load().unwrap_or_default(), &state.engine.store.machine_health().unwrap_or_default());` and call it from both the `Message::NavigateRemotes` handler (after `state.clients.clear()`) and a new `Message::RemotesRefresh => { Self::refresh_remotes(state); Task::none() }` arm.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox-gui navigating_to_remotes_populates_rows_from_config`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-gui/src/components/remotes.rs crates/ninox-gui/src/app.rs
git commit -m "feat(gui): render the Remotes tab as a read-only machine list"
```

---

## Task 3: Remove remote with inline confirm

**Files:**
- Modify: `crates/ninox-gui/src/components/remotes.rs` (confirm row rendering)
- Modify: `crates/ninox-gui/src/app.rs` (3 new messages + handlers)
- Test: `crates/ninox-gui/src/app.rs` test module

**Interfaces:**
- Consumes: `ninox_ui_shared::machine::remove(id: &str) -> anyhow::Result<()>` (already exists, Task 1 untouched), `App::refresh_remotes` (Task 2).
- Produces: `Message::RemoteRemoveRequest(String)`, `Message::RemoteRemoveConfirm`, `Message::RemoteRemoveCancel` — no other task depends on these.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn remote_remove_confirm_removes_profile_and_sets_success_notice() {
    // seed one MachineProfile "a" into a scratch AppConfig (same fixture as Task 2's test)
    let (m, _) = base(e).update(Message::NavigateRemotes);
    let (m, _) = m.update(Message::RemoteRemoveRequest("a".into()));
    assert_eq!(m.remotes.remove_confirm, Some("a".into()));
    let (m, _) = m.update(Message::RemoteRemoveConfirm);
    assert!(m.remotes.remove_confirm.is_none());
    assert!(m.remotes.rows.is_empty());
    assert!(matches!(m.remotes.notice, Some(Ok(_))));
}

#[test]
fn remote_remove_confirm_missing_id_sets_failure_notice() {
    let (m, _) = base(e).update(Message::NavigateRemotes); // empty config, no machine "ghost"
    let (m, _) = m.update(Message::RemoteRemoveRequest("ghost".into()));
    let (m, _) = m.update(Message::RemoteRemoveConfirm);
    assert!(matches!(m.remotes.notice, Some(Err(_))));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-gui remote_remove_confirm`
Expected: FAIL (messages don't exist).

- [ ] **Step 3: Implement the remove flow**

Add the three `Message` variants. Handlers: `RemoteRemoveRequest(id) => state.remotes.remove_confirm = Some(id)`; `RemoteRemoveCancel => state.remotes.remove_confirm = None`; `RemoteRemoveConfirm => { if let Some(id) = state.remotes.remove_confirm.take() { state.remotes.notice = Some(ninox_ui_shared::machine::remove(&id).map(|()| format!("removed {id}")).map_err(|e| e.to_string())); Self::refresh_remotes(state); } }`. In `remotes_view`, when `remove_confirm == Some(row.id)`, render an inline "Remove `{label}`? This won't touch the remote machine." strip with Confirm/Cancel buttons in place of that row's normal actions; otherwise render a "Remove" button per row dispatching `RemoteRemoveRequest(row.id.clone())`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-gui remote_remove_confirm`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-gui/src/components/remotes.rs crates/ninox-gui/src/app.rs
git commit -m "feat(gui): remove remote machines with an inline confirm"
```

---

## Task 4: Add remote modal — form, validation, and the embedded-terminal subprocess

**Files:**
- Modify: `crates/ninox-gui/src/components/remotes.rs` (`AddRemoteForm`, `machine_add_argv`, `add_remote_modal`)
- Modify: `crates/ninox-gui/src/app.rs` (5 new messages + handlers, modal-stack wiring, Esc wiring)
- Test: `crates/ninox-gui/src/components/remotes.rs` (pure argv test), `crates/ninox-gui/src/app.rs` test module (validation + spawn-plumbing tests)

**Interfaces:**
- Consumes: `ninox_core::hooks::canonical_exe() -> std::io::Result<PathBuf>`, `ninox_core::client::AttachedClient::spawn(engine, session_id, argv, cols, rows, generation) -> Result<AttachedClient>`, `crate::components::terminal::TerminalState::new(cols, rows, Some(client.input_sender()))`, `crate::components::terminal::TerminalWidget` (constructed with `session_id: remotes::MACHINE_ADD_SESSION_ID.to_string()` — its own `canvas::Program::update` already emits `Message::TerminalWrite { session_id, bytes }` for keyboard input, and the existing generic `Message::TerminalWrite` handler in `app.rs` already forwards those bytes to `state.clients.get(&session_id)`, so no new keyboard-handling code is needed here).
- Produces: `pub const MACHINE_ADD_SESSION_ID: &str = "__ninox_machine_add__";`, `pub struct AddRemoteForm { pub host: String, pub label: String, pub error: Option<String>, pub running: bool }` (replacing Task 2's placeholder), `pub fn machine_add_argv(exe: &Path, host: &str, label: Option<&str>) -> Vec<String>` (pure; Task 5 does not need it), a private `App::spawn_machine_add(state: &mut App, argv: Vec<String>) -> Result<(), String>` that Task 5's tests also call directly with a trivial argv.

- [ ] **Step 1: Write the failing pure-argv test**

```rust
#[test]
fn machine_add_argv_includes_label_only_when_given() {
    let exe = std::path::Path::new("/usr/local/bin/ninox");
    assert_eq!(
        machine_add_argv(exe, "example.com", None),
        vec!["/usr/local/bin/ninox", "machine", "add", "example.com"],
    );
    assert_eq!(
        machine_add_argv(exe, "example.com", Some("prod")),
        vec!["/usr/local/bin/ninox", "machine", "add", "example.com", "--label", "prod"],
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-gui machine_add_argv_includes_label_only_when_given`
Expected: FAIL (function doesn't exist).

- [ ] **Step 3: Implement `machine_add_argv` and `AddRemoteForm`**

`machine_add_argv` returns `vec![exe.display().to_string(), "machine".into(), "add".into(), host.into()]`, appending `"--label"` and `label` only when `label` is `Some` and non-empty after trimming. Replace Task 2's placeholder `AddRemoteForm` with the real fields.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox-gui machine_add_argv_includes_label_only_when_given`
Expected: PASS

- [ ] **Step 5: Write the failing validation test**

```rust
#[test]
fn remote_add_submit_rejects_blank_host() {
    let (m, _) = base(e).update(Message::RemoteAddOpen);
    let (m, _) = m.update(Message::RemoteAddHostChanged("   ".into()));
    let (m, _) = m.update(Message::RemoteAddSubmit);
    let form = m.remotes.add.as_ref().unwrap();
    assert!(form.error.is_some());
    assert!(!form.running);
    assert!(!m.clients.contains_key(remotes::MACHINE_ADD_SESSION_ID));
}
```

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test -p ninox-gui remote_add_submit_rejects_blank_host`
Expected: FAIL (messages don't exist).

- [ ] **Step 7: Implement the modal open/field/cancel/submit messages and spawn plumbing**

Add `Message::{RemoteAddOpen, RemoteAddHostChanged(String), RemoteAddLabelChanged(String), RemoteAddCancel, RemoteAddSubmit}`. `RemoteAddOpen => state.remotes.add = Some(AddRemoteForm::default())`. `RemoteAddHostChanged(v) => { if let Some(f) = &mut state.remotes.add { f.host = v; f.error = None; } }` (and the equivalent for `label`, no error-clearing needed). `RemoteAddCancel => { state.clients.remove(remotes::MACHINE_ADD_SESSION_ID); state.terminals.remove(remotes::MACHINE_ADD_SESSION_ID); state.remotes.add = None; }` (removing the `AttachedClient` from the map drops it, and `Drop for AttachedClient` already kills its child process — `ninox-core/src/client.rs:127`). `RemoteAddSubmit`: trim `form.host`; if empty, set `form.error = Some("Host is required".into())` and return; else resolve `canonical_exe()` (error -> `form.error`), build `argv` via `machine_add_argv`, call `Self::spawn_machine_add(state, argv)`.

Implement `fn spawn_machine_add(state: &mut App, argv: Vec<String>) -> Result<(), String>`: mirrors the existing `Message::ClientAttach` success path (app.rs ~2252-2293) but keyed by `remotes::MACHINE_ADD_SESSION_ID` instead of a real `SessionId`, using `state.terminal_cols`/`state.terminal_rows` and `state.next_client_generation` the same way, inserting into `state.clients`/`state.terminals` on success and returning `Err(e.to_string())` on `AttachedClient::spawn` failure (set on `form.error`, do not set `form.running`). On success, set `state.remotes.add.as_mut().unwrap().running = true`.

In `remotes_view`/`add_remote_modal`: follow `catalogue_modal.rs`'s backdrop+card convention. When `!form.running`: Host/Label text inputs (`Message::RemoteAddHostChanged`/`RemoteAddLabelChanged`), `form.error` shown inline, a Submit button dispatching `RemoteAddSubmit`, Cancel dispatching `RemoteAddCancel`. When `form.running`: render `iced::widget::Canvas::new(TerminalWidget { state: app.terminals.get(remotes::MACHINE_ADD_SESSION_ID).unwrap(), session_id: remotes::MACHINE_ADD_SESSION_ID.to_string(), font_size: crate::components::terminal::FONT_SIZE, terminal_bg: ..., terminal_fg: ..., cursor_color: ..., ansi: ..., session_ids: vec![] })` sized to fill the modal card (pull the exact theme field names from `session_detail.rs:545-554`'s call, substituting v1 token colors for the Field-Notes ones it uses there), with no Submit/Cancel buttons (Task 5 handles completion).

Wire the modal into `app.rs`'s `view()` stack chain (after the existing `catalogue_modal`/`restart_all_confirm` branches, since Remotes' own modal can't coexist with session-detail modals in practice — note its position in a one-line comment): `else if let Some(form) = &state.remotes.add { iced::widget::stack![base, remotes::add_remote_modal(state, form)].into() }`. Wire Esc: in the existing Esc match arm (the one already closing `catalogue_modal`), add closing `state.remotes.add` the same way `RemoteAddCancel` does (kill the client, clear the field) — and clear `state.remotes.remove_confirm` too.

- [ ] **Step 8: Run test to verify it passes**

Run: `cargo test -p ninox-gui remote_add_submit_rejects_blank_host`
Expected: PASS

- [ ] **Step 9: Write the failing spawn-plumbing test**

```rust
#[tokio::test]
async fn spawn_machine_add_inserts_client_and_terminal() {
    let e = test_engine();
    let mut m = base(e);
    m.remotes.add = Some(AddRemoteForm::default());
    let argv = vec!["sh".to_string(), "-c".to_string(), "sleep 1".to_string()];
    let result = App::spawn_machine_add(&mut m, argv);
    assert!(result.is_ok());
    assert!(m.clients.contains_key(remotes::MACHINE_ADD_SESSION_ID));
    assert!(m.terminals.contains_key(remotes::MACHINE_ADD_SESSION_ID));
    assert!(m.remotes.add.unwrap().running);
}
```

(Uses a trivial real `sh -c sleep 1` argv rather than a real `ninox machine add` invocation — `canonical_exe()` in a `cargo test` binary resolves to the test binary itself, not a real `ninox` CLI, so exercising the actual subcommand here would be meaningless; this test is only responsible for the generic spawn-and-insert plumbing, which `machine_add_argv`'s own already-tested output feeds in production.)

- [ ] **Step 10: Run test to verify it fails, then passes**

Run: `cargo test -p ninox-gui spawn_machine_add_inserts_client_and_terminal`
Expected: FAIL, then PASS once Step 7's `spawn_machine_add` exists (this step should already be satisfied by Step 7's implementation — if the test passes immediately on first run, that's fine; still run it to confirm).

- [ ] **Step 11: Commit**

```bash
git add crates/ninox-gui/src/components/remotes.rs crates/ninox-gui/src/app.rs
git commit -m "feat(gui): add remote machines via an embedded machine-add terminal"
```

---

## Task 5: Completion handling — auto-close, refresh, inline success/failure

**Files:**
- Modify: `crates/ninox-gui/src/app.rs` (`Event::ClientClosed` handler)
- Test: `crates/ninox-gui/src/app.rs` test module

**Interfaces:**
- Consumes: `App::spawn_machine_add`, `AddRemoteForm.host` (Task 4), `App::refresh_remotes` (Task 2), `ninox_core::config::AppConfig::load()`.
- Produces: `fn resolve_machine_add(state: &mut App)` (private) — a shared resolution helper used both here and by Task 2's `NavigateRemotes` handler (patched below); nothing further downstream depends on it.

**Note on a gap this task also closes:** every `Navigate*` handler already does `state.clients.clear()` on tab switch ("Off-screen sessions keep running detached; drop their view clients" — existing comment in `NavigateFleet`/`NavigateBoard`/etc.), which already kills a mid-flight `machine add` child process via `Drop for AttachedClient` the moment the user switches away from Remotes (Review Focus #4) — no new code needed for the kill itself. What's missing is resolving the now-stale `state.remotes.add` when the user comes back, which this task fixes by having `NavigateRemotes` call the same resolution helper this task defines.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn machine_add_exit_closes_pane_and_refreshes_with_success_notice() {
    let e = test_engine();
    let mut m = base(e);
    m.remotes.add = Some(AddRemoteForm { host: "a@host".into(), ..Default::default() });
    // Seed the profile the "subprocess" would have written, so the
    // post-exit existence check finds it (this test is about the
    // GUI's reaction to ClientClosed, not about `machine add`'s own logic).
    AppConfig::update(|c| {
        c.remote_machines.machines.push(MachineProfile {
            id: "a".into(), label: "a".into(), ssh_target: "a@host".into(),
            remote_session: "default".into(), enabled: true,
        });
        Some(())
    }).unwrap();
    App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "exit 0".into()]).unwrap();
    let generation = m.clients.get(remotes::MACHINE_ADD_SESSION_ID).unwrap().generation;

    let (m, _) = m.update(Message::EngineEvent(Box::new(
        ninox_core::events::Event::ClientClosed { session_id: remotes::MACHINE_ADD_SESSION_ID.into(), generation },
    )));

    assert!(m.remotes.add.is_none());
    assert!(!m.clients.contains_key(remotes::MACHINE_ADD_SESSION_ID));
    assert!(matches!(m.remotes.notice, Some(Ok(_))));
    assert_eq!(m.remotes.rows.len(), 1);
}
```

Check the exact wrapping `Message` variant `Event::ClientClosed` arrives through (the explore report shows handlers matching directly on `Event::ClientClosed { .. }` inside what is presumably an outer `Message::EngineEvent(Box<Event>)` dispatch — confirm the exact match path used by the existing `ClientClosed` tests before writing this one, and mirror it exactly).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-gui machine_add_exit_closes_pane_and_refreshes_with_success_notice`
Expected: FAIL (no Remotes-specific reaction to `ClientClosed` yet, so `m.remotes.add` stays `Some`).

- [ ] **Step 3: Implement `resolve_machine_add` and call it from `ClientClosed`**

```rust
fn resolve_machine_add(state: &mut App) {
    let Some(form) = state.remotes.add.take() else { return };
    let added = AppConfig::load()
        .map(|cfg| cfg.remote_machines.machines.iter().any(|m| m.ssh_target == form.host))
        .unwrap_or(false);
    state.remotes.notice = Some(if added {
        Ok(format!("added {}", form.host))
    } else {
        Err(format!("machine add for {} did not complete — run `ninox machine add {}` directly for details", form.host, form.host))
    });
    Self::refresh_remotes(state);
}
```

After the existing unconditional `state.clients.remove(&session_id); state.terminals.remove(&session_id); state.diffs.remove(&session_id);` in the `Event::ClientClosed` arm (app.rs, the existing generic handler — its generation check and `viewing`/reattach logic are for `SessionDetail` and must stay untouched), add:

```rust
if session_id == crate::components::remotes::MACHINE_ADD_SESSION_ID {
    Self::resolve_machine_add(state);
}
```

Place this branch so it runs regardless of the `generation` early-return already in the handler — if that early-return would skip it for a stale generation, move this block before that check (a stale/superseded machine-add client never happens in practice, since nothing re-attaches `MACHINE_ADD_SESSION_ID`, but don't rely on an accidental ordering; place it explicitly so it always runs when `session_id` matches).

- [ ] **Step 3a: Write the failing orphaned-add test**

```rust
#[tokio::test]
async fn navigating_away_and_back_resolves_an_orphaned_add() {
    let e = test_engine();
    let mut m = base(e);
    m.remotes.add = Some(AddRemoteForm { host: "a@host".into(), ..Default::default() });
    App::spawn_machine_add(&mut m, vec!["sh".into(), "-c".into(), "sleep 5".into()]).unwrap();

    // Simulate what every other Navigate* handler's existing
    // `state.clients.clear()` already does when the user switches away
    // from Remotes mid-add: the client (and its process, via Drop) is
    // gone, but `state.remotes.add` is untouched until something resolves it.
    m.clients.remove(remotes::MACHINE_ADD_SESSION_ID);
    m.terminals.remove(remotes::MACHINE_ADD_SESSION_ID);

    let (m, _) = m.update(Message::NavigateRemotes);

    assert!(m.remotes.add.is_none());
    assert!(matches!(m.remotes.notice, Some(Err(_))));
}
```

- [ ] **Step 3b: Run test to verify it fails**

Run: `cargo test -p ninox-gui navigating_away_and_back_resolves_an_orphaned_add`
Expected: FAIL (`NavigateRemotes` doesn't check for an orphaned add yet).

- [ ] **Step 3c: Patch `NavigateRemotes` to resolve an orphaned add before refreshing**

In the `Message::NavigateRemotes` handler (Task 2), before calling `Self::refresh_remotes(state)`, add:

```rust
if state.remotes.add.is_some() && !state.clients.contains_key(crate::components::remotes::MACHINE_ADD_SESSION_ID) {
    Self::resolve_machine_add(state);
}
```

- [ ] **Step 3d: Run test to verify it passes**

Run: `cargo test -p ninox-gui navigating_away_and_back_resolves_an_orphaned_add`
Expected: PASS

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox-gui machine_add_exit_closes_pane_and_refreshes_with_success_notice`
Expected: PASS

- [ ] **Step 5: Add the failure-path test and confirm it passes**

Same shape as Step 1 but without seeding a matching `MachineProfile`; assert `matches!(m.remotes.notice, Some(Err(_)))` instead.

Run: `cargo test -p ninox-gui machine_add_exit` (matches both tests)
Expected: PASS

- [ ] **Step 6: Full workspace check**

Run: `cargo fmt -- --check crates/ninox-gui/src/app.rs crates/ninox-gui/src/components/remotes.rs crates/ninox-ui-shared/src/machine.rs crates/ninox-tui/src/tui/remotes.rs` (format only the touched files, never the whole tree), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
Expected: all PASS / clean.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-gui/src/app.rs
git commit -m "feat(gui): close the machine-add pane on exit and report the result"
```

---

## After implementation

- Rebase on `origin/v1` before opening the PR (other v1 workers are landing in parallel).
- Isolated-instance visual check per `BRIEF-common.md` §7 (fresh `--db`, scratch config/brain, `env -u` every `NINOX_*` var, no synthetic mouse input) — window-only screenshot of the Remotes tab (empty state, populated list, Add-remote modal mid-flow) compared against `Legend.dc.html`/`Main.dc.html`'s remote-node styling.
- PR title: `MLOPS-5339: feat(gui): remotes tab`, body `## Why` / `## Summary` / `## Test plan`, targeting `v1`. Note the embedded-terminal design choice for `machine add` and the "success inferred from the resulting `AppConfig` rather than a subprocess exit code" decision (no exit-code plumbing exists in `AttachedClient`/`Event::ClientClosed` today, and adding it is out of scope for this PR) explicitly, since the brief asked for this choice to be justified.
