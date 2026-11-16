# MLOPS-5415: GUI Ninox prompt reply drawer + global search blur dismissal

Spec: `PromptReply.dc.html` (approved). Replaces the one-line
`ninox_reply_strip` with a drawer rising from the prompt bar, and makes the
⌘K global search dismiss on blur.

## Part A: reply drawer

1. **Transcript reader** (`ninox-ui-shared/src/prompt_agent.rs`): Claude
   Code's on-screen rendering drops Markdown, so a claude-code prompt
   agent's answers come from its own transcript JSONL
   (`lifecycle::usage::claude_projects_dir()/<slug(workspace)>`).
   - File choice: `<claude_session_id>.jsonl` from the session row. If a
     newer top-level transcript was written after the turn was asked
     (`/clear` rotates the transcript without updating the row), follow
     it. Fall back to the newest top-level file only when the row has no id.
     Never descend into subdirectories, never read `agent-*.jsonl`, and skip
     `isSidechain: true` entries.
   - Delta: assistant `text` blocks after a byte cursor (complete lines
     only); `stop_reason: "end_turn"` marks the answer finished.
   - Non-claude harnesses keep the pane-text `reply_since` path.
2. **Drawer state machine** (`ninox-gui/src/prompt_agent.rs`): `ReplyDrawer
   { turns, expanded, notice, context_pct }`, `Turn { question, answer,
   state, relays, … }`. Pure transitions: ask, collapse, reopen, clear,
   apply transcript/screen deltas, apply relay events, status.
3. **Relays**: `message_events` rows from `ninox-prompt` → X during a turn.
   A row waits until X → `ninox-prompt` arrives. While no reply has come,
   it reads "waiting for its answer…" until the turn ends, plus a grace
   period. After that it times out to "no direct reply yet", which covers a
   reply that went to the worker's parent orchestrator instead.
4. **Status / context**: relaying > answering > idle; `context N% used`
   from the session row's `context_used_pct`, omitted when `None`.
5. **View** (`components/shell/reply_drawer.rs`): drawer stacked over the
   bottom of the centre column (470px, clamped). The header has Clear
   context ⌘⇧K and Collapse esc. The body is a vertical-only scrollable
   with right-aligned user bubbles. Answers are markdown parsed in
   `update()`, with session names linkified via `components::internal_link` (iced keeps only http(s) links) and
   known `#N` PRs to their URL. When collapsed, a one-line indicator sits
   above the prompt bar. The prompt bar gains a `↑↓ history` hint.
6. **Esc**: a captured Escape (a focused `text_input` swallows it) is
   forwarded; priority is search → drawer → existing handling.
7. Ghost-plan cards: out of scope; `reply_drawer::turn_extensions` is the
   empty slot they will fill.

## Part B: search dismissal

- Esc: forwarded captured/uncaptured Escape closes it.
- Click outside: `mouse_area` tracks hover over field + dropdown; a left
  press anywhere while not hovered closes it (presses inside still let a
  result button fire).
- Focus loss: window `Unfocused` closes it, as does focusing the ninox
  prompt. iced 0.13 has no Tab focus traversal, so a click elsewhere is
  the only in-window way the input loses focus, and that's covered above.
- Tab switch: every `Navigate*` closes it.
- All dismissals keep `draft`; ⌘K reopens with it.

## Tests

Unit tests for every transition and dismissal route through `App::apply`,
plus transcript parsing/selection fixtures under `isolate()`.
