# Graph View Live Comms Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Graph tab's fleet view show real inter-session message
deliveries as they happen — a flash on sender and receiver, the connecting
edge animating with direction, and a small log of recent deliveries —
driven only by genuine `deliver_message` calls, never a decorative
always-on animation.

**Architecture:** A new `message_events` SQLite table is the single
source of truth for "what was delivered, by whom, to whom, when",
populated from `ninox_core::messaging::deliver_message`. The TUI's
existing 1s tick polls it, ages a small set of in-flight "flash" timers in
`GraphState`, and `sync_fleet` (already rebuilding the fleet graph fresh
every tick) folds flash state into the `SessionNodeContent`/edge data it
already produces. A new strip in the Graph tab's layout lists the most
recent deliveries.

**Tech Stack:** Rust, rusqlite, ratatui, rataflow 0.1 (vendored source at
`~/.cargo/registry/src/.../rataflow-0.1.0` — read directly when a method's
exact behavior matters; its public docs are accurate and complete).

**Spec:** `docs/superpowers/specs/2026-10-20-graph-view-live-comms-design.md`

## Global Constraints

- Flash window: `FLASH_TICKS = 3` (ticks of the existing 1s refresh, not a
  new animation loop).
- `message_events` pruned to the most recent 200 rows on every insert.
- `recent_events` buffer (feeds the activity panel) capped at 20.
- `delivered_at` / `MessageEvent` timestamps are unix millis (matches
  `inbox.rs`'s `sent_at` and `view::ago`'s `t_ms`), not seconds.
- Do not run `cargo fmt` repo-wide. Run `cargo build`, `cargo test
  --workspace`, and `cargo clippy --workspace --all-targets -- -D
  warnings` before opening the PR.
- Never hand-edit the three derived-from-registry surfaces described in
  this repo's `CLAUDE.md` — not touched by this feature, but don't let a
  stray edit creep in near `capabilities.rs` while working in
  `ninox-core`.

## Review Focus

- **A session renamed or removed between the event being recorded and the
  next tick** (e.g. ended mid-flash): `sync_fleet` must resolve `from`/`to`
  against the *current* tick's `node_ids`, not a stale set, and must not
  panic when an id that was present at ingest time is gone by render time.
  Covered in Task 3's degradation tests.
- **Two message events landing in the same 1s tick** (e.g. a reaction
  firing right after a manual send): `ingest_message_events` must handle a
  `events` slice with more than one element in one call, not just the
  single-event case every other test exercises. Covered in Task 2.
- **A message with no resolvable sender (`from: None`)** — the common case
  for poller-driven reactions — must still flash the receiver and still
  show up in the activity panel with a visible "no sender" rendering,
  not a blank or panicking one. Covered in Tasks 2 and 4.
- **An edge that already has its own non-default style** (the dependency
  edges' dotted stroke from part one) losing that visual identity when a
  flash lands on it: the flash override must preserve "is this a
  dependency edge" (dotted) vs. "is this a hierarchy edge" (solid),
  distinguished via the existing `dep:`-prefixed edge id convention, not
  silently fall back to a plain solid line. Covered in Task 3.
- **A burst of historical rows already in `message_events` from a prior
  process** (the table survives a TUI restart and is pruned to 200, not
  0) must not flash the entire fleet the instant the TUI opens. Covered in
  Task 2's startup-catch-up test.

---

## Task 1: `message_events` table and `Store` API

**Files:**
- Modify: `crates/ninox-core/src/store.rs` (schema in `Store::open`'s
  `execute_batch`, near `session_messages`; new methods near
  `record_message_delivered`/`message_delivered_counts`, ~line 685)
- Modify: `crates/ninox-core/src/types.rs` (new `MessageEvent` struct, near
  `SessionDep`)

**Interfaces:**
- Produces: `ninox_core::types::MessageEvent { id: i64, from_id:
  Option<SessionId>, to_id: SessionId, text: String, delivered_at: i64 }`
  (derive `Debug, Clone, Serialize, Deserialize, PartialEq, Eq`, matching
  `SessionDep`'s derive list).
- Produces: `Store::record_message_event(&self, from: Option<&str>, to:
  &str, text: &str) -> Result<()>`.
- Produces: `Store::recent_message_events(&self, since_id: i64) ->
  Result<Vec<MessageEvent>>` — rows with `id > since_id`, ordered by `id`
  ascending.

- [ ] **Step 1: Write the failing tests**

In `store.rs`'s `#[cfg(test)] mod tests` (same module/fixture style as the
existing `record_message_delivered_counts_per_session` tests nearby):

```rust
#[test]
fn recent_message_events_round_trips_and_orders_by_id() {
    let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
    store.record_message_event(Some("orch"), "worker-1", "first").unwrap();
    store.record_message_event(None, "worker-1", "second").unwrap();
    store.record_message_event(Some("worker-1"), "orch", "third").unwrap();

    let all = store.recent_message_events(0).unwrap();
    assert_eq!(all.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["first", "second", "third"]);
    assert_eq!(all[1].from_id, None);
    assert_eq!(all[2].to_id, "orch");

    let since = store.recent_message_events(all[0].id).unwrap();
    assert_eq!(since.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["second", "third"]);
}

#[test]
fn message_events_are_pruned_to_the_most_recent_200() {
    let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
    for i in 0..250 {
        store.record_message_event(None, "worker-1", &format!("msg {i}")).unwrap();
    }
    let all = store.recent_message_events(0).unwrap();
    assert_eq!(all.len(), 200, "table must be pruned, not grow unbounded");
    assert_eq!(all.last().unwrap().text, "msg 249", "pruning must drop the oldest rows, not the newest");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core --lib store:: -- recent_message_events_round_trips message_events_are_pruned`
Expected: FAIL to compile — `record_message_event`/`recent_message_events`/`MessageEvent` not defined.

- [ ] **Step 3: Add `MessageEvent` to `types.rs`**

Place it directly after `SessionDep` (~line 387), with the same derive
list as `SessionDep` plus the doc-comment convention used there (one
sentence on what the type represents and why it exists separately from
`session_messages`' per-recipient counter).

- [ ] **Step 4: Add the table and the two `Store` methods**

Add the `CREATE TABLE IF NOT EXISTS message_events (...)` DDL (exact
columns in the spec's "Data model" section) to `Store::open`'s
`execute_batch`, after `session_messages`'s own `CREATE TABLE`. Implement
`record_message_event` (compute `delivered_at` via
`SystemTime::now().duration_since(UNIX_EPOCH)` as millis, matching
`inbox::write_message`'s `sent_at` pattern; insert; then prune with
`DELETE FROM message_events WHERE id <= (SELECT MAX(id) FROM
message_events) - 200`) and `recent_message_events` (prepared `SELECT id,
from_id, to_id, text, delivered_at FROM message_events WHERE id > ?1 ORDER
BY id`, mapped into `MessageEvent`), following `query_deps`'s shape for
the query/map split.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox-core --lib store:: -- recent_message_events_round_trips message_events_are_pruned`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/store.rs crates/ninox-core/src/types.rs
git commit -m "feat(ninox-core): add message_events table for live delivery tracking"
```

---

## Task 2: Record every delivery through `deliver_message`

**Files:**
- Modify: `crates/ninox-core/src/messaging.rs` (`deliver_message`, ~line
  80)

**Interfaces:**
- Consumes: `Store::record_message_event` from Task 1.
- Produces: no new public signature — `deliver_message`'s own signature is
  unchanged; this task only changes its body.

- [ ] **Step 1: Write the failing test**

Add to `messaging.rs`'s existing `#[cfg(test)] mod tests`, alongside
`the_inbox_mechanism_writes_the_message_when_target_can_drain`:

```rust
#[tokio::test]
async fn delivering_a_message_records_an_event_not_just_the_counter() {
    let store = Store::open(tempdir().unwrap().keep().join("t.db")).unwrap();
    let ws = tempdir().unwrap().keep();
    write_installed_hooks(&ws);
    store.upsert_session(&session_with("worker-1", "claude-code", Some(ws.to_string_lossy().to_string()))).unwrap();
    let sessions_dir = tempdir().unwrap();

    deliver_message(&store, sessions_dir.path(), "worker-1", "hello worker", SendMechanism::Inbox, Some("orch-1"))
        .await
        .unwrap();

    let events = store.recent_message_events(0).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].from_id.as_deref(), Some("orch-1"));
    assert_eq!(events[0].to_id, "worker-1");
    assert_eq!(events[0].text, "hello worker");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-core --lib messaging:: -- delivering_a_message_records_an_event`
Expected: FAIL — `events.len()` is 0 (nothing records the event yet).

- [ ] **Step 3: Call `record_message_event` in `deliver_message`**

In `deliver_message`, alongside the existing
`store.record_message_delivered(session_id)` call (same
warn-and-continue-on-error treatment — a miss here must never turn an
already-delivered message into an error, matching the existing comment on
that line), add `store.record_message_event(from, session_id, message)`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox-core --lib messaging:: -- delivering_a_message_records_an_event`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/messaging.rs
git commit -m "feat(ninox-core): record a message_events row on every delivery"
```

---

## Task 3: `GraphState` flash lifecycle

**Files:**
- Modify: `crates/ninox-app/src/tui/graph.rs` (`GraphState`,
  `SessionNodeContent`, `sync_fleet`)

**Interfaces:**
- Consumes: `ninox_core::types::MessageEvent` (Task 1);
  `rataflow::{Edge, EdgeMarker, EdgeStyle}` (add `EdgeMarker` to the
  existing `rataflow` import list at the top of the file).
- Produces: `pub enum FlashDirection { Sender, Receiver }` (add
  `Debug, Clone, Copy, PartialEq, Eq`).
- Produces: `SessionNodeContent.flashing: Option<FlashDirection>` — a new
  field; every existing struct-literal site in this file (the production
  site in `sync_fleet` and three test sites: `a_need_only_stuck...`,
  `render_blocked`, `mergeable_renders_...`) must add `flashing: None,`
  except the one this task updates to wire it up for real.
- Produces: `GraphState::ingest_message_events(&mut self, events:
  &[MessageEvent])`.
- Produces: `GraphState::last_seen_event_id(&self) -> i64`.
- Produces: `GraphState::recent_events: VecDeque<MessageEvent>` — made
  `pub` for Task 4's rendering to read.
- Consumes (from `sync_fleet`'s existing body): `node_ids: HashSet<&str>`,
  already computed there for dependency-edge filtering.

- [ ] **Step 1: Write the failing tests**

Add to `graph.rs`'s existing `#[cfg(test)] mod tests`, using the existing
`styled_rows`/`dep` helpers already defined there:

```rust
fn event(id: i64, from: Option<&str>, to: &str, text: &str) -> MessageEvent {
    MessageEvent { id, from_id: from.map(String::from), to_id: to.into(), text: text.into(), delivered_at: 0 }
}

#[test]
fn a_new_message_event_flashes_both_endpoints_then_expires() {
    let mut graph = GraphState::default();
    graph.ingest_message_events(&[]); // startup catch-up: nothing pre-existing
    let rows = styled_rows(&["a", "b"]);
    let palette = Palette::default();

    graph.ingest_message_events(&[event(1, Some("a"), "b", "hi")]);
    graph.sync_fleet(&rows, &palette, &[]);
    {
        let fleet = graph.fleet.borrow();
        assert_eq!(fleet.node("a").unwrap().content.flashing, Some(FlashDirection::Sender));
        assert_eq!(fleet.node("b").unwrap().content.flashing, Some(FlashDirection::Receiver));
    }

    for _ in 0..FLASH_TICKS {
        graph.ingest_message_events(&[]);
    }
    graph.sync_fleet(&rows, &palette, &[]);
    let fleet = graph.fleet.borrow();
    assert_eq!(fleet.node("a").unwrap().content.flashing, None, "flash must expire after its window");
    assert_eq!(fleet.node("b").unwrap().content.flashing, None);
}

#[test]
fn a_message_with_no_sender_still_flashes_the_receiver_alone() {
    let mut graph = GraphState::default();
    graph.ingest_message_events(&[]);
    let rows = styled_rows(&["b"]);
    let palette = Palette::default();

    graph.ingest_message_events(&[event(1, None, "b", "hi")]);
    graph.sync_fleet(&rows, &palette, &[]);

    let fleet = graph.fleet.borrow();
    assert_eq!(fleet.node("b").unwrap().content.flashing, Some(FlashDirection::Receiver));
}

#[test]
fn a_message_whose_endpoint_is_not_in_the_current_fleet_is_skipped_without_panicking() {
    let mut graph = GraphState::default();
    graph.ingest_message_events(&[]);
    let rows = styled_rows(&["a"]);
    let palette = Palette::default();

    graph.ingest_message_events(&[event(1, Some("a"), "gone", "hi")]);
    graph.sync_fleet(&rows, &palette, &[]); // must not panic

    let fleet = graph.fleet.borrow();
    assert_eq!(fleet.node("a").unwrap().content.flashing, None);
}

#[test]
fn ingest_handles_more_than_one_event_in_the_same_tick() {
    let mut graph = GraphState::default();
    graph.ingest_message_events(&[]);
    let rows = styled_rows(&["a", "b", "c"]);
    let palette = Palette::default();

    graph.ingest_message_events(&[event(1, Some("a"), "b", "one"), event(2, Some("b"), "c", "two")]);
    graph.sync_fleet(&rows, &palette, &[]);

    let fleet = graph.fleet.borrow();
    assert_eq!(fleet.node("a").unwrap().content.flashing, Some(FlashDirection::Sender));
    assert_eq!(fleet.node("c").unwrap().content.flashing, Some(FlashDirection::Receiver));
    // b is both this tick's receiver (from "a") and sender (to "c") —
    // either resolved role is acceptable, but it must be Some, not None.
    assert!(fleet.node("b").unwrap().content.flashing.is_some());
}

#[test]
fn a_flash_along_an_existing_dependency_edge_marks_it_animated_and_keeps_its_dotted_style() {
    let mut graph = GraphState::default();
    graph.ingest_message_events(&[]);
    let rows = styled_rows(&["a", "b"]);
    let palette = Palette::default();
    let deps = vec![dep("a", "b", ninox_core::types::DepKind::Declared)];

    graph.ingest_message_events(&[event(1, Some("b"), "a", "reply")]); // reverse of the dep edge's a->b
    graph.sync_fleet(&rows, &palette, &deps);

    let fleet = graph.fleet.borrow();
    let edge = fleet.edges().iter().find(|e| e.connects("a", "b")).expect("edge still present");
    assert!(edge.animated, "a flashed edge must be marked animated");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-app --lib tui::graph:: -- a_new_message_event a_message_with_no_sender a_message_whose_endpoint ingest_handles_more_than_one a_flash_along_an_existing`
Expected: FAIL to compile — `ingest_message_events`, `FlashDirection`, `MessageEvent` import, `FLASH_TICKS` not yet defined; `flashing` field doesn't exist.

- [ ] **Step 3: Implement the flash lifecycle**

Add `use ninox_core::types::MessageEvent;` and `use
std::collections::VecDeque;` (alongside the existing `HashSet` import) and
`EdgeMarker` to the `rataflow` import. Define `FLASH_TICKS: u8 = 3`,
`RECENT_EVENTS_CAP: usize = 20`, the `FlashDirection` enum, and a private
`ActiveFlash { from: Option<String>, to: String, ticks_left: u8 }`. Add
`last_seen_event_id: i64`, `events_initialized: bool`, `active_flashes:
Vec<ActiveFlash>`, and `pub recent_events: VecDeque<MessageEvent>` to
`GraphState` (all covered by its existing `#[derive(Default)]`).

Implement `ingest_message_events` per the spec's "TUI: polling and flash
lifecycle" section: age-and-drop existing flashes first, then for each
event advance `last_seen_event_id`, push to `recent_events` (trimming to
`RECENT_EVENTS_CAP` from the front), and — only when `events_initialized`
was already `true` at the start of this call — push a new `ActiveFlash`
with `ticks_left: FLASH_TICKS`. Set `events_initialized = true`
unconditionally at the end. Add `last_seen_event_id(&self) -> i64`.

- [ ] **Step 4: Wire flashes into `sync_fleet`**

In `sync_fleet`: build a `flash_by_node: HashMap<&str, FlashDirection>`
from `self.active_flashes` filtered against the already-computed
`node_ids` (a `to` match inserts `Receiver`; a `from` match, when
`node_ids` contains it, inserts `Sender`), and pass
`flash_by_node.get(row.id()).copied()` as `SessionNodeContent.flashing`
when building `nodes`. After building `edges` (change its binding to
`let mut edges: Vec<Edge<StepEdge>> = ...`), iterate
`self.active_flashes` again: skip any flash without both endpoints in
`node_ids`; for the rest, find a connecting edge via
`edges.iter_mut().find(|e| e.connects(from, &flash.to))`, set
`edge.animated = true`, and rebuild `edge.content` as a `StepEdge` whose
`EdgeStyle` starts from `EdgeStyle::dotted()` when `edge.id.starts_with("dep:")`
else `EdgeStyle::default()`, with a brightened `with_stroke_style` (e.g.
`palette.working`, bold) and exactly one of `with_marker_start`/
`with_marker_end` set to `EdgeMarker::ArrowClosed` — `marker_start` when
`edge.source == flash.to` (the message traveled opposite the edge's
stored direction), otherwise `marker_end`, with the other explicitly set
to `EdgeMarker::None`.

Update the production `SessionNodeContent { ... }` literal in `sync_fleet`
and the three existing test literals to include the new `flashing` field
(`None` for the three untouched tests).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox-app --lib tui::graph::`
Expected: PASS (full module, including the pre-existing tests —
confirms the new field didn't break anything already covered).

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src/tui/graph.rs
git commit -m "feat(ninox-app): flash fleet-graph nodes and edges on real message delivery"
```

---

## Task 4: Render the flash and the recent-activity panel

**Files:**
- Modify: `crates/ninox-app/src/tui/graph.rs`
  (`SessionNodeContent::render`)
- Modify: `crates/ninox-app/src/tui/layout.rs` (`GraphLayout`, `graph()`)
- Modify: `crates/ninox-app/src/tui/view.rs` (`draw_graph`)
- Modify: `crates/ninox-app/src/tui/mod.rs` (`refresh`)

**Interfaces:**
- Consumes: `GraphState::ingest_message_events`/`last_seen_event_id`/
  `recent_events` (Task 3); `Store::recent_message_events` (Task 1);
  `view::ago`, `view::truncate` (both already defined in `view.rs`).
- Produces: `GraphLayout.activity: Rect`.
- Produces: no new public function in `view.rs` beyond `draw_graph`'s
  existing signature — the activity panel is rendered inline there (plus
  a small private `session_label` helper, since both the detail strip and
  the new panel need "session id → current display name, falling back to
  the raw id").

- [ ] **Step 1: Write the failing test**

Add to `layout.rs`'s test module (find its existing `mod tests` — follow
whatever fixture it already uses to build a `Rect` and call a private
`fn` directly, matching how this file's other layout functions are
tested):

```rust
#[test]
fn graph_layout_reserves_a_recent_activity_strip_below_the_detail_strip() {
    let body = Rect::new(0, 0, 80, 30);
    let gl = graph(body);
    assert!(gl.activity.height > 0);
    assert!(gl.activity.y > gl.detail.y, "activity strip must sit below the detail strip");
    assert_eq!(gl.canvas.height + gl.detail.height + gl.activity.height, body.height);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox-app --lib tui::layout:: -- graph_layout_reserves_a_recent_activity_strip`
Expected: FAIL to compile — `GraphLayout` has no `activity` field yet.

- [ ] **Step 3: Extend `GraphLayout` and `graph()`**

Add `pub activity: Rect` to `GraphLayout`. In `graph()`, keep `detail_h`
as today (`2u16.min(body.height)`), add `activity_h =
6u16.min(body.height.saturating_sub(detail_h))`, shrink `canvas`'s height
by both, and position `detail` directly under `canvas` with `activity`
directly under `detail` (in that top-to-bottom order — the per-node
detail strip stays adjacent to the canvas it describes; the fleet-wide
activity log sits at the very bottom).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p ninox-app --lib tui::layout:: -- graph_layout_reserves_a_recent_activity_strip`
Expected: PASS

- [ ] **Step 5: Poll for events on the existing tick**

In `tui/mod.rs`'s `refresh`, before the existing `st.graph.sync_fleet(...)`
call, add:

```rust
let events = store.recent_message_events(st.graph.last_seen_event_id()).unwrap_or_default();
st.graph.ingest_message_events(&events);
```

- [ ] **Step 6: Render the flash in `SessionNodeContent::render`**

After the existing `stuck`/`style` computation (do not change the
existing `glyph`/`color`/`prefix` logic at all — this is additive only):
when `self.flashing.is_some()`, add `Modifier::BOLD | Modifier::RAPID_BLINK`
to the border `style`. Build the line's spans as today, then insert one
extra `Span` after the status glyph span — before the label span — when
`self.flashing` is `Some`: `"↗ "` styled in `self.palette.working` (bold)
for `Sender`, `"↙ "` for `Receiver`.

- [ ] **Step 7: Render the activity panel in `draw_graph`**

After the existing detail-strip rendering, if `gl.activity.height > 0`:
build up to `gl.activity.height as usize` lines from
`st.graph.recent_events.iter().rev()`, each line `ago(st.now_ms,
ev.delivered_at)` (faint), then `"{from} → {to}  "` using a small
`session_label(st, id)` helper (looks up `st.rows` by id for
`row.session.name`, falling back to the raw id — and to a literal
placeholder like `"?"` when `ev.from_id` is `None`), then
`truncate(&ev.text, remaining_width)`. Render via one `Paragraph::new(lines)`
into `gl.activity`, matching the existing `Paragraph`-per-strip style
already used for `gl.detail`.

- [ ] **Step 8: Run the full workspace test suite, clippy, and build**

Run: `cargo build && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: all PASS, no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-app/src/tui/graph.rs crates/ninox-app/src/tui/layout.rs crates/ninox-app/src/tui/view.rs crates/ninox-app/src/tui/mod.rs
git commit -m "feat(ninox-app): render live message flashes and a recent-activity panel in the Graph tab"
```

---

## Task 5: Manual verification and PR

Not a code task — confirms the four tasks above actually produce the
user-visible behavior the spec describes, since none of the automated
tests render a live terminal frame or drive two real sessions exchanging
a message.

- [ ] **Step 1:** Launch the TUI against a fleet with at least two live
  sessions (or synthesize one via whatever this repo's `run`
  skill/existing dev fixtures already support), send a message between
  them (`ninox send <id> "..."` or let a poller reaction fire), and
  confirm in the Graph tab: both endpoints flash, the edge (if any)
  animates with a visible direction, the flash clears after a few
  seconds, and the message appears in the activity panel.
- [ ] **Step 2:** Restart the TUI with existing rows already in
  `message_events` (from step 1) and confirm it does *not* flash the
  whole fleet on open.
- [ ] **Step 3:** Open a PR (plain descriptive title, no ticket — none
  exists for this change per the task brief) and request a code review.
