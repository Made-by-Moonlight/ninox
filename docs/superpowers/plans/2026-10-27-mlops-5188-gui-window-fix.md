# MLOPS-5188: ninox-gui opens zero windows on macOS

## Root cause

`App::new` (`crates/ninox-gui/src/app.rs`) ran, synchronously, before iced's
`run_with` ever creates the winit window: for *every* persisted review
comment (`engine.store.list_comments()`), it eagerly built an
`iced::widget::text_editor::Content::with_text(&comment.body)` — a full
cosmic-text shape of the comment body — to seed `comment_editors`.

On a dev machine with real review history (5,508 rows / ~12MB of comment
bodies in `review_comments`), this pegged the main thread at 100% CPU inside
`cosmic_text::Buffer::shape_until_scroll` for the entire run, confirmed via
`sample` on the live process: the whole 3s sample window showed one single
call stack, rooted at `App::new` → `text_editor::Content::with_text`. Since
window creation happens *after* this synchronous constructor returns, no
window ever appeared — reproducible both via `cargo run` and the bundled
`.app`, matching the ticket exactly.

## Fix

Stop building `comment_editors` eagerly. `review_threads` (cheap: grouping
already-deserialized `Comment` structs, no shaping) still loads in full at
startup, same as before. `comment_editors` starts empty and is hydrated
lazily, per PR, the same way `ensure_diff`/`ensure_plan` already hydrate
their on-demand panels: a new `ensure_comment_editors(state, pr_id)` is
called from `Message::NavigateSession` (the only place a session's PR
thread becomes visible — `info_panel`'s Marginalia feed already tolerates a
missing entry, rendering blank space for it, so no view-layer change was
needed). Real-world PRs top out at ~38 comments each, so the per-PR shape
cost at navigation time is negligible.

## Verification

- `cargo test -p ninox-gui`: 294 passed (2 new tests covering this: `App::new`
  no longer populates `comment_editors` while still loading `review_threads`,
  and `NavigateSession` lazily builds editors for exactly the viewed PR).
- `cargo clippy -p ninox-gui --all-targets -- -D warnings`: clean.
- Built the real binary against the live local store (the same
  5,508-comment/12MB dataset that reproduced the bug) and launched it:
  `osascript 'tell application "System Events" to count windows of process
  "ninox-gui"'` went from `0` (hangs indefinitely, 100% CPU, confirmed via
  `sample`) to `1` within ~3 seconds of launch, with normal CPU usage
  afterward. Screenshot of the rendered window (fleet board, real session
  data) confirms it isn't just an empty/blank window.

## Notes

- No design spec existed under `docs/superpowers/specs` for this ticket;
  this is a self-contained bugfix with no open design questions.
