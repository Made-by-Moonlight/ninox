# MLOPS-5350: TUI Brain Sources view

Brief: `BRIEF-MLOPS-5350-tui-sources.md`. Design: `BrainSources.dc.html`. GUI
reference: `crates/ninox-gui/src/components/brain_sources.rs` (#278).

## 1. Shared source logic (`ninox-ui-shared/src/brain_sources.rs`)

Move out of the GUI, unchanged in behaviour, with their tests:

- `SourceType` (`from_remote`, `label`), `SourceStatus`, `SourceEditForm`,
  `form_from_catalogue`, `build_catalogue_ref`, `format_ago`.

New shared helpers both UIs call:

- `Health` + `health(status)`: failing iff the last sync errored.
- `pending_pushes(status)`, `cadence_label`, `sync_label(c, status, now)` —
  the GUI card's "local only" / "synced X · cadence" / "never synced · cadence".
- `check_name_free(existing, name, editing)` and `prepare_save(form, existing)`
  (validate, unique name, create the cache dir, initialise the index).
- `apply_save` / `apply_toggle` / `apply_remove`: pure `&mut AppConfig`
  mutations, always run inside `AppConfig::update` (the #292 guarded path).
- `load_status(c)`, `sync_source(config, path)`, `sync_log_line(name, report)`.
- Secrets: `redact_credentials(text)` masks URL userinfo and credential-like
  query values; `SourceField { label, value, secret }` with `shown()` rendering
  a secret as `set`/`unset`, everything else through `redact_credentials`.
  `CatalogueRef` has no secret fields today, so nothing is flagged secret yet.

The GUI's `brain_sources.rs` and `app.rs` handlers switch to these.

## 2. TUI (`ninox-tui/src/tui/sources.rs` + wiring)

- `BrainView.sources: SourcesView` (open, cursor, detail, catalogues, status,
  log). `S` in the Brain tab opens it and fires `Action::LoadBrainSources`.
- Keys inside: `j/k` (`g/G`) move, `↵` toggles the detail pane, `a` add,
  `e` edit, `d` remove (confirm via `Pending::RemoveBrainSource`), `t`
  enable/disable, `s` sync selected, `S` sync all, `esc` closes the detail pane
  then the sub-view, `q/b` leave it.
- Form: `Modal::SourceForm(SourceForm)` — the GUI's fields (Name, Type, location,
  S3 Region/Endpoint, Local cache, Sync, Access). Tab/↑↓ move, ←/→ step a
  choice, ↵ validates then emits `Action::SaveBrainSource`, esc cancels. A save
  failure reopens the form with the error.
- Actions run through `sources::save` / `toggle` / `remove` (each one
  `AppConfig::update` call) and `sources::sync` (off the loop, results on a new
  channel; a successful sync rebuilds that catalogue's index and reloads).
- Rendering: one table row per source (health dot, name, kind, sync state,
  pending pushes, last error), disabled rows dimmed, detail pane on the right,
  recent sync activity below. Every value goes through `SourceField::shown` /
  `redact_credentials`; nothing is horizontally scrolled — columns truncate.
- Footer hints and `HELP`: `S` in the Brain section, a new "Brain › Sources"
  section for the sub-view keys (`S` there = sync all).

## 3. Tests

- Shared: moved GUI tests, health/sync label, name check, apply_* mutations,
  redaction, `shown()` masking.
- TUI: key routes (open/close, every action key, `c` untouched), form state
  machine (focus order per type, choice stepping, validation error, submit),
  rendered output never contains a credential, and a save through
  `sources::save` under `isolate_with_config` (refuses an unparseable file and
  leaves it byte-identical; writes `.bak` otherwise).
