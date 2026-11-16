# Orchestrator Plan Tracking: registered goals doc + live markdown panel

**Date:** 2026-08-26
**Status:** Approved design, pre-implementation

## Problem

Orchestrators plan and re-plan continuously across a session (goals, task
breakdown, what's delegated to which worker, what's blocked). That plan
currently lives only in the orchestrator's own working memory / scrollback —
a user watching the desktop app has no way to see it without scrolling the
orchestrator's terminal and guessing which lines are still current.

Orchestrators already write scratch markdown files for their own use. This
feature lets an orchestrator register one such file as its "goals/plan doc"
and gives the desktop app a live, rendered, at-a-glance view of it that
updates as the orchestrator edits the file — no different from watching a
README render on GitHub as you edit it, but inside Ninox, scoped to one
orchestrator session.

Two problems block a good version of this:

1. There's no session-scoped "register a resource against my orchestrator
   session" storage/CLI pattern the UI can poll, comparable to the `prs`
   table but for a single doc path instead of a set of PR watches.
2. Ninox's only markdown renderer (`iced::widget::markdown`, used by the
   brain panel) does not support text selection — nothing in the codebase
   does; there is no `.selectable()`, `text_editor`, or selection-color
   style anywhere outside the terminal widget. A live plan panel a user is
   expected to read continuously needs to support copy-paste.

## Decisions (scope for this pass)

- **No config toggle.** Unlike `[pr_watch]`, this has no API-cost or
  rate-limit blast radius to gate — it's a local CLI + local DB row + local
  UI panel. Always available once shipped.
- **One plan doc per orchestrator, upsert semantics.** Table is keyed by
  `orchestrator_id PRIMARY KEY` (mirrors `orchestrator_runtimes`). "Register"
  and "update" are the same call — `ninox plan register <path>` upserts.
  There is no versioning/history; the row always reflects the latest
  registration.
- **Store the path, not the content.** The DB row is a pointer
  (`file_path`, timestamps). The UI reads the file from disk on its poll
  tick and re-parses on mtime change. This avoids a write-fan-out problem
  (orchestrator edits the file with its own tools, not through `ninox`) and
  matches the fact that nothing else in the codebase content-syncs a file
  into sqlite.
- **Poll, not file-watch.** There is no filesystem-watcher dependency
  anywhere in this codebase (`notify` is not in the tree; `notify-rust` is
  the unrelated toast-notification crate). Introducing one for a single
  markdown panel is disproportionate. The desktop app already has a
  precedent: `poll_sub` (`app.rs:3608-3616`, `Subscription::run_with_id`
  ticking every 3s, driving `Message::PollSessions`). This feature adds a
  sibling tick at the same cadence that stats the registered file and
  reloads on mtime change — cheap, no new dependency, consistent UX with
  the rest of the app (sessions list itself is only ever 3s-fresh).
- **Orchestrator-only, authorized like `Workers`, not like `request-work`.**
  `request-work` is worker→orchestrator and only checks `NINOX_SESSION` is
  set. This command is orchestrator-facing, so it uses the same
  anti-spoofing path as the `Workers` subcommand family:
  `ninox_core::workers::authorize_orchestrator` — requires
  `NINOX_ORCHESTRATOR_ID` + `NINOX_CALLER_TYPE=orchestrator` and
  cross-checks the live tmux pane identity against the persisted
  `orchestrator_runtimes` row.
- **UI surfaces the panel as a `DetailPanel` tab, not a new top-level
  `View`.** The task calls for something "similar to the existing split
  view." The existing split view *is* `DetailPanel::Split` inside
  `session_detail.rs` (terminal + info pane, resizable via
  `DragTarget`/`drag_handle`). Reusing that exact mechanism — a new
  `DetailPanel::Plan` tab rendering `row![terminal, drag_handle,
  plan_pane]` — is more consistent than inventing a second, parallel
  panel-composition system. The tab only appears for orchestrator sessions.
- **Markdown selection: read-only `text_editor` + a hand-rolled
  `Highlighter`, not a custom canvas/hit-testing widget.** The terminal
  widget's selection mechanism (`canvas::Program`, `SelectionState`,
  pixel→cell hit-testing) only works because the grid is fixed-size
  monospace. Markdown is proportional, variable-line-height, wrapped text —
  reusing that approach means building real glyph-layout hit-testing from
  scratch, which is exactly the kind of standalone subsystem the task
  flagged as an escalation trigger. Instead: iced's `text_editor` widget
  already implements real selection, keyboard nav, and clipboard copy for
  free. Its `Highlighter` trait (used for code-editor syntax highlighting
  in iced's own examples) lets us color/weight spans per line without
  owning selection logic ourselves. Trade-off, called out explicitly for
  Ethan to sign off on: this gets bold/italic/heading/code/link *styling*
  and full text selection, but not `markdown::view`'s block layout (no
  distinct heading font sizes, no indented block quotes/lists, no inline
  images, no clickable links). If pixel-parity rendering *and* selection is
  wanted, that's a materially bigger effort (a custom selectable markdown
  widget) and should be its own follow-up, not bundled here.
- **Retrofit the brain panel: attempt, not required.** The task says this
  is "welcome" if natural. Swapping brain panel's `markdown::view` call for
  the new selectable component is a small, isolated change (same input:
  markdown string) — do it if it doesn't complicate the plan-panel work,
  otherwise leave a follow-up note in the PR.

## 1. CLI

New top-level clap subcommand, dispatched before heavy TUI setup, mirroring
`Command::Workers`:

```rust
Command::Plan { action: PlanAction }

enum PlanAction {
    /// Register (or re-register) a markdown file as this orchestrator's plan doc.
    Register { file: PathBuf },
    /// Remove this orchestrator's registered plan doc.
    Unregister,
    /// Print the current registration (path + timestamps + existence check).
    Show,
}
```

- `ninox plan register <path>` — resolves `orchestrator_id` via
  `authorize_orchestrator`, canonicalizes `path` if it exists (stores as
  given, best-effort-canonicalized, if it doesn't exist yet — an
  orchestrator may register before writing the first draft), upserts the
  `orchestrator_plans` row. Idempotent.
- `ninox plan unregister` — deletes the caller's row, if any. No error if
  none exists (idempotent).
- `ninox plan show` — prints the same JSON envelope shape as
  `emit_workers_envelope` (`schema_version`/`ok`/`data`/`error`), `data`
  being `{ file_path, registered_at, updated_at, exists }`.

All three exit through `classify_worker_error`-style distinct exit codes:
unauthorized (not run by an orchestrator) vs. not-found (unregister/show
with no row) vs. success.

## 2. Storage — `ninox-core/src/store.rs`

```sql
CREATE TABLE IF NOT EXISTS orchestrator_plans (
    orchestrator_id TEXT PRIMARY KEY,
    file_path       TEXT NOT NULL,
    registered_at   INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
```

Added into the same `execute_batch` schema block as
`orchestrator_runtimes` (store.rs:553-562), same file, same style — no
separate migrations list exists in this codebase to register with.

Methods on `Store`, following `register_orchestrator_runtime`'s upsert
shape (store.rs:1252-1290):

- `register_orchestrator_plan(&self, orchestrator_id: &str, file_path: &str) -> Result<()>`
  — `INSERT INTO orchestrator_plans (...) VALUES (...) ON CONFLICT(orchestrator_id) DO UPDATE SET file_path = excluded.file_path, updated_at = excluded.updated_at`. `registered_at` is only set on first insert (`ON CONFLICT` clause leaves it untouched via `excluded` omission — keep the original `registered_at`, bump `updated_at`).
- `get_orchestrator_plan(&self, orchestrator_id: &str) -> Result<Option<OrchestratorPlan>>`
  — plain `query_row(...).optional()`, mirrors `worker_finalization` (store.rs:1738-1759).
- `unregister_orchestrator_plan(&self, orchestrator_id: &str) -> Result<bool>`
  — `DELETE ... WHERE orchestrator_id = ?1`, returns whether a row was removed.

`OrchestratorPlan` type (in `ninox-core::types` alongside other row
structs): `{ orchestrator_id: String, file_path: String, registered_at: i64, updated_at: i64 }`.

## 3. Server route (optional parity layer)

For the browser/remote UI (`feat/brain-browser-ui` line of work) to be able
to show the same panel later, add the same thin route pattern as
`sessions_router` (`ninox-server/src/routes/sessions.rs`):

```rust
// routes/orchestrator_plan.rs
pub fn orchestrator_plan_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/:orchestrator_id/plan", get(get_plan))
        .with_state(engine)
}
```

`get_plan` calls `engine.store.get_orchestrator_plan(...)`, reads the file
from disk (best-effort; `exists: false` + empty body if missing), and
returns `{ file_path, updated_at, exists, content }` as JSON. This is not
on the desktop app's critical path (see below) — it exists so the route
layer isn't skipped entirely and a future remote UI doesn't need a second
design pass, but is a small, mechanical addition scoped to "don't leave a
gap," not a requirement for the desktop panel to work.

## 4. Desktop UI discovery/refresh

The desktop app (`ninox-app`) holds `Arc<Store>` in-process via `Engine`
already (`state.engine.store...`, same as the existing `PollSessions`
handler) — it does **not** need to go through the HTTP route to read its
own local DB. Flow:

1. Extend the existing 3s poll subscription (`app.rs:3608-3616`) with a
   new tick target, or add a sibling `Subscription::run_with_id("plan-poll", ...)`
   at the same 3s cadence emitting `Message::PollOrchestratorPlan { orchestrator_id }`
   only while a `DetailPanel::Plan` tab is the active view (avoid polling
   for sessions the user isn't looking at).
2. Handler: `store.get_orchestrator_plan(orchestrator_id)` → if `file_path`
   changed or unset, or the file's `mtime` differs from the last-seen
   value cached in `PlanViewState`, re-read the file, re-parse markdown,
   update `PlanViewState { file_path, content, markdown_lines, last_mtime, last_error }`.
   `last_error` covers "row exists but file missing/unreadable" so the
   panel can show a clear placeholder instead of silently going stale.
3. No row registered yet → panel shows a placeholder: *"No plan doc
   registered. The orchestrator can run `ninox plan register <file>`."*

`PlanViewState` lives on `App` next to `BrainViewState`
(`app.rs:235`-style field), scoped per currently-viewed orchestrator
session (not global — an orchestrator's plan is its own).

## 5. UI panel — `DetailPanel::Plan`

- Extend `DetailPanel` (`session_detail.rs:191-199`) with a `Plan` variant.
  Tab (`panel_btn(app, "Plan", DetailPanel::Plan, *panel)`) is only added
  to the tab row when the session being viewed is an orchestrator (mirrors
  how other session-kind-specific affordances are gated elsewhere in that
  file).
- Composition (`session_detail.rs:648-662` pattern):

```rust
DetailPanel::Plan => row![
    term_stage(s, term_frame(s, color, tmux_line, status_word, terminal_pane)),
    App::drag_handle(DragTarget::InfoPanel, s.rule_dark),
    plan_pane(app, session_id),
].height(Length::Fill).into(),
```

Reuses the existing `DragTarget::InfoPanel` resize plumbing verbatim (same
`sidebar_width`/`info_width`-style clamp-and-drag state machine already in
`app.rs:2708-2739`) — no new resize mechanism.
- `plan_pane` (new fn in `components/plan_panel.rs`): header (file path,
  last-updated timestamp, small "open in editor" affordance reusing the
  pattern from PR #13's "Open in editor" button), then the selectable
  markdown body (§6) filling the remaining height, styled with
  `style::card_style`/theme tokens like every other panel.

## 6. Selectable markdown rendering — `components/selectable_markdown.rs`

New shared component, not panel-specific, so both the plan panel and (if
the brain-panel retrofit lands) the brain panel can use it:

```rust
pub fn selectable_markdown<'a>(
    content: &'a text_editor::Content,
    scheme: &'a ColorScheme,
) -> Element<'a, Message> {
    text_editor(content)
        .highlight_with::<MarkdownHighlighter>(
            MarkdownHighlighterSettings { scheme: scheme.clone() },
            |highlight, _theme| highlight.to_format(),
        )
        .style(/* transparent background, no visible cursor line gutter */)
        .on_action(Message::PlanEditorAction) // read-only: only Move/Select actions applied, Edit actions are ignored/no-op
        .into()
}
```

- `text_editor::Content` is rebuilt from the raw markdown string whenever
  the file changes (cheap — these are short plan docs, not multi-MB logs).
- Read-only enforcement: `Message::PlanEditorAction(text_editor::Action)`
  handler applies the action to `Content` only when
  `!action.is_edit()`(cursor moves, selection, scroll) and drops edit
  actions — giving real selection/copy/keyboard-nav without allowing
  mutation. This is the standard "read-only text_editor" pattern (no
  separate widget mode exists in iced 0.13; gating in the update handler is
  the documented approach).
- `MarkdownHighlighter` implements `iced::advanced::text::highlighter::Highlighter`:
  a small hand-rolled per-line scanner (not a full CommonMark parser) that
  recognizes: `#`/`##`/`###` headings → bold + `ink` color; fenced/backtick
  code → `MONO` font + tinted background-adjacent color; `**bold**` →
  `SANS_BOLD`; `*italic*`/`_italic_` → italic; `[text](url)` → `accent`
  color (not clickable — see trade-off above); `-`/`*`/numbered list
  markers → `faint` color for the marker only. This is intentionally
  simpler than `pulldown-cmark`-driven parsing (already pulled in via the
  `markdown` iced feature) — a line-scanner is enough for "clearly reads as
  formatted," and keeps this from becoming a second markdown parser
  implementation.
- Copy: `text_editor` has native Cmd+C support already (no `arboard`
  wiring needed, unlike the terminal widget) — this comes for free from the
  widget.

## 7. Orchestrator skill docs

There is no `skills/orchestrator/*.md` on disk and no capability registry
(confirmed: `grep -ri capabilit` across the repo returns nothing relevant).
Skill content is generated as Rust string literals and always-overwritten
per session, per `setup_orchestrator_root` (`app.rs:3670`). Plan tracking
is documented the same way `## Work Requests` is documented today:

- Add a new `## Tracking Your Plan` section to the `spawn-worker` SKILL.md
  content (`app.rs`, alongside the `## Work Requests` section at
  app.rs:3782-3794), telling the orchestrator: register a markdown file
  with `ninox plan register <path>`, that the user can see it live in the
  desktop app, and to re-run `register` (or just keep editing the same
  file) to keep it current. `ninox plan unregister` when no longer tracking
  one.
- Add a clap doc-comment on `Command::Plan`/`PlanAction` variants in
  `main.rs` (this is what `--help` renders — the only other place command
  surface is documented, per the research: no separate registry exists).

## 8. Testing

- Store: round-trip register → get → update (upsert bumps `updated_at`,
  preserves `registered_at`) → unregister → get returns `None`. Mirrors
  existing `Store` test style for `orchestrator_runtimes`.
- CLI: `authorize_orchestrator` gating (rejects missing
  `NINOX_ORCHESTRATOR_ID`/wrong `NINOX_CALLER_TYPE`, same as `Workers`
  tests); `show`/`unregister` on an unregistered orchestrator return the
  documented "not found" envelope rather than erroring.
- `MarkdownHighlighter`: unit tests per construct (heading, bold, italic,
  inline code, link, list marker) asserting the emitted `(Range, Highlight)`
  spans, following the style of any existing highlighter/formatting unit
  tests in the codebase.
- Read-only enforcement: a test driving `Message::PlanEditorAction` with an
  edit action asserts `Content` is unchanged.

## Out of scope (deliberate)

- Plan doc history/versioning — only the latest registration is tracked.
- Multiple plan docs per orchestrator — one row per `orchestrator_id`.
- File-watching (inotify/FSEvents) — 3s poll matches existing app-wide
  freshness bar.
- Clickable links / full block-layout parity with `markdown::view` inside
  the selectable widget — flagged above as a bigger, separate effort if
  wanted later.
- Remote/browser UI panel — the server route is added for parity but no
  browser-side component is built in this pass.
