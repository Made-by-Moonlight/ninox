# MLOPS-5339 GUI parity, item 2: Brain entry editing

Spec: `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5339-gui-parity.md` item 2. The edit/new
board is being drawn from the layout proposed below; the GUI views wait for it.

## What the TUI actually does

The TUI never edits fields in-app. `e` opens the raw `.md` in `$EDITOR`, `a` opens a template
(`template`) in `$EDITOR` and derives the new file's path with `entry_from_edit`, and `D` deletes
behind a confirm. A `Job` (`Write`/`Delete`/`Reindex`/`ReindexAll`) then runs through `run_job`:
write or delete, sync the brain's remote, full rebuild. All of that lives in
`ninox-tui/src/tui/brain.rs`. PR #293 adds `brain_job_paths`/`unqualify_job_id`, which turn a
qualified `source:id` from the merged "All sources" index into the owning catalogue's path and raw
id. That fixes delete under All sources building a bogus path.

## Tasks

1. **Frontmatter fields** (`ninox-core/src/brain_fields.rs`, done): `EntryFields { name, tags,
   repos, body }`, `read_fields`, `apply_fields`. Apply rewrites only those three keys and the body.
   Unknown keys, comments, blank lines and key order survive. Round-trip tests, plus a test pinning
   `read_fields` to what `BrainIndex` stores for the same file.
2. **Extract the job path into `ninox-ui-shared::brain_edit`** (after #293 merges, its own commit):
   move `template`, `slug`, `entry_from_edit`, `source_file`, `Job`, `JobDone`, `run_job`,
   `brain_job_paths`, `unqualify_job_id` and their tests out of `ninox-tui`. The TUI calls them
   through a re-export; no behaviour change. Add `Job::Save { id, content }` (same write as `Write`,
   verb "saved") so an in-place edit isn't reported as "added".
3. **GUI state and message plumbing** (no views): a `BrainEditor` draft (target catalogue root, raw
   id, original file text, name, tags, repos, body as `text_editor::Content`, kind for new entries),
   kept outside `BrainViewState`'s `Debug` derive. Messages for start edit, start new, field
   changes, body action, save, cancel, delete request/confirm/cancel, and job done. Save:
   `apply_fields(original, draft)` → `Job::Save`, or for a new entry `template(kind, [])` +
   `apply_fields` → `entry_from_edit` → `Job::Write`. Delete: `Job::Delete`. Every job resolves its
   catalogue and raw id through `brain_job_paths`, runs `run_job(.., embed: false)` in a
   `Task::future`, and lands in the existing `BrainReindexed` refresh. New is refused under All
   sources.
4. **GUI views** (after the board is approved): Edit/Delete buttons next to Show in graph / Open
   file, an in-place editor in the reading pane, an inline delete confirm, and "+ New" in the List
   toolbar (disabled under All sources) with a kind picker.
   Unresolved wikilinks render as dim plain text instead of in the link colour (resolve each
   target against the loaded entries in `preprocess_wikilinks`, the same way `BrainLinkClicked`
   does).
5. Isolated-instance window captures against the approved board.
