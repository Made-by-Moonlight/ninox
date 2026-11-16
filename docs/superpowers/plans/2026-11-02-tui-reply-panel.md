# MLOPS-5350: TUI Ninox prompt reply panel

Spec: `PromptReply.dc.html` (GUI build: #302). The TUI currently shows a
prompt reply as one line (`PromptStripState::Streaming`'s last line).

## 1. Shared reply state (`ninox-ui-shared/src/prompt_agent/reply.rs`)

Move from `ninox-gui/src/prompt_agent.rs`, unchanged in behaviour:
`AnswerSource`, `AnswerState`, `Relay`, `RelayState`,
`RELAY_REPLY_GRACE_MS`, `Turn`, `DrawerStatus`, `Notice`, `ReplyDrawer`,
`AnswerUpdate`, `PromptPoll`, `last_reply_block`, and their tests.

- `Turn<R>` / `ReplyDrawer<R>` carry a frontend render cache `rendered: R`
  (GUI: `Vec<iced::widget::markdown::Item>`; TUI: `()`), so the iced
  parse stays in the GUI.
- New async helpers lifted out of `ninox-gui/src/app.rs`:
  `deliver_to_agent` (transcript baseline + `ask` + source pick),
  `orchestrator_baseline`, and `poll` (transcript / `reply_since` /
  picked-orchestrator `read_screen` + `context_used_pct` + message events).
- GUI `prompt_agent.rs` keeps `PromptTarget`/`resolve_delivery` and
  re-exports the shared items via type aliases; `app.rs` swaps its inline
  async bodies for the shared helpers. Nothing else in the GUI changes.

## 2. TUI markdown (`ninox-tui/src/tui/markdown.rs`)

One renderer for the TUI: `render(body, width, palette, opts) -> Vec<Line>`.
Headings, bold, inline code, `-`/`*`/numbered lists with hanging indent,
blockquotes, fenced code in a dim box, wrapped by display width. Brain's
`markdown_lines` is replaced by it, keeping frontmatter stripping and
wikilink / link styling (`opts`).

## 3. TUI panel

- `PromptStrip`/`PromptStripState` are replaced by `ReplyDrawer<()>` plus
  panel scroll/focus state on `FleetState`.
- Delivery uses the shared helpers; a picked orchestrator keeps the
  `deliver_interactive` + pane-snapshot path. Polls are generation-gated, one
  in flight.
- Layout: the panel overlays the bottom of the Fleet main area, above the
  prompt. Collapsed, a one-line indicator replaces the old strip.
- Header: `❯ Ninox prompt · <status>[ · context N% used]`, hints
  `ctrl+l clear`, `esc collapse`.
- Body: right-aligned question, then the rendered answer and relay rows
  (`Asked <name> (its orchestrator) — relayed HH:MM, waiting…` →
  `no direct reply yet` / `replied HH:MM`).
- Keys: `/` opens the panel too; PgUp/PgDn scroll while typing; esc out of
  input focuses the panel (j/k/PgUp/PgDn/g/G scroll); esc again collapses;
  enter or `/` reopens; ctrl+l clears.

## 4. Capture fixture

`NINOX_TUI_REPLY_FIXTURE` seeds the panel; it is behind
`#[cfg(debug_assertions)]`, with a test asserting it's absent in release.

## 5. Tests

Panel state machine (open, collapse, reopen, clear, history, scroll clamp),
markdown wrapping at 120/200 columns, relay timeout, and the existing GUI
drawer tests (moved).
