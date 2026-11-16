# Fleet Graph Node Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add three node-rendering features to the ninox-tui fleet graph: a
compact/minimal node display toggle, a depth-cap reporting indicator (red
ring), and a machine-label badge on remote sessions — entirely within
`ninox-tui`'s graph rendering code.

**Architecture:** `GraphState` (`crates/ninox-tui/src/tui/graph.rs`) gains a
`compact: bool` view flag, toggled by a new key in the Graph tab's key
handler and reflected in the footer hints. `SessionNodeContent` and
`GhostNodeContent` each gain a `compact: bool` field and branch their
`NodeContent::render` between the existing full-card layout and a new
single-line layout. `SessionNodeContent` also gains `depth_capped: bool`
(real depth computed via the already-existing `ninox_overview::tree::OverviewTree`,
thresholded against a `DEFAULT_DEPTH_CAP` constant) and `machine_id:
Option<String>` (read straight from `Session.machine_id`), both rendered as
extra spans/border-color changes. No changes to `ninox-overview`'s crate —
`RealNode`'s `depth`/`breadcrumb`/`depth_capped` fields already exist; this
plan is the first thing to populate them with real data instead of the
hardcoded placeholders in `overview_from_rows`.

**Tech Stack:** Rust, ratatui, rataflow (vendored crates.io dep, `Node`
width/height are per-node fields, independently settable).

**Spec:** `docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`
— "Node content and appearance" and "Nested orchestration and the depth
cap" sections. Ticket: MLOPS-5313.

## Global Constraints

- Depth cap default is 3 (spec: "Depth is hard-capped, default 3,
  configurable"). This plan implements the default as a fixed constant;
  making it configurable via `AppConfig` is out of scope (see Open
  Questions below) — this ticket is graph-rendering-only.
- The compact toggle is "a view over the same node data; nothing is lost
  switching between them except on-canvas detail" (spec) — no content
  field is removed in compact mode, only hidden/abbreviated in rendering.
- Ghost nodes must stay dashed/distinct in **both** card and compact mode
  (spec, "Plan review (ghost nodes)").
- The depth-cap ring is "independent of card/compact mode" (spec) — must
  render in both.
- Scope is localized to graph rendering in `ninox-tui`; do not touch the
  ninox-prompt wiring or the worker-types screen (parallel work by other
  workers on this same v1 branch).

## Review Focus

- A node with `machine_id: None` (the common local-session case) must
  render byte-for-byte as it did before this change — no stray badge or
  trailing whitespace. (Task 3 test.)
- A node that is simultaneously `depth_capped` and `Blocked`/`Mergeable`
  (both want to set the border/status color) must still read clearly —
  depth-cap takes the border, the stuck glyph/color logic is unchanged.
  (Task 3 test.)
- Toggling compact mode while a ghost plan-review is active must keep
  ghost nodes visually distinct, not collapse them into a blank line.
  (Task 4 test.)
- A session name long enough to overflow the compact card's width must
  not panic — ratatui's `Paragraph` truncates; verify with a
  narrower-than-label `Rect`. (Task 3 test.)
- A multi-level `Row.group` chain (even though today's spawn model never
  actually produces one beyond 1 level) must still compute `depth`/
  `depth_capped` correctly if it ever occurs — tested directly via
  synthetic rows, since the underlying depth-walk is generic. (Task 1
  test.)

---

### Task 1: Depth-cap and machine-id data on `SessionNodeContent`

**Files:**
- Modify: `crates/ninox-tui/src/tui/graph.rs`

**Interfaces:**
- Produces: `SessionNodeContent.depth_capped: bool`, `SessionNodeContent.machine_id: Option<String>` — consumed by Task 3's render logic.
- Produces: `const DEFAULT_DEPTH_CAP: u32 = 3;` — consumed nowhere else, but keep it `pub(crate)`-visible-by-default (private is fine, same module).

- [ ] **Step 1: Add the two new fields to `SessionNodeContent` and update every existing struct literal**

  Add to the struct (near `dep_count`):
  ```rust
  pub depth_capped: bool,
  pub machine_id: Option<String>,
  ```
  Grep `SessionNodeContent {` in this file — update every literal (the
  production one in `sync_fleet`'s node-building closure, and every test
  literal: `a_need_only_stuck_session_renders_with_the_accent_color`,
  `render_blocked`'s helper, `mergeable_renders_with_the_mergeable_color_not_accent`,
  `a_node_with_dependencies_renders_a_badge_instead_of_needing_a_graph_edge`,
  `dummy_session_content()`) to add `depth_capped: false, machine_id:
  None,` so the crate still compiles. Don't change behavior yet — `render`
  doesn't read these fields until Task 3.

- [ ] **Step 2: Add `DEFAULT_DEPTH_CAP` and import `OverviewTree`**

  Near the other tuning constants at the top of the file, add:
  ```rust
  /// Spec default: "Depth is hard-capped, default 3, configurable." This
  /// ticket implements the rendering side only — see the plan's Open
  /// Questions for why it's a fixed constant, not a config value.
  const DEFAULT_DEPTH_CAP: u32 = 3;
  ```
  Add `use ninox_overview::tree::OverviewTree;` to the existing
  `ninox_overview::` import block.

- [ ] **Step 3: Write the failing test for `overview_from_rows`' depth-cap math**

  Add (near the other `Row`-building test helpers):
  ```rust
  #[test]
  fn overview_from_rows_marks_a_node_at_or_past_the_depth_cap() {
      // A chain deeper than today's orchestrator->worker model ever
      // produces in production, but the depth walk itself (OverviewTree,
      // reused unchanged from ninox-overview) is generic over however many
      // hierarchy edges exist — this proves the threshold math on top of
      // it is correct regardless.
      let rows = vec![
          (orchestrator_row("o"), Attention::Working, None),
          (worker_row("w1", "o"), Attention::Working, None),
          (worker_row("w2", "w1"), Attention::Working, None),
          (worker_row("w3", "w2"), Attention::Working, None),
          (worker_row("w4", "w3"), Attention::Working, None),
      ];
      let (nodes, _edges) = overview_from_rows(&rows);
      let depth_capped = |id: &str| -> bool {
          nodes.iter().find_map(|n| match n {
              OverviewNode::Real(r) if r.id == id => Some(r.depth_capped),
              _ => None,
          }).unwrap()
      };
      assert!(!depth_capped("o"), "depth 0 must not be capped");
      assert!(!depth_capped("w1"), "depth 1 must not be capped");
      assert!(!depth_capped("w2"), "depth 2 must not be capped");
      assert!(depth_capped("w3"), "depth 3 == DEFAULT_DEPTH_CAP must be capped");
      assert!(depth_capped("w4"), "depth 4 past the cap must be capped");
  }
  ```

- [ ] **Step 4: Run it, confirm it fails**

  Run: `cargo test -p ninox-tui overview_from_rows_marks_a_node_at_or_past_the_depth_cap`
  Expected: FAIL (every `depth_capped` is hardcoded `false` today).

- [ ] **Step 5: Implement the depth computation in `overview_from_rows`**

  After building `nodes`/`edges` (the existing logic, unchanged) and
  before returning them, compute the tree and patch each real node:
  ```rust
  let tree = OverviewTree::build(&nodes, &edges);
  for node in &mut nodes {
      if let OverviewNode::Real(r) = node {
          let depth = tree.depth.get(r.id.as_str()).copied().unwrap_or(0);
          r.depth = depth;
          r.breadcrumb = tree.breadcrumb.get(r.id.as_str()).cloned().unwrap_or_default();
          r.depth_capped = depth >= DEFAULT_DEPTH_CAP;
      }
  }
  ```
  Change `nodes` from `let nodes: Vec<OverviewNode> = ...` to `let mut
  nodes: Vec<OverviewNode> = ...` to allow the in-place patch. Return
  `(nodes, edges)` as before.

- [ ] **Step 6: Run the test again, confirm it passes**

  Run: `cargo test -p ninox-tui overview_from_rows_marks_a_node_at_or_past_the_depth_cap`
  Expected: PASS

- [ ] **Step 7: Write the failing test for `sync_fleet`'s wiring of `depth_capped` and `machine_id` onto the rendered content**

  ```rust
  #[test]
  fn sync_fleet_wires_depth_capped_and_machine_id_onto_session_content() {
      let mut graph = GraphState::default();
      let rows = vec![
          (orchestrator_row("o"), Attention::Working, None),
          (worker_row("w1", "o"), Attention::Working, None),
          (worker_row("w2", "w1"), Attention::Working, None),
          (worker_row("w3", "w2"), Attention::Working, None),
      ];
      let palette = Palette::default();
      graph.sync_fleet(&rows, &palette, &[]);
      let fleet = graph.fleet.borrow();
      assert!(session_content(&fleet, "w3").depth_capped, "depth 3 must flow through to the rendered content");
      assert!(!session_content(&fleet, "o").depth_capped);
      assert_eq!(session_content(&fleet, "o").machine_id, None, "a local session must carry no machine badge");
  }

  #[test]
  fn sync_fleet_wires_a_remote_sessions_machine_id_onto_its_content() {
      let mut graph = GraphState::default();
      let mut row = worker_row("w", "o");
      row.session.machine_id = Some("prof1".into());
      let rows = vec![(orchestrator_row("o"), Attention::Working, None), (row, Attention::Working, None)];
      let palette = Palette::default();
      graph.sync_fleet(&rows, &palette, &[]);
      let fleet = graph.fleet.borrow();
      assert_eq!(session_content(&fleet, "w").machine_id, Some("prof1".to_string()));
  }
  ```

- [ ] **Step 8: Run both, confirm they fail**

  Run: `cargo test -p ninox-tui sync_fleet_wires_`
  Expected: both FAIL (the `nodes` built in `sync_fleet` for the rataflow
  `Flow` don't read `depth_capped`/`machine_id` yet).

- [ ] **Step 9: Reorder `sync_fleet` to compute `overview_from_rows` first, and wire the two fields**

  Move the `let (real_nodes, real_edges) = overview_from_rows(rows);` call
  (currently after the `Flow::with_graph(...)` block, near the end of the
  function) up to just after `let group_of: HashMap<...> = ...;` near the
  top. Immediately after it, build a lookup:
  ```rust
  let depth_capped_by_id: HashMap<&str, bool> = real_nodes.iter().filter_map(|n| match n {
      OverviewNode::Real(r) => Some((r.id.as_str(), r.depth_capped)),
      _ => None,
  }).collect();
  ```
  In the `nodes: Vec<Node<OverviewNodeContent>>` construction closure, add:
  ```rust
  depth_capped: depth_capped_by_id.get(row.id()).copied().unwrap_or(false),
  machine_id: row.session.machine_id.clone(),
  ```
  Near the bottom of the function, where `let (real_nodes, real_edges) =
  overview_from_rows(rows);` used to be called a second time before
  `OverviewState::new(real_nodes, real_edges)` — delete that now-duplicate
  call and reuse the `real_nodes`/`real_edges` bindings from the top of
  the function instead (the borrow from `depth_capped_by_id` ends well
  before this point, so moving `real_nodes`/`real_edges` into
  `OverviewState::new` here is fine under NLL).

- [ ] **Step 10: Run the tests again, confirm they pass**

  Run: `cargo test -p ninox-tui sync_fleet_wires_`
  Expected: both PASS

- [ ] **Step 11: Run the full existing graph.rs test suite to check for regressions**

  Run: `cargo test -p ninox-tui graph::`
  Expected: all PASS (this reorder must not change any existing
  flash/ghost/dep-badge behavior — those tests don't touch
  `depth_capped`/`machine_id` and should be unaffected).

- [ ] **Step 12: Commit**

  ```bash
  git add crates/ninox-tui/src/tui/graph.rs
  git commit -m "feat(tui): compute real depth-cap and wire machine_id onto fleet graph nodes"
  ```

---

### Task 2: Compact/minimal toggle infrastructure

**Files:**
- Modify: `crates/ninox-tui/src/tui/graph.rs` (new field + size constants)
- Modify: `crates/ninox-tui/src/tui/state.rs` (keybinding + footer hint)

**Interfaces:**
- Consumes: nothing new from Task 1.
- Produces: `GraphState.compact: bool`, `const FULL_CARD_SIZE: (f64, f64)`,
  `const COMPACT_CARD_SIZE: (f64, f64)` — consumed by Task 3 (session node
  sizing/rendering) and Task 4 (ghost node sizing/rendering).

- [ ] **Step 1: Add the `compact` field and size constants in `graph.rs`**

  Add near `DEFAULT_DEPTH_CAP`:
  ```rust
  /// Default full-card node size, used by `sync_fleet` and `sync_ghost_nodes`.
  const FULL_CARD_SIZE: (f64, f64) = (24.0, 3.0);
  /// Compact/minimal node size — a single text row, no border (spec:
  /// "a small status+harness dot with just the name").
  const COMPACT_CARD_SIZE: (f64, f64) = (18.0, 1.0);
  ```
  Add `pub compact: bool,` to `GraphState` (near `prompt_editing`) — no
  `Default` impl change needed, `#[derive(Default)]` already covers it
  (defaults `false`).

- [ ] **Step 2: Use the size constants in the two node-construction sites**

  In `sync_fleet`'s `nodes: Vec<Node<OverviewNodeContent>>` closure, replace
  the literal `(24.0, 3.0)` with:
  ```rust
  let size = if self.compact { COMPACT_CARD_SIZE } else { FULL_CARD_SIZE };
  ```
  computed once before the `.map(...)` call, and pass `size` into
  `Node::new(row.id().to_string(), (0.0, 0.0), size, ...)`. Leave
  `rebuild_drilldown`'s `ToolNodeContent` sizing (also `(24.0, 3.0)`)
  untouched — compact mode only applies to the fleet overview, not the
  per-session tool-call drilldown.

- [ ] **Step 3: Write a failing test asserting the toggle changes node size**

  ```rust
  #[test]
  fn compact_mode_shrinks_the_rendered_node_size() {
      let mut graph = GraphState::default();
      let rows = styled_rows(&["a"]);
      let palette = Palette::default();
      graph.sync_fleet(&rows, &palette, &[]);
      assert_eq!((graph.fleet.borrow().node("a").unwrap().width, graph.fleet.borrow().node("a").unwrap().height), FULL_CARD_SIZE);

      graph.compact = true;
      graph.sync_fleet(&rows, &palette, &[]);
      assert_eq!((graph.fleet.borrow().node("a").unwrap().width, graph.fleet.borrow().node("a").unwrap().height), COMPACT_CARD_SIZE);
  }
  ```

- [ ] **Step 4: Run it, confirm it fails, then passes after Step 2's change**

  Run: `cargo test -p ninox-tui compact_mode_shrinks_the_rendered_node_size`
  Expected: FAIL before Step 2 is wired, PASS after (if you did Step 2
  first, just confirm PASS directly).

- [ ] **Step 5: Add the `m` keybinding in `graph_key` (`crates/ninox-tui/src/tui/state.rs`)**

  Add an arm alongside the other ungated graph-tab toggles (e.g. near the
  `'v'`/`'p'` arms, guarded the same way):
  ```rust
  KeyCode::Char('m') if st.graph.drilled_into.is_none() => {
      st.graph.compact = !st.graph.compact;
      Action::None
  }
  ```

- [ ] **Step 6: Add a footer hint in `graph_footer_hints`**

  In the function body (state.rs, the `graph_footer_hints` helper), push
  a hint reflecting the current state, guarded the same way the other
  graph-only hints are (`if st.graph.drilled_into.is_none()`):
  ```rust
  hints.push(("m".into(), if st.graph.compact { "full cards" } else { "compact" }));
  ```

- [ ] **Step 7: Write a failing test for the keybinding**

  In state.rs's test module, mirroring the existing `TuiState { rows:
  vec![Row::test_row("a")], view: View::Graph, ..Default::default() }` +
  `key(c)` (state.rs:2415) setup used by the other Graph-tab tests around
  state.rs:2450-2482:
  ```rust
  #[test]
  fn m_toggles_compact_mode_on_the_graph_tab() {
      let mut st = TuiState { rows: vec![Row::test_row("a")], view: View::Graph, ..Default::default() };
      assert!(!st.graph.compact);
      handle_key(&mut st, key('m'));
      assert!(st.graph.compact);
      handle_key(&mut st, key('m'));
      assert!(!st.graph.compact);
  }
  ```

- [ ] **Step 8: Run it, confirm it fails, then implement and confirm it passes**

  Run: `cargo test -p ninox-tui m_toggles_compact_mode_on_the_graph_tab`
  Expected: FAIL before Step 5, PASS after.

- [ ] **Step 9: Commit**

  ```bash
  git add crates/ninox-tui/src/tui/graph.rs crates/ninox-tui/src/tui/state.rs
  git commit -m "feat(tui): add compact/minimal fleet-graph node toggle"
  ```

---

### Task 3: Render the depth-cap ring, machine badge, and compact session card

**Files:**
- Modify: `crates/ninox-tui/src/tui/graph.rs` (`SessionNodeContent::render` only)

**Interfaces:**
- Consumes: `SessionNodeContent.{compact, depth_capped, machine_id}` from Tasks 1–2.

- [ ] **Step 1: Write the failing tests**

  ```rust
  fn render_session(content: &SessionNodeContent, area: ratatui::layout::Rect) -> (ratatui::style::Color, String) {
      let mut buf = Buffer::empty(area);
      let ctx = NodeRenderContext {
          id: "n", area, selected: false, dragging: false,
          position_absolute: Position::new(0.0, 0.0),
          theme: rataflow::Theme::default(), animation_phase: 0,
      };
      content.render(&ctx, &mut buf);
      let border_color = buf[(area.x, area.y)].fg;
      let rendered: String = (0..area.height)
          .map(|y| (0..area.width).map(|x| buf[(area.x + x, area.y + y)].symbol().to_string()).collect::<String>())
          .collect::<Vec<_>>().join("\n");
      (border_color, rendered)
  }

  #[test]
  fn depth_capped_full_card_renders_a_red_ring_border() {
      let palette = Palette::default();
      let content = SessionNodeContent { label: "w".into(), attention: Attention::Working, need: None, activity_note: None, palette, flashing: None, dep_count: 0, compact: false, depth_capped: true, machine_id: None };
      let (border_color, _) = render_session(&content, ratatui::layout::Rect::new(0, 0, 24, 3));
      assert_eq!(border_color, palette.ci_failed);
  }

  #[test]
  fn depth_capped_overrides_a_stuck_nodes_border_color_too() {
      let palette = Palette::default();
      let content = SessionNodeContent { label: "w".into(), attention: Attention::Blocked, need: Some(Need::Mergeable), activity_note: None, palette, flashing: None, dep_count: 0, compact: false, depth_capped: true, machine_id: None };
      let (border_color, _) = render_session(&content, ratatui::layout::Rect::new(0, 0, 24, 3));
      assert_eq!(border_color, palette.ci_failed, "depth-cap must win over the stuck/mergeable color");
  }

  #[test]
  fn machine_id_renders_a_badge_when_present_and_nothing_when_absent() {
      let palette = Palette::default();
      let with_machine = SessionNodeContent { label: "w".into(), attention: Attention::Working, need: None, activity_note: None, palette, flashing: None, dep_count: 0, compact: false, depth_capped: false, machine_id: Some("prof1".into()) };
      let (_, rendered) = render_session(&with_machine, ratatui::layout::Rect::new(0, 0, 24, 3));
      assert!(rendered.contains("@prof1"), "rendered: {rendered:?}");

      let local = SessionNodeContent { machine_id: None, ..with_machine.clone() };
      let (_, rendered) = render_session(&local, ratatui::layout::Rect::new(0, 0, 24, 3));
      assert!(!rendered.contains('@'), "a local session must render no machine badge: {rendered:?}");
  }

  #[test]
  fn compact_mode_renders_a_single_line_with_no_border_and_no_need_prefix() {
      let palette = Palette::default();
      let content = SessionNodeContent { label: "worker".into(), attention: Attention::Blocked, need: Some(Need::Mergeable), activity_note: None, palette, flashing: None, dep_count: 0, compact: true, depth_capped: false, machine_id: None };
      let (_, rendered) = render_session(&content, ratatui::layout::Rect::new(0, 0, 18, 1));
      assert!(rendered.contains("worker"), "rendered: {rendered:?}");
      assert!(!rendered.contains("mergeable:"), "compact mode must drop the need/blocked-kind prefix, just the name: {rendered:?}");
      assert!(!rendered.contains('│') && !rendered.contains('┌'), "compact mode must render no border: {rendered:?}");
  }

  #[test]
  fn compact_mode_shows_the_depth_cap_marker_and_machine_badge_too() {
      let palette = Palette::default();
      let content = SessionNodeContent { label: "w".into(), attention: Attention::Working, need: None, activity_note: None, palette, flashing: None, dep_count: 0, compact: true, depth_capped: true, machine_id: Some("prof1".into()) };
      let (_, rendered) = render_session(&content, ratatui::layout::Rect::new(0, 0, 24, 1));
      assert!(rendered.contains('⛔'), "rendered: {rendered:?}");
      assert!(rendered.contains("@prof1"), "rendered: {rendered:?}");
  }
  ```

- [ ] **Step 2: Run the new tests, confirm they fail**

  Run: `cargo test -p ninox-tui depth_capped_full_card_renders_a_red_ring_border depth_capped_overrides_a_stuck_nodes_border_color_too machine_id_renders_a_badge_when_present_and_nothing_when_absent compact_mode_renders_a_single_line_with_no_border_and_no_need_prefix compact_mode_shows_the_depth_cap_marker_and_machine_badge_too`
  Expected: all FAIL (none of this rendering exists yet).

- [ ] **Step 3: Implement `SessionNodeContent::render`**

  Keep the existing `glyph`/`color`/`stuck`/`blocked_kind` computation
  unchanged. The existing border-style chain is:
  ```rust
  let mut style = Style::default().fg(color);
  if stuck {
      style = style.add_modifier(Modifier::BOLD);
  }
  if self.flashing.is_some() {
      style = style.add_modifier(Modifier::BOLD | Modifier::RAPID_BLINK);
  }
  if ctx.selected {
      style = style.add_modifier(Modifier::REVERSED);
  }
  ```
  Change only its first two lines — keep the `flashing`/`selected`
  modifier accumulation exactly as-is, since the border must still react
  to those independently of depth-cap:
  ```rust
  let border_color = if self.depth_capped { self.palette.ci_failed } else { color };
  let mut style = Style::default().fg(border_color);
  if stuck || self.depth_capped {
      style = style.add_modifier(Modifier::BOLD);
  }
  ```

  Build the depth-cap marker span and machine badge span once, right
  after that block:
  ```rust
  let cap_marker = self.depth_capped.then(|| Span::styled("⛔ ", Style::default().fg(self.palette.ci_failed).add_modifier(Modifier::BOLD)));
  let machine_badge = self.machine_id.as_deref().map(|id| Span::styled(format!(" @{id}"), Style::default().fg(self.palette.faint)));
  ```

  Add, before the existing `let block = ...` line, a compact branch that
  returns early (this one intentionally does *not* reuse `style` — a
  compact node has no border, so none of the border-only modifiers
  apply):
  ```rust
  if self.compact {
      let mut spans = vec![Span::styled(format!("{glyph} "), Style::default().fg(color))];
      if let Some(marker) = cap_marker.clone() {
          spans.push(marker);
      }
      spans.push(Span::styled(self.label.clone(), Style::default().fg(self.palette.ink)));
      if let Some(badge) = machine_badge.clone() {
          spans.push(badge);
      }
      if self.dep_count > 0 {
          spans.push(Span::styled(format!(" ⛓{}", self.dep_count), Style::default().fg(self.palette.review)));
      }
      Paragraph::new(Line::from(spans)).render(ctx.area, buf);
      return;
  }
  ```
  The existing full-card path still builds `Block::default().borders(Borders::ALL).border_style(style)` — unchanged syntactically, it now just carries the (possibly depth-cap-red) `style` computed above. Insert `cap_marker` into `spans` right after the glyph span (before the existing `flash_marker` push), and insert `machine_badge` into `spans` right after the label push (before the `dep_count` badge push).

- [ ] **Step 4: Run the tests again, confirm they pass**

  Run: `cargo test -p ninox-tui depth_capped_full_card_renders_a_red_ring_border depth_capped_overrides_a_stuck_nodes_border_color_too machine_id_renders_a_badge_when_present_and_nothing_when_absent compact_mode_renders_a_single_line_with_no_border_and_no_need_prefix compact_mode_shows_the_depth_cap_marker_and_machine_badge_too`
  Expected: all PASS

- [ ] **Step 5: Run the full graph.rs suite for regressions**

  Run: `cargo test -p ninox-tui graph::`
  Expected: all PASS, including every pre-existing test (`blocked_asking_a_question_renders_its_own_glyph_color_and_prefix`, `mergeable_renders_with_the_mergeable_color_not_accent`, etc.) — none of them set `depth_capped`/`compact`, so they exercise the unchanged full-card, non-capped path.

- [ ] **Step 6: Commit**

  ```bash
  git add crates/ninox-tui/src/tui/graph.rs
  git commit -m "feat(tui): render depth-cap ring, machine badge, and compact session cards"
  ```

---

### Task 4: Ghost node compact support

**Files:**
- Modify: `crates/ninox-tui/src/tui/graph.rs` (`GhostNodeContent` struct, `render`, and `sync_ghost_nodes`)

**Interfaces:**
- Consumes: `GraphState.compact`, `FULL_CARD_SIZE`/`COMPACT_CARD_SIZE` (Task 2).

- [ ] **Step 1: Add `compact: bool` to `GhostNodeContent` and update its one existing construction site + test literal**

  Add the field to the struct. In `sync_ghost_nodes`, where
  `GhostNodeContent { label: ..., worker_type: ..., palette: self.palette
  }` is built, add `compact: self.compact,`. Update the `GhostNodeContent`
  literal in `toggle_demo_plan_adds_ghost_nodes_with_the_ghost_badge_and_dashed_border`'s
  retrieval (it clones an existing one via pattern match, so no literal to
  edit there — only `sync_ghost_nodes`'s own construction needs it).

- [ ] **Step 2: Use the size constants in `sync_ghost_nodes`'s `Node::new` call**

  Replace the literal `(24.0, 3.0)` passed to `Node::new(g.id.clone(),
  (0.0, 0.0), (24.0, 3.0), content)` with `if self.compact {
  COMPACT_CARD_SIZE } else { FULL_CARD_SIZE }`.

- [ ] **Step 3: Write the failing test for compact ghost rendering**

  ```rust
  #[test]
  fn toggle_demo_plan_in_compact_mode_renders_a_distinct_single_line_marker() {
      let mut graph = GraphState::default();
      let rows = vec![(orchestrator_row("o"), Attention::Working, None)];
      let palette = Palette::default();
      graph.compact = true;
      graph.sync_fleet(&rows, &palette, &[]);

      graph.toggle_demo_plan("o");

      let fleet = graph.fleet.borrow();
      let ghost = fleet.nodes().find_map(|n| match &n.content {
          OverviewNodeContent::Ghost(g) => Some(g.clone()),
          _ => None,
      }).expect("a ghost node content must be present");
      assert!(ghost.compact, "ghost built while graph.compact is true must itself be compact");

      let area = ratatui::layout::Rect::new(0, 0, 24, 1);
      let mut buf = Buffer::empty(area);
      let ctx = NodeRenderContext { id: "n", area, selected: false, dragging: false, position_absolute: Position::new(0.0, 0.0), theme: rataflow::Theme::default(), animation_phase: 0 };
      OverviewNodeContent::Ghost(ghost).render(&ctx, &mut buf);
      let rendered: String = (0..area.width).map(|x| buf[(area.x + x, area.y)].symbol().to_string()).collect();
      assert!(rendered.to_uppercase().contains("GHOST"), "compact ghost must still be unmistakably marked: {rendered:?}");
  }
  ```

- [ ] **Step 4: Run it, confirm it fails**

  Run: `cargo test -p ninox-tui toggle_demo_plan_in_compact_mode_renders_a_distinct_single_line_marker`
  Expected: FAIL

- [ ] **Step 5: Implement the compact branch in `GhostNodeContent::render`**

  Before the existing `Block::default()...` line, add:
  ```rust
  if self.compact {
      let worker_type = self.worker_type.clone().unwrap_or_else(|| "unassigned".into());
      let spans = vec![
          Span::styled("┄GHOST┄ ", Style::default().fg(self.palette.accent).add_modifier(Modifier::BOLD)),
          Span::styled(format!("{worker_type}: {}", self.label), Style::default().fg(self.palette.faint).add_modifier(Modifier::ITALIC)),
      ];
      Paragraph::new(Line::from(spans)).render(ctx.area, buf);
      return;
  }
  ```
  Leave the rest of the function (the full dashed-card path) unchanged.

- [ ] **Step 6: Run the test again, confirm it passes**

  Run: `cargo test -p ninox-tui toggle_demo_plan_in_compact_mode_renders_a_distinct_single_line_marker`
  Expected: PASS

- [ ] **Step 7: Run the full graph.rs suite for regressions**

  Run: `cargo test -p ninox-tui graph::`
  Expected: all PASS, including the original
  `toggle_demo_plan_adds_ghost_nodes_with_the_ghost_badge_and_dashed_border`
  (full-card/non-compact default path, unchanged).

- [ ] **Step 8: Commit**

  ```bash
  git add crates/ninox-tui/src/tui/graph.rs
  git commit -m "feat(tui): render ghost plan nodes distinctly in compact mode"
  ```

---

### Task 5: Full verification pass

**Files:** none (verification only).

- [ ] **Step 1: Format only the files this plan touched**

  Run: `rustfmt --edition 2021 crates/ninox-tui/src/tui/graph.rs crates/ninox-tui/src/tui/state.rs`
  (Not `cargo fmt` over the whole tree/package — per this repo's CLAUDE.md,
  the tree isn't rustfmt-clean and a wholesale pass would bury this
  change's diff in unrelated churn.)

- [ ] **Step 2: Lint**

  Run: `cargo clippy --workspace --all-targets -- -D warnings`
  Expected: no warnings from the touched files.

- [ ] **Step 3: Full test suite**

  Run: `cargo test --workspace`
  Expected: all PASS.

- [ ] **Step 4: Manual sanity pass (if a dev build is feasible in this environment)**

  Run the TUI against a fleet with at least one orchestrator + worker,
  press `m` on the Graph tab, confirm the footer hint and card size
  change; confirm a session with `machine_id` set (or toggle one via
  `ninox machine` fixtures) shows the `@`-badge.

## Open Questions

- **Depth-cap configurability**: the spec says "default 3, configurable."
  This plan hardcodes `DEFAULT_DEPTH_CAP = 3` with no config knob, since
  (a) this ticket is scoped to graph rendering only, and (b) today's spawn
  model (orchestrator → worker, no nested sub-orchestrators) never
  actually produces a depth past 1, so there's no live code path to wire
  a config value into yet. Picked as the most spec-consistent option
  available without expanding scope into orchestrator-spawn logic; a
  config knob is a natural follow-up once nested orchestration exists.
- **Machine-label badge content**: the ticket says "a machine-label badge
  on remote sessions (Session.machine_id)." `Session.machine_id` is an
  opaque id referencing a `MachineProfile.id`, not its human-readable
  `MachineProfile.label` — resolving to the label would require threading
  `AppConfig`'s `remote_machines` list into the graph-rendering call path
  (currently it isn't there), which is more plumbing than "localized to
  graph rendering" implies. This plan renders the raw `machine_id` string
  directly, matching how the rest of ninox-tui already surfaces
  `machine_id` (e.g. `connect.rs` uses the raw id, with no existing
  label-resolution helper anywhere in the TUI to reuse).
