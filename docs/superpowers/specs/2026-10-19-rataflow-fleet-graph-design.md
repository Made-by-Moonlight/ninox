# Fleet + Per-Session Flow-Graph Overview (rataflow)

**Status:** Proposal — design spec only. No implementation in this change
yet. (Originally scoped under a Linear ticket that was later canceled as a
tracking artifact; the feature itself is unchanged and described here on
its own terms.)

## Context

Ninox's TUI has a `View::Overview` today, but it's not a graph — it's a
live tiled grid of terminal panes (`layout.rs::overview()` computes a
`cols × rows` grid of `Tile` rects; `view.rs::draw_overview()` streams each
session's actual pane content into its tile). There is no single view of
fleet topology, and no way to see a session's own internal agent activity
(tool calls, Task-tool subagent spawns) the way `zoetrope` does for one
session at a time.

Ninox already reads each session's own Claude Code JSONL transcript for
cost/usage tracking (`crates/ninox-core/src/lifecycle/usage.rs`), and the
`rataflow` crate (crates.io, v0.1.0+, ratatui-based node-graph widget, same
author as `zoetrope`) provides the rendering primitive needed to visualize
both fleet topology and per-session internal flow.

**Primary motivating use case:** scan the whole fleet at a glance for any
session that's stuck or waiting on human input, then jump straight to it
to unblock — not watching activity for its own sake. The backing data
already exists: `ActivityState::Blocked` (self-declared via
`ninox worker-status set blocked --note "…"`, persisted past `Stop` — see
`crates/ninox-core/src/worker_status.rs`) and the `Need`/`Attention`
helpers already computed in `tui/state.rs`. `ninox list --json` already
exposes `activity`/`activity_note`/`activity_since` per session — no new
backend data is needed for the fleet layer.

A separate, unrelated effort is expected to make the `activity`/
`activity_note`/`Blocked` signal itself more reliable over time (wiring
additional hook-based detection for cases the self-declared signal
misses). This spec does not depend on or block on that — it consumes
`activity`/`activity_note` on `Session` exactly as they exist today
(`crates/ninox-core/src/types.rs`), and will automatically benefit once
that detection improves, since both write to the same fields.

## Scope

TUI-only — `rataflow` is a ratatui widget; there is no Iced/GUI
equivalent. Two levels:

1. **Fleet topology** — one node per orchestrator, one child node per
   session under it, edges from the existing parent-child relationship.
2. **Per-session drill-down** — selecting a session node reveals that
   session's own tool-call/subagent activity, parsed live from its actual
   Claude Code JSONL transcript.

Out of scope: reusing `zoetrope`'s own code (not published as a library —
only the `zoe` binary and `rataflow` are on crates.io, so the transcript
parser here is new); a browser/web view; a GUI/Iced equivalent.

## Architecture

### New view, following the existing self-contained-subview pattern

A new `View::Graph` variant (`tui/state.rs`'s `View` enum, alongside
`Board`/`Overview`/`PrWatches`/`Brain`/`Settings`), implemented in a new
`crates/ninox-app/src/tui/graph.rs`, structurally mirroring
`tui/brain.rs`: its own state struct embedded in `TuiState`, its own
key-handler function, its own draw function wired into `view.rs::draw()`.
It gets a numbered tab mnemonic alongside the existing
`["1 Fleet", "2 Overview", "3 PRs", "4 Brain", "5 Settings"]` list in
`view.rs`, and an entry in the `HELP` const (`state.rs`) so its keybindings
are discoverable through the TUI's existing `?` help overlay — this is
the "discoverable in the keybinding/help surface" requirement; there is no
separate keybinding registry to extend.

`GraphState` holds **two independent `rataflow::Flow` instances** — one
for the fleet topology, one for whichever session is currently drilled
into — plus `drilled_into: Option<SessionId>` selecting which is active
and receiving input. `Esc` pops `drilled_into` back to `None`, mirroring
the existing `Board ↔ Overview` back-navigation idiom already in
`overview_key`.

**Alternatives considered:**

- *Nest the per-session graph inside its fleet node using rataflow's
  compound/child-node support.* Rejected: that feature targets fixed
  bounded-extent nested diagrams, not a subgraph that must independently
  live-update while the fleet view stays at a glance. It would also
  clutter the fleet view, undermining the "scan the whole fleet at a
  glance" requirement.
- *Add only the per-session drill-down as a new view, leave fleet-wide
  scanning to the existing Board list.* Rejected: misses the two-level
  requirement and the primary "scan the whole fleet" use case.

### Fleet topology layer: no new backend data

`st.rows: Vec<Row>` (`tui/state.rs`) is already repopulated every 1s tick
from the store and already carries exactly the parent-child data needed:
`Row { session, is_orchestrator, group }`, where `group` is the owning
orchestrator's id. The fleet `Flow`'s nodes and edges are built directly
from `st.rows` on each refresh — no new store queries, no new `ninox list`
fields.

### Stuck/blocked styling: reuse existing attention computation

`st.attention(row)` already computes an `Attention` enum
(`Unknown < Idle < Working < Done < Blocked`) with `Blocked` sourced
directly from `ActivityState::Blocked`. `st.needs_you(row)` already
aggregates `Need::{Blocked, CiFailed, Interrupted, Unread, Review,
Mergeable}` — `CiFailed`/`Review`/`Mergeable`/`Interrupted` are concretely
how a PR needing attention manifests today via `SessionStatus`.

Note: `NotificationKind::AgentStuck` is declared in `types.rs` but is
**never emitted anywhere in the current codebase** (confirmed by search).
Building a notification-feed consumer in the TUI for an event that's never
raised would be dead code. Instead: a node gets "stuck" styling whenever
`attention(row) == Attention::Blocked` OR `needs_you(row).is_some()`. Once
something starts emitting `AgentStuck` in the future, it will show up
through `activity`/`activity_note` on the same `Session` struct this
reads, with no changes needed here.

Colors reuse the existing `attention_glyph(a, &palette)` helper
(`tui/view.rs`, already `pub`) for visual parity with Board/Overview
rather than inventing a second color scheme.

### Interaction

`rataflow::Flow::handle_key_event`/`handle_mouse_event` return
`FlowEvent::NodeClicked`/`SelectionChanged`. On a selected session node:

- One key drills down: builds/activates that session's tool-call `Flow`
  (parsing its transcript — see below) and sets `drilled_into`.
- A distinct key ("connect", matching whatever Board already uses) jumps
  straight to the live terminal by reusing the **existing** path exactly:
  `st.selected = idx; st.view = View::Board; st.open_selected()` — the
  same call `open_tile()` already makes, which returns `Action::Connect`
  for `tui/mod.rs`'s `perform()` to act on. No new terminal-attach code.

`activity_note` and `needs_you(row).label()` are shown in a small
side/info panel when a node is focused — ratatui has no real mouse-hover
concept; this is the keyboard-driven TUI's equivalent, consistent with how
other panels (e.g. `info_panel.rs`) already work.

### Per-session drill-down data: extend `usage.rs`, don't fork it

Per the scope note to extend rather than duplicate the parser: add a new
function alongside `parse_turn` in `crates/ninox-core/src/lifecycle/
usage.rs` (not modifying `parse_turn`'s usage-only contract), reading:

- `assistant`-type lines' `message.content[]` entries of type `tool_use`
  (`id`, `name`, `input`) — a `tool_use` with `name == "Task"` is treated
  as a subagent spawn (its `input` carries `prompt`/`description`/
  `subagent_type`).
- `user`-type lines' `message.content[]` entries of type `tool_result`
  (`tool_use_id`, `content`), paired back to their originating `tool_use`
  by id.

**Transcript resolution (assumption, flagged for review):** a session's
*current* transcript is the most-recently-modified `*.jsonl` file in
`claude_projects_dir()/claude_project_slug(workspace)`. Today's usage
ingestion deliberately sums *all* files in that directory (lifetime cost);
the live tool-call view instead needs just the active conversation. If
there's an existing, more precise session→transcript-file mapping
elsewhere in the codebase, prefer that over the mtime heuristic.

### Live-tailing: TUI-local, scoped to the drilled-in session only

No incremental tailing exists anywhere today — `ingest_dir()` fully
re-reads every file on every call, which is fine for small usage sums but
won't scale to a growing tool-call graph re-parsed from byte 0 every tick.
Plan: track a byte offset in `GraphState`, **only for whichever session is
currently drilled into**, driven by the TUI's existing 1s
`tick.tick()` (`tui/mod.rs`). Reset/reparse from scratch when drilling
into a different session.

**Alternatives considered:**

- *Daemon-side fleet-wide tailing*, extending `poller.rs::poll_usage()` to
  extract tool-events for every session every tick. Rejected: pays full
  transcript-parse cost for sessions nobody is currently looking at, and
  adds store/schema surface the ticket doesn't call for.
- *A dedicated background thread/task per drilled session* (inotify-style
  watching). Rejected as unneeded complexity — the TUI's existing
  synchronous 1s tick already drives every other refresh in this screen;
  reusing it keeps this consistent with the rest of the codebase.

### rataflow integration specifics

- `Flow::from_edges(&edges, Sugiyama::vertical())` for fleet topology (auto
  layout is fine — orchestrator/session trees are shallow); `Flow::
  with_graph(nodes, edges)` if explicit positioning turns out to read
  better for the drill-down's call-sequence shape.
- Custom node rendering via the `NodeContent` trait (`render(&self, ctx:
  &NodeRenderContext<'_>, buf: &mut Buffer)`) — one `SessionNodeContent`
  for fleet nodes (label + attention-driven styling) and one
  `ToolNodeContent` for drill-down nodes (tool name + call/result/
  subagent kind).
- `flow.request_fit_view()` on first render of each level.
- **Dependency risk, not a blocker:** the workspace pins `ratatui = "0.29"`
  / `crossterm = "0.28"` (root `Cargo.toml`). rataflow's own version
  constraints weren't confirmable ahead of time; `cargo add rataflow` will
  fail fast at resolve time if incompatible, which is an acceptable way to
  discover this.

## Testing

- Unit tests for the new tool-event JSONL parsing (tool_use/tool_result
  pairing, Task-as-subagent detection), in `usage.rs`'s existing test
  style: inline JSON string fixtures written via its `write_jsonl` helper
  into a tempdir, not separate fixture files.
- Manual verification per the original test plan: spawn a couple of real
  sessions via `ninox spawn`, confirm the fleet graph reflects them,
  confirm drilling into a running session shows live-updating tool-call
  nodes, confirm a blocked session is visually distinct and one keypress
  from it reaches its live terminal.

## Acceptance criteria

- New TUI view/mode reachable via a keybinding, discoverable in the TUI's
  existing keybinding/help surface.
- Shows every known orchestrator and its children as a graph, matching
  `ninox list --json` data.
- Any session with `activity == Blocked` (or an outstanding need per
  `needs_you`) is visually unmistakable — distinct from Working/Idle/
  Unknown — with its `activity_note` visible on focus.
- From a highlighted blocked/stuck node, one keypress jumps straight into
  that session's live terminal.
- Selecting a session node reveals its own tool-call/subagent graph
  sourced from its real JSONL transcript, live-updating while running.
- `cargo build`, `cargo test --workspace`, and `cargo clippy --workspace
  --all-targets` all clean.
