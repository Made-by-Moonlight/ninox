# Graph View: Live Inter-Session Communication

**Status:** Implemented alongside this spec (see the plan this links to).

## Context

Part one of this feature (merged, commit `9fcf32b`, PR #184) gave the
Graph tab's fleet view distinct glyphs for blocked/asking/awaiting-permission
sessions, its own color for "mergeable", and a second, visually distinct
edge kind for `session_deps` (`EdgeStyle::dotted()` in
`GraphState::sync_fleet`, `crates/ninox-app/src/tui/graph.rs`). That made
fleet *state* legible at a glance. This part makes fleet *communication*
legible: when one session actually delivers a message to another — via
`ninox send`, a poller reaction, or any other path through
`ninox_core::messaging::deliver_message` — the Graph tab should show it
happening: a brief flash on the sender, the connecting edge lighting up
and animating, a brief flash on the receiver, and the delivery logged in a
small activity panel. It must be driven by real, discrete delivery events —
one flash per actual message — never a decorative, always-on animation
implying constant traffic.

## Data model

A new `message_events` table (`crates/ninox-core/src/store.rs`) is a
separate, purpose-built event log for this feature — distinct from the
existing `session_messages` table, which only tracks a per-recipient
delivered-count for the sidebar's unread badge and has no sender, text, or
per-event timestamp:

```sql
CREATE TABLE message_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id TEXT,              -- NULL: delivery with no resolvable sender
    to_id TEXT NOT NULL,
    text TEXT NOT NULL,
    delivered_at INTEGER NOT NULL  -- unix millis, matching inbox.rs's sent_at
);
```

`Store::record_message_event(from, to, text)` is called from
`messaging::deliver_message` alongside the existing
`record_message_delivered` counter bump — both run regardless of which
`SendMechanism` actually delivered the message. This table exists purely to
drive live visualization, not as an audit log, so each insert prunes down
to the most recent 200 rows (`DELETE ... WHERE id <= (SELECT MAX(id) ...) -
200`) rather than growing unbounded.

`Store::recent_message_events(since_id)` returns rows with `id > since_id`,
ordered by `id` — the same "poll what's new since last time" shape as
`list_session_deps`/`deps_for_session`.

## TUI: polling and flash lifecycle

`GraphState` (`tui/graph.rs`) gains:

- `last_seen_event_id: i64` — the polling cursor.
- `events_initialized: bool` — see "startup catch-up" below.
- `active_flashes: Vec<ActiveFlash>` — in-flight flashes, each with a
  `ticks_left: u8` counter (`FLASH_TICKS = 3`).
- `recent_events: VecDeque<MessageEvent>` — a capped (20) buffer feeding the
  activity panel, independent of the flash lifecycle.

`GraphState::ingest_message_events(&mut self, events: &[MessageEvent])` is
called from `refresh()` in `tui/mod.rs`'s existing 1s tick, right before
`sync_fleet` (no new tick added):

1. Age existing flashes by one tick, dropping any that hit zero.
2. For each new event: advance `last_seen_event_id`, append to
   `recent_events` (trimming to the cap), and — unless this is the very
   first call this TUI process has made — push a new `ActiveFlash`.

**Startup catch-up:** `recent_message_events(0)` on the first tick after
launch returns whatever's currently in the (already-pruned-to-200) table,
which could be minutes or hours old. Treating all of that as "just
delivered" would flash the whole fleet the moment the TUI opens. The first
call to `ingest_message_events` only catches the cursor up and populates
`recent_events` for the activity panel; it does not start flashes. Every
call after that reflects genuinely new deliveries, because
`recent_message_events` only ever returns rows newer than the cursor.

`sync_fleet` resolves each `ActiveFlash`'s `from`/`to` against the fleet
nodes it's about to render (`node_ids`, already computed for the
dependency-edge-skipping logic from part one) and degrades per-endpoint:

- Neither endpoint present → nothing to show (the flash still ages out on
  schedule).
- Only one endpoint present → that node alone flashes.
- Both present → both nodes flash, and if an edge already connects them
  (hierarchy or dependency, in either stored direction — `Edge::connects`
  already checks both), that edge is marked `animated = true` with a
  brightened stroke and its arrow marker pointed the way the message
  actually traveled (see below), not necessarily the way the edge happens
  to be stored.

## Rendering

**Nodes:** `SessionNodeContent` gains `flashing: Option<FlashDirection>`
(`Sender` / `Receiver`), rebuilt fresh every tick in `sync_fleet` exactly
like every other field on this type (never mutated after the fact). When
set, the border gains `Modifier::RAPID_BLINK` (on top of `BOLD`, which a
permanently-stuck node already has) — blink is the one modifier `stuck`
never sets, so it's the unambiguous "this is transient" signal — and a
small arrow span (`↗` sender / `↙` receiver) is added after the existing
status glyph, leaving that glyph's own color/meaning untouched.

**Edges:** `rataflow::Edge::animated` drives the library's own marching-ants
clock; nothing else about edge routing changes. Direction is shown by
building the flash's `EdgeStyle` with only one of `marker_start`/
`marker_end` set to `ArrowClosed` (whichever end the message actually
arrived at), rather than reusing whatever the edge's stored source/target
already default to — a reply along a hierarchy edge (child → orchestrator)
needs its arrow pointing the opposite way from that edge's usual
orchestrator → child direction.

## Recent activity panel

`layout::graph()` adds a third strip (`GraphLayout::activity`) below the
existing canvas + detail strip, sized up to 6 rows from whatever height
remains. `draw_graph` renders `GraphState::recent_events` there, most
recent first: relative time (reusing `view::ago`), `from → to` (session
names when the row is still in the fleet, else the raw id), and the
message text (`view::truncate`’d to fit) — the log the user can check for
a message they might have missed the flash for.

## Testing

Matches the existing per-module `#[cfg(test)] mod tests` style:
`store.rs` covers round-trip/ordering/pruning for the new table;
`messaging.rs` covers `deliver_message` recording an event (not just the
counter) for the inbox mechanism; `graph.rs` covers flash creation,
expiry, graceful degradation for a missing sender or a not-currently-
rendered endpoint, and that a flash along an existing edge sets
`animated`.
