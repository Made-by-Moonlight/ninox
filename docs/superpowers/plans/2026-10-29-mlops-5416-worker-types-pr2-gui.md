# MLOPS-5416 PR2: GUI Worker Types tab

Replaces `View::WorkerTypes`'s placeholder per `WorkerTypes.dc.html` and
the ticket brief's item 6, using the `ninox_core::worker_type_cli` backend
that landed in PR1 (#265, merged).

## Visual language: content/structure from the mockup, styling from v1's shipped design system

`WorkerTypes.dc.html` predates several since-finalized decisions (no
`Orchestrator` builtin, no `can_spawn`/`hidden`/`can_manage_types` fields,
"Duplicate" instead of "Reset") and is styled in a dark hex-CSS language
that no other shipped v1 GUI tab (Fleet, Board, Brain) actually uses —
those all follow `style.rs`'s folio (serif/ink-on-paper) system instead.
Followed here too: the mockup's *content and interactions* are
authoritative (two-pane list+form, built-in/custom grouping, badges,
Save/Reset/Delete/Export/Import), its literal colors are not. Fields not
yet in the mockup (`can_spawn`, `can_manage_types`, `hidden`) are added per
the ticket brief and the orchestrator's follow-up message.

## State (`crates/ninox-gui/src/components/worker_types.rs`)

- `WorkerTypesState`: `entries: Vec<worker_type_cli::Entry>` (refreshed via
  `list()` on navigation and after every mutation), `selected: Option<String>`
  (canonical name; `None` = the "+ New type" draft), `form: WorkerTypeForm`,
  `error`/`notice: Option<String>`, `import_path: String`.
- `WorkerTypeForm`: plain editable fields mirroring `worker_type_cli::Fields`
  (`name`, `harness`, `model`, `allow`/`deny`/`ask`/`can_spawn` as
  comma-joined `String`s — same convention the CLI's own `--can-spawn`
  flag and the TUI editor already use, so the format a user types is
  consistent everywhere), `can_manage_types`/`hidden: bool`, `built_in: bool`,
  `is_new: bool`. `brief` is an `iced::widget::text_editor::Content`
  (multi-line, editable — follows `plan_panel.rs`'s `PlanEditorAction`
  pattern, the one other editable-prose field in this GUI).

## Messages (flat variants on `app::Message`, per this codebase's convention)

`WorkerTypeSelect(String)`, `WorkerTypeNewDraft`, `WorkerTypeNameChanged`,
`WorkerTypeBriefAction(text_editor::Action)`, `WorkerTypeHarnessChanged`,
`WorkerTypeModelChanged`, `WorkerTypeAllowChanged`/`DenyChanged`/
`AskChanged`/`CanSpawnChanged`, `WorkerTypeToggleCanManageTypes`,
`WorkerTypeToggleHidden`, `WorkerTypeSave`, `WorkerTypeReset`,
`WorkerTypeDelete`, `WorkerTypeExport`, `WorkerTypeImportPathChanged`,
`WorkerTypeImport`, `WorkerTypeDismissNotice`.

## Backend wiring

Every mutating call passes `caller_session_id: None` — the GUI has no
ninox-managed pane of its own, so it IS "the user" by the backend's own
contract, unconditionally authorized including for `can_manage_types`/
hidden-type edits (confirmed in PR1: `authorize_mutation` short-circuits
on `None` before any gate runs). `&state.engine.store` supplies the
`Store`; no `Task`/async needed anywhere — every other config-writing path
in this GUI (`persist_config`, `CatalogueFormConfirm`, etc.) is synchronous
too, and `worker_type_cli`'s functions already do their own disk I/O.

`WorkerTypeSave` calls `create` when `is_new`, else `edit`; both surface
the `Result`'s `Err` into `state.worker_types.error` and leave the form
open, mirroring `CatalogueFormConfirm`'s guard-and-return style — no
separate client-side validation duplicate of what the backend already
checks (name/rule syntax, `can_spawn` resolution, self-type/hidden/
can_manage_types gates — all moot here since caller is `None`, but
duplicating them client-side would drift).

Export writes to `AppConfig::worker_type_exports_dir()/<slugify(name)>.toml`
(collision-numbered past a different type's file there, mirroring
`ninox-tui`'s `export_type`/`export_path`) and reports the path in
`notice`. Import reads `import_path` from disk and calls
`worker_type_cli::import`, reporting the imported names.

## List filtering

The list uses `WorkerTypeRegistry::user_facing_names()` (via
`worker_type_cli::list()`, which already filters hidden types) — MLOPS-5415's
`prompt` type never appears, matching `ninox worker-type list`/the TUI tab.

## Out of scope

- A native file picker for Import (no `rfd`-equivalent dependency in this
  GUI yet) — a plain path text field, consistent with there being no file
  dialog infra anywhere else in `ninox-gui`.
- Migrating the TUI's own, older, simpler worker-type editor
  (`ninox-tui/src/tui/worker_types.rs`, predates `worker_type_cli`/PR1,
  has no `can_spawn`/`can_manage_types` fields) onto the shared backend —
  flagged as a follow-up, not touched here.
- Wiring a worker-type picker into the spawn modal — out of this ticket's
  scope (the tab itself, not spawn-flow integration).

## Tests

- Pure logic (group-by-built-in, comma-join/split, the Save dispatch
  choosing `create` vs `edit`) unit-tested the same way `brain_list.rs`
  splits sort/filter into free functions tested apart from the `Element`
  builder.
- `App::apply` tests for each message, following this file's existing
  `CatalogueFormConfirm`-style test shape: build a minimal `App`, apply,
  assert on `state.worker_types`/`state.view`.
- Visual check per `BRIEF-common.md`'s isolated-instance process: scratch
  `--db`/config, JXA window capture, compared against `WorkerTypes.dc.html`
  with deviations listed (expected: the color/typography system, the added
  `can_spawn`/`can_manage_types`/`hidden` fields, Reset instead of
  Duplicate, Orchestrator in the built-in list, no "drafted, not saved"
  ghost section — no backend primitive for that exists).
