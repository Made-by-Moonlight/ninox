# Fleet + Per-Session Flow-Graph Overview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a new TUI-only `View::Graph` that renders the fleet (orchestrators + sessions) as a node graph via `rataflow`, with a per-session drill-down into that session's live tool-call/subagent activity, and makes blocked/stuck sessions visually unmistakable with a one-keypress jump to their live terminal.

**Architecture:** One new `View::Graph`, structured like the existing `tui/brain.rs` self-contained sub-view (data model in `tui/graph.rs`, key handling in `tui/state.rs`, drawing in `tui/view.rs`). Fleet topology is built from the existing `st.rows` (no new backend data). Per-session drill-down extends `ninox-core`'s `usage.rs` JSONL reader with a new tool-event parser and a byte-offset incremental tailer, scoped to only the currently drilled-into session, driven by the TUI's existing 1-second tick.

**Tech Stack:** Rust, `rataflow` 0.1 (ratatui-based node-graph widget), `ratatui` 0.30, `crossterm` 0.29, `serde_json` (already a dependency).

**Spec:** `docs/superpowers/specs/2026-10-19-rataflow-fleet-graph-design.md`

## Global Constraints

- `rataflow` requires `ratatui = "0.30"`; the workspace currently pins `"0.29"`. Task 1 bumps the workspace-wide `ratatui`/`crossterm` pins (only `ninox-app` depends on `ratatui`, confirmed via `grep -rl ratatui crates/*/Cargo.toml`) before any feature code is written, and verifies the existing suite is still green on the bumped versions in isolation.
- Do not run `cargo fmt` on the whole tree — it is not rustfmt-clean and will bury real changes in churn.
- No changes to the `public` git remote; this PR targets the private mirror only.
- Any store/session-store write spanning an `.await` must use read→apply→write — not applicable to this feature (no new store writes; everything here is TUI-local state plus an existing-file read).
- `cargo build`, `cargo test --workspace`, and `cargo clippy --workspace --all-targets` must stay clean after every task.

## Review Focus

- An empty fleet (no sessions/orchestrators yet) must render the graph view without panicking — an empty `Flow` is a valid, expected state, not an error.
- A session with `workspace_path: None` must show a friendly "no transcript" state on drill-down, not panic on an unwrapped `None`.
- A brand-new session with zero turns yet (transcript directory doesn't exist) must drill down to an empty graph with a friendly message, not propagate an `Err`/panic.
- A transcript file being appended to concurrently (a partial, not-yet-newline-terminated trailing line at read time) must not be parsed as JSON and must not be skipped once it completes on a later tick — covered by the incremental tailer only ever advancing its offset past a trailing `\n`.
- A `tool_result` line whose `tool_use_id` doesn't match any `tool_use` seen so far (e.g. tailing started mid-session) must be silently dropped, not panic on a missing lookup.

---

## Task 1: Bump ratatui/crossterm, add rataflow dependency

**Files:**
- Modify: `Cargo.toml:52-53` (workspace `[workspace.dependencies]`)
- Modify: `crates/ninox-app/Cargo.toml` (add `rataflow` dependency)

**Interfaces:**
- Produces: workspace-wide `ratatui = "0.30"`, `crossterm = { version = "0.29", features = ["event-stream"] }`; `ninox-app` gains a direct `rataflow = "0.1"` dependency. No code changes in this task.

- [ ] **Step 1: Bump the workspace pins**

In `Cargo.toml`, change:

```toml
ratatui     = "0.29"
crossterm   = { version = "0.28", features = ["event-stream"] }
```

to:

```toml
ratatui     = "0.30"
crossterm   = { version = "0.29", features = ["event-stream"] }
```

- [ ] **Step 2: Add the `rataflow` dependency**

In `crates/ninox-app/Cargo.toml`, in the `[dependencies]` block, add a line near the existing `ratatui`/`crossterm` lines:

```toml
rataflow = "0.1"
```

- [ ] **Step 3: Build and confirm the version bump alone doesn't break anything**

Run: `cargo build --workspace`
Expected: builds clean. (A prior audit found zero usages in `ninox-app` of the APIs that changed between ratatui 0.29→0.30 — `block::Title`, `Styled` on `Style`, `Flex::SpaceAround`, a custom `Backend` impl, or exhaustive `Marker` matches — so no source changes should be required here. If the build surfaces one of these anyway, fix it in this task, in isolation from the feature code below.)

- [ ] **Step 4: Run the full existing test suite**

Run: `cargo test --workspace`
Expected: all existing tests pass unchanged. This isolates "did the dependency bump break anything" from "does the new feature work," per this task's own test cycle.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean (matching whatever the baseline was before this change — do not introduce new warnings).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ninox-app/Cargo.toml
git commit -m "chore(deps): bump ratatui to 0.30, crossterm to 0.29, add rataflow

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: `View::Graph` navigation skeleton

**Files:**
- Modify: `crates/ninox-app/src/tui/state.rs` (View enum, switch_view, tab_key digit range, command() digit match, HELP, new `graph_key` stub)
- Modify: `crates/ninox-app/src/tui/layout.rs` (TABS array, compute() match arm, header_tabs test)
- Modify: `crates/ninox-app/src/tui/view.rs` (draw() dispatch, draw_footer pill + hints, tab-mnemonic test, every_view_renders test)
- Create: `crates/ninox-app/src/tui/graph.rs` (empty `GraphState` stub for now — filled in Task 3)
- Modify: `crates/ninox-app/src/tui/mod.rs` (register the `graph` module)

**Interfaces:**
- Produces: `View::Graph` variant; `pub struct GraphState` (empty stub, `#[derive(Debug, Default)]`); `TuiState.graph: GraphState` field; a reachable, empty placeholder view with working navigation (no fleet data yet — that's Task 3).

- [ ] **Step 1: Create the `graph` module stub**

Create `crates/ninox-app/src/tui/graph.rs`:

```rust
//! The Graph tab's model: fleet topology and per-session tool-call
//! drill-down, rendered via `rataflow`. Data model only — key handling
//! lives in `state.rs`'s `graph_key`, drawing in `view.rs`'s `draw_graph`,
//! matching the existing split used by the Brain tab.

#[derive(Debug, Default)]
pub struct GraphState {}
```

- [ ] **Step 2: Register the module**

In `crates/ninox-app/src/tui/mod.rs`, find the existing `mod` declarations for the other tui submodules (alongside `mod brain;`, `mod state;`, etc. — grep for `mod brain;` to find the block) and add:

```rust
mod graph;
```

- [ ] **Step 3: Add the `View::Graph` variant**

In `crates/ninox-app/src/tui/state.rs`, change:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Board,
    Overview,
    PrWatches,
    Brain,
    Settings,
}
```

to:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Board,
    Overview,
    PrWatches,
    Brain,
    Settings,
    Graph,
}
```

- [ ] **Step 4: Add the `graph: GraphState` field to `TuiState`**

In `crates/ninox-app/src/tui/state.rs`, add the import `use super::graph::GraphState;` near the existing `pub use super::brain::BrainView;` line, then add a field to the `TuiState` struct right after `pub brain: BrainView,`:

```rust
pub brain: BrainView,
pub graph: GraphState,
```

In `impl Default for TuiState`, add the matching initializer right after `brain: BrainView::default(),`:

```rust
brain: BrainView::default(),
graph: GraphState::default(),
```

- [ ] **Step 5: Extend `switch_view`**

In `crates/ninox-app/src/tui/state.rs`, the `switch_view` match (the one with arms for `View::Board`, `View::Overview`, `View::PrWatches`, `View::Brain`, `View::Settings`) gets a new arm:

```rust
View::Graph => {
    self.view = View::Graph;
    Action::None
}
```

- [ ] **Step 6: Extend `tab_key`'s digit range and `TABS`**

In `crates/ninox-app/src/tui/layout.rs`, change:

```rust
pub const TABS: [(View, &str); 5] = [
    (View::Board, "Fleet"),
    (View::Overview, "Overview"),
    (View::PrWatches, "PRs"),
    (View::Brain, "Brain"),
    (View::Settings, "Settings"),
];
```

to:

```rust
pub const TABS: [(View, &str); 6] = [
    (View::Board, "Fleet"),
    (View::Overview, "Overview"),
    (View::PrWatches, "PRs"),
    (View::Brain, "Brain"),
    (View::Settings, "Settings"),
    (View::Graph, "Graph"),
];
```

In `crates/ninox-app/src/tui/state.rs`, change `tab_key`'s doc comment and range:

```rust
/// `1`..`6` select the header tabs.
fn tab_key(key: &KeyEvent) -> Option<View> {
    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return None;
    }
    let KeyCode::Char(c @ '1'..='6') = key.code else { return None };
    Some(super::layout::TABS[(c as u8 - b'1') as usize].0)
}
```

And in `command()`, change the fallback match arm:

```rust
KeyCode::Char('1'..='6') => {
    if let Some(v) = tab_key(&key) {
        return st.switch_view(v);
    }
}
```

Also add a bare-key toggle mirroring `'o'` for Overview — find the `KeyCode::Char('o') => { ... }` arm in `command()` and add a sibling arm right after it:

```rust
KeyCode::Char('v') => {
    if st.view == View::Graph {
        st.view = View::Board;
    } else {
        st.view = View::Graph;
    }
}
```

- [ ] **Step 7: Add the `graph_key` handler and wire it into `handle_key`**

In `crates/ninox-app/src/tui/state.rs`, add a new function near `overview_key`/`brain_key`:

```rust
fn graph_key(st: &mut TuiState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc | KeyCode::Char('v') => {
            st.view = View::Board;
            Action::None
        }
        KeyCode::Char('?') => {
            st.modal = Some(Modal::Help);
            Action::None
        }
        _ => Action::None,
    }
}
```

In `handle_key`'s main `match st.view { ... }`, add:

```rust
View::Graph => return graph_key(st, key),
```

- [ ] **Step 8: Add a placeholder `draw_graph` and wire the `draw()` dispatch**

In `crates/ninox-app/src/tui/view.rs`, add a new function near `draw_brain`:

```rust
fn draw_graph(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let _ = st;
    f.render_widget(Paragraph::new("Graph view — coming soon"), lay.body);
}
```

In `draw()`'s match, add:

```rust
View::Graph => draw_graph(f, st, lay),
```

- [ ] **Step 9: Extend the footer's mode pill and key-hint matches**

In `crates/ninox-app/src/tui/view.rs`'s `draw_footer`, the pill match gets a new arm:

```rust
(View::Graph, _) => (" GRAPH ", p.ink, p.rule),
```

And the hints match gets a new arm (place it near the `View::Overview` arm):

```rust
(_, View::Graph, _) => vec![("arrows".into(), "move"), ("↵".into(), "open"), ("c".into(), "connect"), ("esc".into(), "back")],
```

- [ ] **Step 10: Extend `compute()`'s view-geometry match**

In `crates/ninox-app/src/tui/layout.rs`'s `compute()`, add an arm so the graph view at least gets `body`'s geometry (no dedicated sub-layout yet — that's Task 3):

```rust
match st.view {
    View::Board => board(&mut lay, body, st),
    View::Overview => overview(&mut lay, body, st),
    View::Brain => lay.brain = Some(brain(body, st)),
    View::Settings => lay.settings = Some(settings(body, st)),
    View::PrWatches => lay.prs = Some(prs(body, st)),
    View::Graph => {}
}
```

- [ ] **Step 11: Add a HELP entry**

In `crates/ninox-app/src/tui/state.rs`'s `HELP` const, add a new group after `"PRs tab (bare keys)"`:

```rust
("Graph tab (bare keys)", &[
    ("v", "toggle the fleet graph from the fleet list"),
    ("arrows / hjkl", "move between nodes"),
    ("↵", "drill into a session's own tool-call graph"),
    ("c", "connect to the highlighted session's live terminal"),
    ("esc", "back (up one level, or to the fleet list)"),
]),
```

Also update the `"From the fleet list (bare keys)"` group's tab-mnemonic line:

```rust
("1-6  (p b s v)", "tabs: fleet · overview · PRs · brain · settings · graph"),
```

- [ ] **Step 12: Update the three existing tests that enumerate views/tabs**

In `crates/ninox-app/src/tui/layout.rs`'s `header_tabs_are_laid_out_in_order_without_overlap` test, change:

```rust
assert_eq!(views, [View::Board, View::Overview, View::PrWatches, View::Brain, View::Settings]);
```

to:

```rust
assert_eq!(views, [View::Board, View::Overview, View::PrWatches, View::Brain, View::Settings, View::Graph]);
```

In `crates/ninox-app/src/tui/view.rs`, the test asserting tab labels in the header row — change:

```rust
for tab in ["1 Fleet", "2 Overview", "3 PRs", "4 Brain", "5 Settings"] {
```

to:

```rust
for tab in ["1 Fleet", "2 Overview", "3 PRs", "4 Brain", "5 Settings", "6 Graph"] {
```

And in `every_view_renders_at_tiny_and_standard_sizes`, change:

```rust
for view in [View::Board, View::Overview, View::PrWatches, View::Brain, View::Settings] {
```

to:

```rust
for view in [View::Board, View::Overview, View::PrWatches, View::Brain, View::Settings, View::Graph] {
```

- [ ] **Step 13: Run the tests**

Run: `cargo test --workspace`
Expected: all pass, including the three updated tests and the fuzz-style `every_view_renders_at_tiny_and_standard_sizes` now covering `View::Graph` at every tested size (including `(1, 1)`) without panicking.

- [ ] **Step 14: Commit**

```bash
git add crates/ninox-app/src/tui/
git commit -m "feat(tui): add empty View::Graph navigation skeleton

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: Fleet-topology rendering

**Files:**
- Modify: `crates/ninox-app/src/tui/graph.rs` (real `GraphState`, `SessionNodeContent`, `sync_fleet`)
- Modify: `crates/ninox-app/src/tui/mod.rs` (call `sync_fleet` from `refresh`)
- Modify: `crates/ninox-app/src/tui/layout.rs` (dedicated `GraphLayout`, `graph()` layout fn)
- Modify: `crates/ninox-app/src/tui/view.rs` (real `draw_graph`)

**Interfaces:**
- Consumes: `Row` (`session`, `is_orchestrator`, `group`), `Attention`, `Need` (from `state.rs`), `Palette` (from `palette.rs`), `attention_glyph(Attention, &Palette) -> (&'static str, Color)` (from `view.rs`, already `pub`).
- Produces: `pub struct SessionNodeContent { label, attention, need, activity_note, palette }` implementing `rataflow::NodeContent`; `GraphState::sync_fleet(&self, rows: &[(Row, Attention, Option<Need>)], palette: &Palette)`; `GraphState::focused_session_id(&self) -> Option<String>`.

- [ ] **Step 1: Write `SessionNodeContent` and `sync_fleet`**

Replace `crates/ninox-app/src/tui/graph.rs` with:

```rust
//! The Graph tab's model: fleet topology and per-session tool-call
//! drill-down, rendered via `rataflow`. Data model only — key handling
//! lives in `state.rs`'s `graph_key`, drawing in `view.rs`'s `draw_graph`,
//! matching the existing split used by the Brain tab.

use std::cell::RefCell;

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use rataflow::{Edge, Flow, Node, NodeContent, NodeRenderContext, StepEdge, Sugiyama};

use super::palette::Palette;
use super::state::{Attention, Need, Row};
use super::view::attention_glyph;

#[derive(Debug, Clone)]
pub struct SessionNodeContent {
    pub label: String,
    pub attention: Attention,
    pub need: Option<Need>,
    pub palette: Palette,
}

impl SessionNodeContent {
    /// A session is "stuck" — the one state this whole view exists to make
    /// unmissable — when it's self-declared `Blocked`, or has anything
    /// else that needs a human (failed CI, needs review, etc).
    fn stuck(&self) -> bool {
        self.attention == Attention::Blocked || self.need.is_some()
    }
}

impl NodeContent for SessionNodeContent {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let (glyph, color) = attention_glyph(self.attention, &self.palette);
        let mut style = Style::default().fg(color);
        if self.stuck() {
            style = style.add_modifier(Modifier::BOLD);
        }
        if ctx.selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let block = Block::default().borders(Borders::ALL).border_style(style);
        let text = format!("{glyph} {}", self.label);
        Paragraph::new(text).style(Style::default().fg(self.palette.ink)).block(block).render(ctx.area, buf);
    }
}

#[derive(Debug, Default)]
pub struct GraphState {
    pub fleet: RefCell<Flow<SessionNodeContent, StepEdge>>,
}

impl GraphState {
    /// Rebuilds the fleet graph from the same per-row data the Board/
    /// Overview tabs already compute — no new backend queries.
    pub fn sync_fleet(&self, rows: &[(Row, Attention, Option<Need>)], palette: &Palette) {
        let nodes: Vec<Node<SessionNodeContent>> = rows
            .iter()
            .map(|(row, attention, need)| {
                Node::new(
                    row.id().to_string(),
                    (0.0, 0.0),
                    (24.0, 3.0),
                    SessionNodeContent {
                        label: row.session.name.clone(),
                        attention: *attention,
                        need: *need,
                        palette: *palette,
                    },
                )
            })
            .collect();
        let edges: Vec<Edge<StepEdge>> = rows
            .iter()
            .filter(|(row, _, _)| !row.is_orchestrator)
            .filter_map(|(row, _, _)| {
                row.group.as_ref().map(|g| Edge::new(format!("{g}->{}", row.id()), g.clone(), row.id().to_string()))
            })
            .collect();
        if let Ok(mut flow) = Flow::with_graph(nodes, edges) {
            flow.apply_layout(Sugiyama::vertical());
            *self.fleet.borrow_mut() = flow;
        }
    }

    /// The currently-selected fleet node's session id, if any.
    pub fn focused_session_id(&self) -> Option<String> {
        self.fleet.borrow().first_selected_node_id()
    }
}
```

`Flow::first_selected_node_id` already returns an owned `Option<String>` (not `Option<&str>`), so no extra `.map(str::to_string)` is needed here or at any other call site in this plan.

- [ ] **Step 2: Call `sync_fleet` from `refresh()`**

In `crates/ninox-app/src/tui/mod.rs`'s `refresh()`, right after the existing `st.apply_rows(build_rows(crate::group_sessions(sessions, orchs)));` line, add:

```rust
let styled: Vec<(Row, Attention, Option<Need>)> =
    st.rows.iter().map(|r| (r.clone(), st.attention(r), st.needs_you(r))).collect();
st.graph.sync_fleet(&styled, &st.palette);
```

Add `Attention, Need` to the existing `use super::state::{...}` import list at the top of `mod.rs` if they aren't already imported there (check the existing import line first — `Row` is almost certainly already imported since `build_rows` returns `Vec<Row>`).

- [ ] **Step 3: Add a dedicated `GraphLayout`**

In `crates/ninox-app/src/tui/layout.rs`, add near the other per-view layout structs (`BrainLayout`, `PrLayout`):

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphLayout {
    pub canvas: Rect,
    /// One-line strip under the canvas showing the focused node's need/note.
    pub detail: Rect,
}

fn graph(body: Rect) -> GraphLayout {
    let detail_h = 1u16.min(body.height);
    GraphLayout {
        canvas: Rect { height: body.height.saturating_sub(detail_h), ..body },
        detail: Rect { y: body.bottom().saturating_sub(detail_h), height: detail_h, ..body },
    }
}
```

Add `pub graph: Option<GraphLayout>,` to the `Layout` struct next to `pub brain: Option<BrainLayout>,`.

Change the `compute()` arm from Task 2's placeholder:

```rust
View::Graph => {}
```

to:

```rust
View::Graph => lay.graph = Some(graph(body)),
```

- [ ] **Step 4: Write the real `draw_graph`**

In `crates/ninox-app/src/tui/view.rs`, replace the Task 2 placeholder:

```rust
fn draw_graph(f: &mut Frame, st: &TuiState, lay: &Layout) {
    let Some(gl) = lay.graph else { return };
    {
        let mut fleet = st.graph.fleet.borrow_mut();
        f.render_widget(&mut *fleet, gl.canvas);
    }
    let session_id = st.graph.focused_session_id();
    let detail = session_id
        .and_then(|id| st.rows.iter().find(|r| r.id() == id).cloned())
        .map(|row| {
            let need = st.needs_you(&row).map(|n| n.label());
            let note = row.session.activity_note.clone();
            match (need, note) {
                (Some(n), Some(note)) => format!("{n} — {note}"),
                (Some(n), None) => n,
                (None, Some(note)) => note,
                (None, None) => String::new(),
            }
        })
        .unwrap_or_default();
    f.render_widget(Paragraph::new(detail).style(Style::default().fg(st.palette.ink_2)), gl.detail);
}
```

- [ ] **Step 5: Write a fleet-graph rendering test**

In `crates/ninox-app/src/tui/view.rs`'s test module, add a test alongside the other `draw_*` tests (use whatever helper the existing tests use to build a populated `TuiState` — likely `fleet()`, seen earlier in `every_view_renders_at_tiny_and_standard_sizes`):

```rust
#[test]
fn graph_view_renders_fleet_nodes_without_panicking() {
    let mut st = fleet();
    st.view = View::Graph;
    let styled: Vec<(super::state::Row, super::state::Attention, Option<super::state::Need>)> =
        st.rows.iter().map(|r| (r.clone(), st.attention(r), st.needs_you(r))).collect();
    st.graph.sync_fleet(&styled, &st.palette);
    let out = render(&st, 120, 30);
    assert!(!out.trim().is_empty());
}

#[test]
fn graph_view_renders_with_an_empty_fleet() {
    let mut st = TuiState::default();
    st.view = View::Graph;
    let out = render(&st, 80, 24);
    let _ = out; // must not panic
}
```

(Adjust the exact helper names — `fleet()` / `render(&st, w, h)` — to whatever this file's existing tests actually call; they're used identically a few lines above in `every_view_renders_at_tiny_and_standard_sizes`.)

- [ ] **Step 6: Run the tests**

Run: `cargo test --workspace`
Expected: all pass, including the two new tests. The empty-fleet case is this task's entry in the plan's Review Focus list — it must not panic on a `Flow` with zero nodes.

- [ ] **Step 7: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-app/src/tui/
git commit -m "feat(tui): render fleet topology in the graph view

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: Fleet-graph interaction — navigate, drill-down trigger, connect-to-terminal

**Files:**
- Modify: `crates/ninox-app/src/tui/state.rs` (`graph_key`, forwarding keys to `rataflow`)
- Modify: `crates/ninox-app/src/tui/graph.rs` (`drilled_into` field, `drill_into`/`back_to_fleet` stubs — real drill-down content lands in Task 6)

**Interfaces:**
- Consumes: `rataflow::Flow::handle_key_event(impl Into<rataflow::KeyEvent>) -> EventResponse`, `Flow::first_selected_node_id() -> Option<String>`, `Flow::select_node(&mut self, id: &str)`.
- Produces: `GraphState.drilled_into: Option<String>`; `GraphState::drill_into(&mut self, session_id: &str)` / `back_to_fleet(&mut self)` (stubs for now, real behavior in Task 6); arrow/hjkl navigation and node selection within the fleet graph; `c` jumps to the selected session's live terminal; `Enter` on a session node marks it drilled-into (rendering of that is Task 6, but the state transition and Esc-back round-trip are covered here).

- [ ] **Step 1: Add `drilled_into` and drill transition stubs to `GraphState`**

In `crates/ninox-app/src/tui/graph.rs`, add to the `GraphState` struct:

```rust
#[derive(Debug, Default)]
pub struct GraphState {
    pub fleet: RefCell<Flow<SessionNodeContent, StepEdge>>,
    pub drilled_into: Option<String>,
}
```

And add methods:

```rust
impl GraphState {
    pub fn drill_into(&mut self, session_id: &str) {
        self.drilled_into = Some(session_id.to_string());
    }

    pub fn back_to_fleet(&mut self) {
        self.drilled_into = None;
    }
}
```

- [ ] **Step 2: Write a failing test for key-forwarding and the connect action**

In `crates/ninox-app/src/tui/state.rs`'s test module, add (using `Row::test_row`, the `#[cfg(test)]` constructor already defined on `Row` itself near the top of this file, and `key(char)`, the existing single-char `KeyEvent` builder already used by the neighboring digit-tab tests around line 2506):

```rust
#[test]
fn graph_c_connects_to_the_selected_fleet_node() {
    let mut st = TuiState::default();
    st.rows = vec![Row::test_row("a"), Row::test_row("b")];
    st.view = View::Graph;
    let styled: Vec<(Row, Attention, Option<Need>)> = st.rows.iter().map(|r| (r.clone(), st.attention(r), st.needs_you(r))).collect();
    st.graph.sync_fleet(&styled, &st.palette);
    st.graph.fleet.borrow_mut().select_node("a");

    let action = handle_key(&mut st, key('c'));

    assert_eq!(st.view, View::Board);
    assert_eq!(st.selected, 0);
    // `open_selected` for a live session returns Action::Connect or Action::None
    // depending on backend — assert it's not an unrelated action:
    assert!(matches!(action, Action::Connect(_) | Action::None));
}

#[test]
fn graph_esc_leaves_the_graph_view() {
    let mut st = TuiState::default();
    st.rows = vec![Row::test_row("a")];
    st.view = View::Graph;
    assert_eq!(handle_key(&mut st, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Action::None);
    assert_eq!(st.view, View::Board);
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test --workspace graph_c_connects_to_the_selected_fleet_node`
Expected: FAIL — `graph_key` doesn't yet forward keys to `rataflow` or handle `'c'`/`Enter`.

- [ ] **Step 4: Implement key forwarding and the connect/drill actions**

Replace `graph_key` in `crates/ninox-app/src/tui/state.rs`:

```rust
fn graph_key(st: &mut TuiState, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            if st.graph.drilled_into.is_some() {
                st.graph.back_to_fleet();
            } else {
                st.view = View::Board;
            }
            Action::None
        }
        KeyCode::Char('v') if st.graph.drilled_into.is_none() => {
            st.view = View::Board;
            Action::None
        }
        KeyCode::Char('?') => {
            st.modal = Some(Modal::Help);
            Action::None
        }
        KeyCode::Char('c') => {
            let target = st.graph.drilled_into.clone().or_else(|| st.graph.focused_session_id());
            let Some(id) = target else { return Action::None };
            let Some(idx) = st.rows.iter().position(|r| r.id() == id) else { return Action::None };
            st.selected = idx;
            st.view = View::Board;
            st.open_selected()
        }
        KeyCode::Enter if st.graph.drilled_into.is_none() => {
            let Some(id) = st.graph.focused_session_id() else { return Action::None };
            let is_session = st.rows.iter().find(|r| r.id() == id).is_some_and(|r| !r.is_orchestrator);
            if is_session {
                st.graph.drill_into(&id);
            }
            Action::None
        }
        _ => {
            let flow_key: rataflow::KeyEvent = key.into();
            st.graph.fleet.borrow_mut().handle_key_event(flow_key);
            Action::None
        }
    }
}
```

**Compatibility note:** `key.into()` relies on `ratatui`'s `crossterm` feature re-exporting the exact same `crossterm` crate instance this codebase depends on directly, so `rataflow`'s `From<ratatui::crossterm::event::KeyEvent>` impl also covers `crossterm::event::KeyEvent` values built here. If this doesn't type-check, run `cargo tree -i crossterm` to check whether Cargo resolved two separate `crossterm` versions; if so, write a small manual mapping function `fn to_rataflow_key(key: crossterm::event::KeyEvent) -> rataflow::KeyEvent` instead, translating `key.code`/`key.modifiers` field-by-field into `rataflow::KeyCode`/`rataflow::Modifiers`, and call that in place of `.into()`.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --workspace graph_c_connects_to_the_selected_fleet_node graph_esc_leaves_the_graph_view`
Expected: PASS.

- [ ] **Step 6: Run the full suite and clippy**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: both clean.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-app/src/tui/
git commit -m "feat(tui): graph view keyboard navigation and connect-to-terminal

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 5: JSONL tool-event extraction and incremental tailing

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/usage.rs`

**Interfaces:**
- Produces:
  - `pub struct ToolEvent { pub id: String, pub name: String, pub is_subagent: bool, pub result_ok: Option<bool> }`
  - `pub enum ToolLine { Use(ToolEvent), Result { tool_use_id: String, ok: bool } }`
  - `pub fn parse_tool_line(line: &str) -> Vec<ToolLine>`
  - `pub fn tail_new_tool_lines(path: &Path, offset: &mut u64) -> Vec<ToolLine>`
  - `pub fn latest_transcript_path(workspace: &str) -> Option<PathBuf>`

- [ ] **Step 1: Write the failing tests for tool-event line parsing**

In `crates/ninox-core/src/lifecycle/usage.rs`'s test module, add:

```rust
#[test]
fn parse_tool_line_extracts_tool_use_blocks() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"doing it"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
    let events = parse_tool_line(line);
    assert_eq!(events, vec![ToolLine::Use(ToolEvent { id: "t1".into(), name: "Bash".into(), is_subagent: false, result_ok: None })]);
}

#[test]
fn parse_tool_line_marks_task_tool_use_as_subagent() {
    let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Task","input":{"prompt":"explore"}}]}}"#;
    let events = parse_tool_line(line);
    assert_eq!(events, vec![ToolLine::Use(ToolEvent { id: "t2".into(), name: "Task".into(), is_subagent: true, result_ok: None })]);
}

#[test]
fn parse_tool_line_extracts_tool_result_blocks() {
    let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file1\nfile2","is_error":false}]}}"#;
    let events = parse_tool_line(line);
    assert_eq!(events, vec![ToolLine::Result { tool_use_id: "t1".into(), ok: true }]);
}

#[test]
fn parse_tool_line_marks_error_results() {
    let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"boom","is_error":true}]}}"#;
    let events = parse_tool_line(line);
    assert_eq!(events, vec![ToolLine::Result { tool_use_id: "t1".into(), ok: false }]);
}

#[test]
fn parse_tool_line_ignores_unrelated_lines() {
    assert_eq!(parse_tool_line(r#"{"type":"system","subtype":"turn_duration"}"#), vec![]);
    assert_eq!(parse_tool_line("not even json"), vec![]);
    assert_eq!(parse_tool_line(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#), vec![]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --workspace -p ninox-core parse_tool_line`
Expected: FAIL to compile — `ToolEvent`, `ToolLine`, `parse_tool_line` don't exist yet.

- [ ] **Step 3: Implement `ToolEvent`, `ToolLine`, and `parse_tool_line`**

Add to `crates/ninox-core/src/lifecycle/usage.rs`, near `parse_turn` (not modifying it — a sibling parser over the same line shape, reading `message.content` instead of `message.usage`):

```rust
/// One `tool_use` invocation seen in a session's transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolEvent {
    pub id: String,
    pub name: String,
    /// `true` when `name == "Task"` — a subagent spawn rather than a plain tool call.
    pub is_subagent: bool,
    /// `None` until a matching `tool_result` line has been seen.
    pub result_ok: Option<bool>,
}

/// One significant line from a transcript's tool-call stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolLine {
    Use(ToolEvent),
    Result { tool_use_id: String, ok: bool },
}

/// Extracts `tool_use`/`tool_result` content blocks from one transcript
/// line. Reads `message.content`, which `parse_turn` ignores entirely —
/// the two parsers read disjoint parts of the same line shape.
pub fn parse_tool_line(line: &str) -> Vec<ToolLine> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return Vec::new() };
    let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) else { return Vec::new() };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => content
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .filter_map(|b| {
                let id = b.get("id")?.as_str()?.to_string();
                let name = b.get("name")?.as_str()?.to_string();
                let is_subagent = name == "Task";
                Some(ToolLine::Use(ToolEvent { id, name, is_subagent, result_ok: None }))
            })
            .collect(),
        Some("user") => content
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
            .filter_map(|b| {
                let tool_use_id = b.get("tool_use_id")?.as_str()?.to_string();
                let ok = !b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
                Some(ToolLine::Result { tool_use_id, ok })
            })
            .collect(),
        _ => Vec::new(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace -p ninox-core parse_tool_line`
Expected: PASS.

- [ ] **Step 5: Write the failing tests for incremental tailing**

Add to the same test module:

```rust
#[test]
fn tail_new_tool_lines_reads_only_whats_new_since_the_last_offset() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}
"#,
    )
    .unwrap();

    let mut offset = 0u64;
    let first = tail_new_tool_lines(&path, &mut offset);
    assert_eq!(first, vec![ToolLine::Use(ToolEvent { id: "t1".into(), name: "Bash".into(), is_subagent: false, result_ok: None })]);
    assert!(offset > 0);

    // Nothing new yet — must not re-emit the same line.
    assert_eq!(tail_new_tool_lines(&path, &mut offset), vec![]);

    // Append a second line; only the new one should come back.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}}]}}}}"#).unwrap();

    let second = tail_new_tool_lines(&path, &mut offset);
    assert_eq!(second, vec![ToolLine::Result { tool_use_id: "t1".into(), ok: true }]);
}

#[test]
fn tail_new_tool_lines_does_not_consume_a_partial_trailing_line() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    // No trailing newline: an in-progress write.
    std::fs::write(&path, r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","#).unwrap();

    let mut offset = 0u64;
    assert_eq!(tail_new_tool_lines(&path, &mut offset), vec![]);
    assert_eq!(offset, 0, "an unterminated line must not be consumed yet");

    // The write completes.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, r#""name":"Bash","input":{{}}}}]}}}}"#).unwrap();

    let events = tail_new_tool_lines(&path, &mut offset);
    assert_eq!(events, vec![ToolLine::Use(ToolEvent { id: "t1".into(), name: "Bash".into(), is_subagent: false, result_ok: None })]);
}

#[test]
fn tail_new_tool_lines_returns_empty_for_a_missing_file() {
    let mut offset = 0u64;
    assert_eq!(tail_new_tool_lines(&Path::new("/does/not/exist.jsonl"), &mut offset), vec![]);
}
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cargo test --workspace -p ninox-core tail_new_tool_lines`
Expected: FAIL to compile — `tail_new_tool_lines` doesn't exist yet.

- [ ] **Step 7: Implement `tail_new_tool_lines` and `latest_transcript_path`**

Add to `crates/ninox-core/src/lifecycle/usage.rs`:

```rust
/// Reads whatever has been appended to `path` since `*offset`, advances
/// `*offset` past the last complete line, and parses each complete line
/// with [`parse_tool_line`]. A trailing line with no `\n` yet (the
/// transcript is still being written) is left unconsumed and re-read on
/// the next call once it completes — never parsed truncated.
pub fn tail_new_tool_lines(path: &Path, offset: &mut u64) -> Vec<ToolLine> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let Ok(len) = file.metadata().map(|m| m.len()) else { return Vec::new() };
    if len < *offset {
        // Truncated or replaced since we last read it — start over.
        *offset = 0;
    }
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    let consumed = buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
    *offset += consumed as u64;
    buf[..consumed].lines().flat_map(parse_tool_line).collect()
}

/// The `*.jsonl` transcript Ninox should treat as a session's *current*
/// conversation: the most recently modified one in its project directory.
/// (`ingest_dir` above deliberately sums every file for lifetime cost;
/// the live tool-call view wants just the active conversation.)
pub fn latest_transcript_path(workspace: &str) -> Option<PathBuf> {
    let dir = claude_projects_dir().join(claude_project_slug(workspace));
    let entries = std::fs::read_dir(&dir).ok()?;
    entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --workspace -p ninox-core`
Expected: all pass, including every test in this file (the new ones plus the pre-existing usage tests, unaffected by this addition since `parse_turn`/`ingest_dir` weren't touched).

- [ ] **Step 9: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean.

- [ ] **Step 10: Commit**

```bash
git add crates/ninox-core/src/lifecycle/usage.rs
git commit -m "feat(usage): extract tool-call events from transcripts with incremental tailing

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 6: Per-session drill-down

**Files:**
- Modify: `crates/ninox-app/src/tui/graph.rs` (`ToolNodeContent`, `TailState`, real `drill_into`/`back_to_fleet`/`tick_drilldown`)
- Modify: `crates/ninox-app/src/tui/state.rs` (`graph_key`'s `Enter` arm passes the session's workspace; forward keys to the drilldown `Flow` when drilled in)
- Modify: `crates/ninox-app/src/tui/view.rs` (`draw_graph` renders the drilldown `Flow` when drilled in)
- Modify: `crates/ninox-app/src/tui/mod.rs` (tick the drilldown tailer every 1s, only while drilled in)

**Interfaces:**
- Consumes: `ninox_core::lifecycle::usage::{latest_transcript_path, tail_new_tool_lines, ToolLine, ToolEvent}`.
- Produces: `GraphState::drill_into(&mut self, session_id: &str, workspace: Option<&str>)`; `GraphState::tick_drilldown(&mut self)`; `GraphState.drilldown: RefCell<Option<Flow<ToolNodeContent, StepEdge>>>`.

- [ ] **Step 1: Add `ToolNodeContent`, `TailState`, and the real drill-down methods**

In `crates/ninox-app/src/tui/graph.rs`, add the imports `use ninox_core::lifecycle::usage::{latest_transcript_path, tail_new_tool_lines, ToolEvent, ToolLine};` and `use std::path::PathBuf;`, then:

```rust
#[derive(Debug, Clone)]
pub struct ToolNodeContent {
    pub name: String,
    pub is_subagent: bool,
    pub result_ok: Option<bool>,
    pub palette: Palette,
}

impl NodeContent for ToolNodeContent {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let (glyph, color) = match self.result_ok {
            Some(true) => ("✓", self.palette.working),
            Some(false) => ("✗", self.palette.ci_failed),
            None => ("…", self.palette.faint),
        };
        let mut style = Style::default().fg(color);
        if ctx.selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let block = Block::default().borders(Borders::ALL).border_style(style);
        let kind = if self.is_subagent { "subagent" } else { "tool" };
        let text = format!("{glyph} {kind}: {}", self.name);
        Paragraph::new(text).style(Style::default().fg(self.palette.ink)).block(block).render(ctx.area, buf);
    }
}

#[derive(Debug)]
struct TailState {
    path: PathBuf,
    offset: u64,
    events: Vec<ToolEvent>,
}
```

Change the `GraphState` struct (from Task 4) to add the two new fields, `drilldown` and `tail`:

```rust
#[derive(Debug, Default)]
pub struct GraphState {
    pub fleet: RefCell<Flow<SessionNodeContent, StepEdge>>,
    pub drilled_into: Option<String>,
    pub drilldown: RefCell<Option<Flow<ToolNodeContent, StepEdge>>>,
    tail: Option<TailState>,
}
```

Leave `sync_fleet` and `focused_session_id` (added in Task 3) exactly as they are. Replace Task 4's stub `drill_into`/`back_to_fleet` methods with the real versions, and add `tick_drilldown`/`rebuild_drilldown` alongside them:

```rust
impl GraphState {
    /// Enters drill-down for `session_id`. `workspace` is that session's
    /// `workspace_path`; `None` (never spawned with a recorded workspace)
    /// or no transcript yet both produce an empty drill-down rather than
    /// an error — there's simply nothing to show yet.
    pub fn drill_into(&mut self, session_id: &str, workspace: Option<&str>) {
        self.drilled_into = Some(session_id.to_string());
        self.tail = workspace
            .and_then(latest_transcript_path)
            .map(|path| TailState { path, offset: 0, events: Vec::new() });
        self.rebuild_drilldown();
    }

    pub fn back_to_fleet(&mut self) {
        self.drilled_into = None;
        self.tail = None;
        *self.drilldown.borrow_mut() = None;
    }

    /// Called from the TUI's existing 1s tick, only while drilled in —
    /// re-tails just this one session's transcript, not the whole fleet.
    pub fn tick_drilldown(&mut self) {
        let Some(tail) = self.tail.as_mut() else { return };
        let lines = tail_new_tool_lines(&tail.path, &mut tail.offset);
        if lines.is_empty() {
            return;
        }
        for line in lines {
            match line {
                ToolLine::Use(ev) => tail.events.push(ev),
                ToolLine::Result { tool_use_id, ok } => {
                    if let Some(ev) = tail.events.iter_mut().find(|e| e.id == tool_use_id) {
                        ev.result_ok = Some(ok);
                    }
                }
            }
        }
        self.rebuild_drilldown();
    }

    fn rebuild_drilldown(&mut self) {
        let Some(tail) = &self.tail else {
            *self.drilldown.borrow_mut() = None;
            return;
        };
        let palette = self.fleet.borrow().node(&self.drilled_into.clone().unwrap_or_default())
            .map(|n| n.content.palette)
            .unwrap_or_default();
        let nodes: Vec<Node<ToolNodeContent>> = tail
            .events
            .iter()
            .map(|ev| {
                Node::new(
                    ev.id.clone(),
                    (0.0, 0.0),
                    (24.0, 3.0),
                    ToolNodeContent { name: ev.name.clone(), is_subagent: ev.is_subagent, result_ok: ev.result_ok, palette },
                )
            })
            .collect();
        let edges: Vec<Edge<StepEdge>> = tail
            .events
            .windows(2)
            .map(|w| Edge::new(format!("{}->{}", w[0].id, w[1].id), w[0].id.clone(), w[1].id.clone()))
            .collect();
        if let Ok(mut flow) = Flow::with_graph(nodes, edges) {
            flow.apply_layout(Sugiyama::vertical());
            *self.drilldown.borrow_mut() = Some(flow);
        }
    }
}
```

`Palette` needs a `Default` impl for the `.unwrap_or_default()` fallback above — check `crates/ninox-app/src/tui/palette.rs` for an existing `impl Default for Palette` (the rest of `TuiState`'s own `Default` impl uses `Palette::default()`, confirming one already exists) — no change needed there.

- [ ] **Step 2: Pass the session's workspace into `drill_into`**

In `crates/ninox-app/src/tui/state.rs`'s `graph_key`, change the `Enter` arm:

```rust
KeyCode::Enter if st.graph.drilled_into.is_none() => {
    let Some(id) = st.graph.focused_session_id() else { return Action::None };
    let Some(row) = st.rows.iter().find(|r| r.id() == id) else { return Action::None };
    if !row.is_orchestrator {
        let workspace = row.session.workspace_path.clone();
        st.graph.drill_into(&id, workspace.as_deref());
    }
    Action::None
}
```

- [ ] **Step 3: Forward keys to the drilldown `Flow` when drilled in**

In `graph_key`'s fallback arm, branch on `drilled_into`:

```rust
_ => {
    let flow_key: rataflow::KeyEvent = key.into();
    if st.graph.drilled_into.is_some() {
        if let Some(flow) = st.graph.drilldown.borrow_mut().as_mut() {
            flow.handle_key_event(flow_key);
        }
    } else {
        st.graph.fleet.borrow_mut().handle_key_event(flow_key);
    }
    Action::None
}
```

- [ ] **Step 4: Render the drilldown in `draw_graph`**

In `crates/ninox-app/src/tui/view.rs`'s `draw_graph`, replace the canvas-rendering block:

```rust
{
    if let Some(id) = &st.graph.drilled_into {
        let mut dd = st.graph.drilldown.borrow_mut();
        if let Some(flow) = dd.as_mut() {
            f.render_widget(flow, gl.canvas);
        } else {
            let label = st.rows.iter().find(|r| r.id() == *id).map(|r| r.session.name.as_str()).unwrap_or(id.as_str());
            f.render_widget(Paragraph::new(format!("no transcript yet for {label}")), gl.canvas);
        }
    } else {
        let mut fleet = st.graph.fleet.borrow_mut();
        f.render_widget(&mut *fleet, gl.canvas);
    }
}
```

- [ ] **Step 5: Tick the drilldown tailer**

In `crates/ninox-app/src/tui/mod.rs`'s main loop, in the `_ = tick.tick() => { ... }` branch, add a line next to the existing `refresh(st, store);` call:

```rust
_ = tick.tick() => {
    refresh(st, store);
    st.graph.tick_drilldown();
    ...
```

- [ ] **Step 6: Write a drill-down test using a real fixture transcript**

In `crates/ninox-app/src/tui/graph.rs`'s test module (add one if none exists yet, following the `#[cfg(test)] mod tests { use super::*; ... }` convention used in `usage.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn drill_into_builds_a_graph_from_the_sessions_transcript() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{}}}}]}}}}"#).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}}]}}}}"#).unwrap();
        drop(f);

        let mut graph = GraphState::default();
        // Bypass workspace→transcript resolution for this unit test by
        // seeding `tail` directly through `drill_into`'s own code path is
        // not possible without a real `claude_projects_dir()` tree, so
        // this test instead drives `tick_drilldown` after pointing `tail`
        // at the fixture file directly.
        graph.tail = Some(TailState { path, offset: 0, events: Vec::new() });
        graph.drilled_into = Some("s1".into());
        graph.tick_drilldown();

        assert!(graph.drilldown.borrow().is_some());
    }

    #[test]
    fn drill_into_a_session_with_no_workspace_shows_no_transcript() {
        let mut graph = GraphState::default();
        graph.drill_into("s1", None);
        assert!(graph.drilldown.borrow().is_none());
    }
}
```

(`TailState`'s fields are private to this module, so this test must live inside `graph.rs` itself to construct one directly, as written above.)

- [ ] **Step 7: Run the tests**

Run: `cargo test --workspace`
Expected: all pass, including the two new drill-down tests — the second one is this task's entry in the plan's Review Focus list (`workspace_path: None` must not panic).

- [ ] **Step 8: Run clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-app/src/tui/
git commit -m "feat(tui): per-session tool-call drill-down, live-tailed

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 7: Final integration pass

**Files:**
- Modify: `crates/ninox-app/src/tui/state.rs` (HELP wording touch-up if needed after real usage)
- No new files.

**Interfaces:** None new — this task is verification only.

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace`
Expected: clean.

- [ ] **Step 2: Full workspace test suite**

Run: `cargo test --workspace`
Expected: clean, including every test added across Tasks 1–6.

- [ ] **Step 3: Full workspace clippy**

Run: `cargo clippy --workspace --all-targets`
Expected: clean.

- [ ] **Step 4: Manual verification**

Per the ticket's own test plan:
1. Spawn at least two real sessions under an orchestrator: `ninox spawn <name> --prompt "..."` (or whatever the existing `ninox spawn` invocation looks like in this environment).
2. Open the TUI, press `v` (or `6`) to reach the Graph view — confirm every known orchestrator and session appears as a node, matching `ninox list --json`.
3. Mark one session blocked: `ninox worker-status set blocked --note "waiting on X"` from inside that session (or however this repo's existing worker-status tooling triggers it) — confirm that node becomes visually distinct in the graph and its note shows in the detail strip when focused.
4. With the blocked node focused, press `c` — confirm it jumps straight into that session's live terminal.
5. Select a running (non-orchestrator) session node and press `Enter` — confirm it drills into that session's own tool-call graph, and that new tool calls appear live as the session keeps working (no need to leave and re-enter the drill-down to see new nodes).
6. Press `Esc` twice — confirm it returns from drill-down to fleet view, then from fleet view back to the fleet list (Board).

- [ ] **Step 5: Commit any touch-ups from manual verification**

If manual verification surfaces wording or minor behavior fixes, commit them:

```bash
git add -A
git commit -m "polish(tui): graph view touch-ups from manual verification

Co-Authored-By: Claude <noreply@anthropic.com>"
```

(Skip this step entirely if nothing needed changing.)
