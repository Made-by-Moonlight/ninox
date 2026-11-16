# Brain: Live Updates & Reindex Progress

## Context

The GUI brain panel (`crates/ninox-app/src/components/brain_panel.rs`) reads
from an in-memory `BrainIndex` (`crates/ninox-core/src/brain.rs`) that is
only ever repopulated by `Message::BrainReindex` — fired exclusively by a
manual "Reindex" button click (`brain_panel.rs:269`). Orchestrators write
directly to the brain's Markdown files on disk (via the CLI/HTTP routes,
outside the GUI process), so the panel goes stale the moment an orchestrator
writes a new fact, until the user notices and clicks reindex.

Two problems, one root cause:

1. **No live updates.** The panel never learns that brain files changed
   underneath it.
2. **No reindex progress.** `Message::BrainReindex` currently calls
   `state.brain.rebuild(None)` *synchronously* inside `apply()`
   (`app.rs:1793`) — this blocks the update thread for the duration of the
   rebuild, and the button gives no visual indication anything is
   happening, before or during.

Both are fixed by the same underlying change: move the rebuild off the UI
thread and give it an in-flight state, then drive that same path from a
filesystem-change signal in addition to the button.

## Goal

- The brain panel reflects on-disk changes automatically, without the user
  needing to click anything, using OS-level filesystem notifications (not
  polling — this must not add any recurring background work when nothing
  has changed).
- The "Reindex" button shows a distinct in-flight state ("Reindexing…")
  while a rebuild is running, whether it was triggered by the button or by
  a detected file change.
- Neither trigger can block the UI thread or stack up concurrent rebuilds.

## Non-goals

- No changes to `ninox-server` or the CLI — both already construct their
  own `BrainIndex` and are out of scope; this is a GUI-app-only fix (see
  `Message::BrainSwitchCatalogue`'s doc comment at `app.rs:1865`, which
  already establishes that catalogue viewing is app-side and independent of
  the server's copy).
- No persistent "watching for changes" indicator anywhere in the UI (e.g. a
  pulsing dot in the folio header). The only feedback surface is the
  Reindex button's transient state.
- No user-visible error banner for rebuild failures — kept identical to
  today's behavior (`tracing::error!`, stale entries left displayed). There
  is no existing precedent for an error surface on the brain panel itself
  (only on modals), and adding one is out of scope here.
- No change to `BrainIndex::rebuild()`'s own logic, schema, or return type.

## Approach

Add `notify` + `notify-debouncer-mini` as new workspace dependencies (the
same crates already named for the planned-but-unbuilt `ninox brain index
--watch` CLI mode in `docs/specs/brain.md`), used here to watch the active
catalogue's brain directory from inside the GUI's `iced::Subscription`
instead.

This was chosen over a periodic-poll subscription (the pattern the
`db-poll` subscription already uses for session state, `app.rs:2219`)
specifically because polling would mean recurring work — a filesystem walk
or stat sweep — every tick forever, whether or not anything changed. A
`notify`-based watch is push-based: the OS (FSEvents on macOS, inotify on
Linux) wakes the app only when a real change happens, so idle cost is
zero, and it's the more responsive option besides.

## Architecture

### Subscription: `brain-watch`

`App::subscription` (`app.rs:2199`) gains a new subscription alongside
`engine_sub`/`keyboard_sub`/`poll_sub`. Unlike those, which use a static
string id, `brain-watch` is keyed on the **active catalogue's brain path**
(`state.catalogues[state.active_catalogue].path`), so switching catalogues
via `Message::BrainSwitchCatalogue` causes iced to drop the old watcher
subscription (and everything it owns — the watcher and its debounce
plumbing live entirely inside the subscription's stream, so dropping it
cleanly unregisters the OS-level watch, no leaked threads) and start a
fresh one on the new path.

Inside the stream:

1. A `notify-debouncer-mini` debouncer watches the brain path recursively,
   with a short quiet period (~400ms) — a burst of writes (e.g. an
   orchestrator writing several files in a row) collapses into one signal
   instead of one rebuild per file.
2. Events under `.index.db`, `.index.db-wal`, `.index.db-shm`, or
   `.gitignore` — the index's own write-back files, written by `rebuild()`
   itself and `ensure_gitignore` — are filtered out before they reach the
   debounce timer. Without this filter, every reindex would rewrite
   `.index.db`, which the watcher would see and use to trigger another
   reindex: an infinite loop.
3. Once the quiet period elapses with at least one real (non-filtered)
   change pending, the stream yields `Message::BrainFilesChanged`.

### Shared reindex kickoff

`Message::BrainReindex` (button click) and `Message::BrainFilesChanged`
(watch event) both route through one new helper,
`fn start_reindex(state: &mut App) -> Task<Message>`:

- If `state.brain_view.reindexing` is already `true`, set
  `state.brain_view.reindex_pending = true` and return `Task::none()` — a
  rebuild already in flight will pick up the newest on-disk state anyway,
  so a second concurrent rebuild is pure waste, not a correctness need.
- Otherwise, set `reindexing = true`, clone `state.brain` (`Arc<BrainIndex>`)
  and capture the current `active_catalogue` index, and return a
  `Task::future` that runs the rebuild via
  `tokio::task::spawn_blocking(move || brain.rebuild(None))` — the same
  backgrounding pattern already used for the embedder load in
  `main.rs:458` — resolving to
  `Message::BrainReindexed { catalogue: usize, result: Result<RebuildStats, String> }`.

### Completion: `Message::BrainReindexed`

If `catalogue` no longer matches `state.active_catalogue` (the user
switched catalogues while this rebuild was running), the result is
discarded entirely — it belongs to an abandoned view. `BrainSwitchCatalogue`
already resets `reindexing`/`reindex_pending` to `false` for the freshly
loaded catalogue, so a stale result arriving after a switch has nothing to
interact with.

Otherwise, this handler contains exactly today's post-rebuild logic
(currently the `Ok(stats) => { ... }` body of `Message::BrainReindex` in
`app.rs:1793`) unchanged: requery entries, `refresh_brain_edges`, clear a
ghost selection if the selected entry no longer resolves, and
`refresh_selection_graph`. These stay synchronous on the update thread —
they're cheap, already-fast in-process SQLite lookups against a small local
DB, not full rebuilds, so there's no benefit to backgrounding them too.

Finally: set `reindexing = false`. If `reindex_pending` was `true`, clear
it and immediately call `start_reindex` again — covers a change landing
while the previous rebuild was already running.

### UI: `reindex_btn`

`brain_panel.rs:269` becomes state-aware:

- Idle (`!reindexing`): unchanged from today — "Reindex" label, clickable,
  hover-highlighted, dispatches `Message::BrainReindex`.
- In flight (`reindexing`): label reads "Reindexing…", no `on_press` (clicks
  while running are simply ignored — the pending-flag mechanism above
  already covers "something changed while we were busy").

No spinner or animation is introduced — this codebase has no existing
loading-affordance primitive, and a text-state swap using the existing
`micro_label` styling is consistent with the panel's current minimal,
static aesthetic.

## Error handling

- `brain.rebuild(None)` failing inside `spawn_blocking` (bad permissions,
  I/O error) is caught and forwarded as `Result::Err(String)` in
  `BrainReindexed`; the handler logs it via `tracing::error!` exactly as
  today and leaves the last-good `brain_view.entries` displayed.
  `reindexing` still clears so the button returns to its idle, clickable
  state — a failed rebuild must not permanently disable manual retry.
- If the watcher itself fails to start (permission denied on the brain
  dir, OS watch-descriptor limit reached), log a warning once and continue
  without live updates for that catalogue — the manual Reindex button
  still works. This must never panic or prevent the app from starting.

## Testing

The four existing tests that call `Message::BrainReindex` synchronously and
assert on `brain_view.entries` immediately after
(`brain_reindex_reloads_entries_from_disk` and three others in
`app.rs:3214-3478`) need updating for the now-async path, following this
codebase's existing precedent for testing `Task`-returning messages:
`iced_runtime::task::into_stream(task)` + drive to completion with
`futures::StreamExt`, exactly as
`resume_message_keeps_status_interrupted_when_tmux_create_fails` does
(`app.rs:2843`).

New tests:

- `start_reindex` sets `reindexing = true` and returns a real `Task`, not
  `Task::none()`.
- A trigger while `reindexing` is already `true` sets `reindex_pending`
  instead of returning a second real task.
- `BrainReindexed` completion clears `reindexing`; if `reindex_pending` was
  set, it immediately starts another rebuild.
- A `BrainReindexed` result tagged with a `catalogue` index that no longer
  matches `state.active_catalogue` is discarded — asserted by switching
  catalogues between kickoff and completion and checking the first
  catalogue's stale result doesn't clobber the second's view.
- The `.index.db`/`.gitignore` filter: simulate a debounced event touching
  only those paths and assert it does not yield `Message::BrainFilesChanged`
  (a pure-function unit test on the filter predicate, no real watcher
  needed).

## Wiring

- Add `notify` and `notify-debouncer-mini` to `[workspace.dependencies]` in
  the root `Cargo.toml`, referenced with `workspace = true` from
  `ninox-app`'s `Cargo.toml` (this is app-only — `ninox-core` and
  `ninox-server` are untouched).
- New `Message` variants in `app.rs`: `BrainFilesChanged`,
  `BrainReindexed { catalogue: usize, result: Result<RebuildStats, String> }`.
- New fields on `BrainView`: `reindexing: bool`, `reindex_pending: bool`
  (both default `false`).
