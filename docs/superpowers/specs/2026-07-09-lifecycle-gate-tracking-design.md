# Structured Lifecycle Gates, Race-Safe State Updates, and Retention Visibility

## Context

Ninox tracks each worker as a `Session` row (`crates/ninox-core/src/types.rs:22-95`)
whose `status: SessionStatus` (`types.rs:9-20`) is the single field the UI
renders everywhere — sidebar (`components/sidebar.rs`), fleet board
(`components/fleet_board.rs`), and PR list (`components/pr_list.rs`) all read
it via one shared `App.sessions: HashMap<SessionId, Session>`
(`crates/ninox-app/src/app.rs:129-205`), populated only by
`Event::SessionUpdated`/`SessionSpawned` from a single broadcast subscription
(`App::subscription`, `app.rs:2468-2499`). There is no independent per-view
cache — architecturally, one source of truth.

That architecture is undermined by how it's written to. A single
`Poller` (`crates/ninox-core/src/lifecycle/poller.rs`) runs four independent
timer arms (5s/10s/30s/6h, `poller.rs:121-156`), plus the app's own startup
reconciliation (`app.rs`) and direct user actions (`Engine::terminate_session`,
`remove_session`, `cleanup_session`, `events.rs`) — each of these is an
independent actor that does its own `list_sessions()` (fresh-but-momentary
read) → mutates the field(s) it owns → `Store::upsert_session` (a full-column
`INSERT...ON CONFLICT DO UPDATE SET` — `store.rs:74-104`, every column,
always) → `emit(Event::SessionUpdated(session))` → the frontend's handler
does `state.sessions.insert(id, session)` (`app.rs:2276-2279`), a **wholesale
replace** of the entry, not a merge.

This combination — full-row DB writes plus full-struct event replace, driven
by multiple concurrently-ticking actors reading from possibly-stale
snapshots — means any field set via an event/in-memory path without a
durable write in the *same* read-modify-write cycle can be silently
reverted the next time *any other* actor's cycle fires, even though every
actor is technically writing through "the" store. This is exactly what
happened in PR #57 (`ee0267c`, preceded by `6654ae5`): `poll_github`
discovered a PR and emitted `Event::PrOpened`, but didn't persist
`session.pr_id` to SQLite in that tick. The very next tick of any other
poller arm (e.g. `poll_usage`) re-read the session from the DB — still
`pr_id: NULL` — and re-emitted `Event::SessionUpdated`, stomping the
frontend's correct in-memory `pr_id` back to `None` and flickering the PR
card between "No PR yet" and the real card. The fix folded `pr_id` into the
same conditional `upsert_session` write that already self-heals
`repo`/`pr_number` (`poller.rs:560-585`), and a same-PR follow-up commit
moved that write to run *before* merge detection's early `continue`
(`poller.rs:560-570`'s comment documents why). Both fixes closed the
specific field; neither closed the general class — any new field is exposed
to the identical hazard unless every write site remembers the
persist-before-broadcast discipline.

Separately, the raw signals that decide status are already fetched every
30s tick but thrown away once collapsed into one `SessionStatus`:
`derive_session_status` (`poller.rs:949-972`) consumes `CIStatus`
(`types.rs:114-121`), `has_changes_requested` (computed inline,
`poller.rs:664`, never persisted), and `pr_status.mergeable` (from the
GitHub API response, never persisted) to produce a single enum value with
no record of *which* check is blocking or *since when*. `derive_session_status`
guards terminal states from being overwritten (`poller.rs:955-958`, tested
by `derive_status_preserves_done`/`derive_status_preserves_terminated`,
`poller.rs:1026-1046`).

"Done" is decided in exactly one place — `handle_merge_detection`
(`poller.rs:827-861`), called only from `poll_github` when
`pr_status.merged` is true — which calls `Engine::cleanup_session`
(`events.rs:195-218`): kills the tmux pane, removes the worktree/hook
artifacts (`remove_worktree_and_artifacts`, `events.rs:225-234`), sets
`status = Done`, and stamps `terminal_at = now_millis()`. `terminal_at`
(`types.rs:83-94`) gates a separate retention sweep,
`Poller::sweep_retired_sessions` (`poller.rs:875-931`, runs every 5s tick
alongside `poll_pids`), which purges `Done`/`Terminated` rows once
`SessionRetentionConfig::done_retention_days` (default 2,
`crates/ninox-core/src/config.rs:116-138`) has elapsed since `terminal_at` —
or immediately if `terminal_at` is `None` (i.e. the session reached a
terminal state via a direct user action — `terminate_session`,
`events.rs:164-188`, explicitly never sets `terminal_at`).

None of this — which gate is blocking, or that a `Done`/`Terminated` session
is sitting in its retention window and will be auto-removed — is visible
anywhere in the UI today. In practice, this means a user manually deletes
sessions the retention sweep was already about to remove on its own, because
nothing in the sidebar row indicates that's pending.

## Goal

1. Close the write-race class of bug generically, so no future field
   (including the ones this spec adds) can be silently stomped by an
   unrelated actor's stale read.
2. Turn the CI/review/mergeable signals `derive_session_status` already
   computes into a persisted, structured `GateStatus` that the UI can render
   plainly — "CI: passing", "Review: changes requested" — instead of only
   ever seeing the single collapsed `SessionStatus`.
3. Make the pending-deletion window (`terminal_at` + retention days) visible
   in the sidebar and fleet board, so a session already scheduled for
   automatic cleanup doesn't get manually deleted out of not knowing that.

## Non-goals

- **Not broadening "Done."** PR-merged-per-GitHub-API remains the sole
  terminal-success signal. GitHub, not the agent or CI, is authoritative on
  merge — that's correct as-is. This spec only makes the existing signal
  legible, not different.
- **Not changing retention/cleanup logic.** The automatic-merge / manual-
  remove / manual-terminate / GC-sweep matrix (`events.rs`, `poller.rs:875-931`)
  stays exactly as it behaves today. The only change is surfacing
  `terminal_at` in the UI.
- **Not a full transition-history/audit log.** No new `session_transitions`
  table, no timeline view. `GateStatus` reflects current state only, with a
  single `since` timestamp per gate — enough to answer "why is this stuck
  and for how long," not "show me every change that ever happened."
- **Not a general optimistic-concurrency/versioning mechanism.** The race
  fix is a merge-on-apply change to the event-handling path, not a
  row-version/CAS scheme on the store.

## Approach

Three independent, sequentially-buildable pieces:

1. Change `Event::SessionUpdated`'s apply path from a wholesale
   `HashMap::insert` to a field-level merge, and make each producer of that
   event carry only the fields its tick is authoritative for. A stale
   rebroadcast from an unrelated actor can then never blow away a fresher
   field, regardless of tick ordering — this is the generic version of what
   PR #57 patched one field at a time.
2. Add `GateStatus` (computed inside `derive_session_status`'s call site
   from data already fetched, no new API calls) as a field on `Session`,
   persisted the same way `status` is, and render it in a hover tooltip
   (following the existing `hover_preview_slip` pattern in
   `brain_panel.rs`) wherever a status dot appears.
3. Add an always-visible "removing in _N_" badge next to the status dot in
   the sidebar and fleet board, computed client-side from
   `terminal_at + retention_days - now`.

## Architecture

### 1. Race-safe session updates

**Problem restated precisely:** `Event::SessionUpdated(Session)` carries a
full snapshot built from whatever `list_sessions()`/`get_session()` returned
at the start of that actor's tick. If actor A's tick reads the row, sets
field X, and (correctly, per the PR #57 fix pattern) persists before
emitting, actor B's tick — already in flight, having read the row *before*
A's write landed — will still emit its own full snapshot with the old value
of X. `state.sessions.insert(id, session)` at the receiving end has no way
to know B's snapshot is stale for field X; it just replaces the whole entry.

**Fix:** change the frontend handler
(`app.rs:2276-2279`) from replace to merge:

```rust
Event::SessionUpdated(incoming) => {
    match state.sessions.get_mut(&incoming.id) {
        Some(existing) => existing.merge_from(&incoming),
        None           => { state.sessions.insert(incoming.id.clone(), incoming); }
    }
    Task::none()
}
```

`Session::merge_from` (new method, `types.rs`) is not a generic "copy every
field" — that would just reintroduce the same bug at the merge layer. It
copies only fields that the *emitting call site* is authoritative for,
signaled by making each poller/engine call site construct its
`Event::SessionUpdated` from an explicit "what changed" set rather than a
raw `session.clone()`. Concretely:

- `poll_github`'s status/gate write (`poller.rs:704-711`) is authoritative
  for `status`, `gate_status`, `pr_number`, `pr_id`, `repo` — the fields it
  just persisted in the same tick.
- `poll_usage` (cost) is authoritative for `cost_usd` only.
- `poll_context_updates`/the statusline hook path is authoritative for
  `context_used_pct`/`context_total_tokens`/`context_window_size`/
  `context_tokens` only.
- `sync_sessions_metadata` is authoritative for `pr_number`/`status` (the
  self-report adoption path, `poller.rs:224-316`) only.
- `Engine::cleanup_session_in`/`terminate_session`/startup reconciliation are
  authoritative for `status`/`terminal_at` only.

Rather than adding a parallel "diff" type, the simplest implementation is:
each call site keeps doing exactly what it does today — read a fresh
`Session`, mutate the specific field(s) it owns, `upsert_session`, emit — but
`merge_from` is written defensively, keyed on **which fields that call site
is known to touch**, via a small `SessionFields` bitflag-style marker
included on the event:

```rust
pub enum Event {
    ...
    SessionUpdated(Session, SessionFields), // fields actually written this tick
    ...
}
```

`SessionFields` is a `bitflags`-style set (`STATUS`, `GATE`, `PR_LINK`, `COST`,
`CONTEXT`, `TERMINAL_AT`, ...). `merge_from(&incoming, fields)` copies exactly
the flagged fields from `incoming` onto `existing`, leaving every other field
on `existing` untouched no matter what `incoming` happens to hold for them
(since `incoming` was read from a DB snapshot that may already be stale for
those other fields). This is a mechanical, low-risk change at each of the
~6 existing `Event::SessionUpdated` call sites (adding the relevant flags),
and it makes the hazard structurally impossible to reintroduce by accident
for a *new* field too, since a new call site must declare which fields it
owns to compile against the new `Event::SessionUpdated` signature.

`Event::SessionSpawned` is unaffected — a brand-new session has no existing
entry to stomp, so it stays a full insert.

The DB write path (`Store::upsert_session`) is unchanged — it stays a
full-row upsert. The race is about the *event/apply* path, not the DB
schema; SQLite's full-row upsert is not itself unsafe (each write is
internally consistent), the hazard is purely in how the frontend applied
possibly-stale full snapshots.

### 2. `GateStatus`

New type in `crates/ninox-core/src/types.rs`, alongside `SessionStatus`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GateCheck {
    Passing,
    Failing,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateStatus {
    pub ci:        GateCheck, // from CIStatus: failing>0 -> Failing, pending>0 -> Pending, else Passing
    pub review:    GateCheck, // has_changes_requested -> Failing, else Passing
    pub mergeable: GateCheck, // pr_status.mergeable: Some(true) -> Passing, Some(false) -> Failing, None -> Unknown
    /// Epoch ms this exact (ci, review, mergeable) combination was first
    /// observed — reset whenever any of the three values changes.
    pub since: i64,
}
```

New nullable column on `sessions`:

```sql
ALTER TABLE sessions ADD COLUMN gate_status TEXT; -- JSON-encoded GateStatus, NULL until first computed
```

`Session` gains `#[serde(default)] pub gate_status: Option<GateStatus>`
(`None` for legacy rows and for sessions with no open PR yet, i.e.
`Spawning`/`Working`).

**Computation:** `derive_session_status`'s call site in `poll_github`
(`poller.rs:704-711`) is the only place with all three raw inputs (`ci`,
`has_changes_requested`, `pr_status.mergeable`) already in hand — no new
GitHub API calls. It gains a sibling `derive_gate_status(&ci, has_changes_requested,
pr_status.mergeable, previous_gate)` that produces the new `GateStatus`,
carrying `since` forward from `previous_gate` when the three values are
unchanged, and resetting it to `now_millis()` when they differ (including
the first time a `GateStatus` is computed for a session). This is persisted
in the same conditional write that already persists `status`
(`poller.rs:708-709`), and included via `SessionFields::GATE` on the same
`Event::SessionUpdated` (see §1) — so it's subject to the exact same
race-safety as `status`.

Terminal sessions keep their last-computed `GateStatus` (not reset to
`Unknown`) — it remains meaningful context ("this merged while CI was still
finishing" is a legitimate thing to show), and nothing recomputes it once
`derive_session_status`'s terminal guard (`poller.rs:955-958`) takes effect.

### 3. UI

**Tooltip (all three gate-bearing views: sidebar row, fleet card, PR list
row).** New `gate_tooltip(s: &ColorScheme, session: &Session) -> Element`
in a shared location (e.g. `components/status_tooltip.rs`), styled after
`hover_preview_slip` (`brain_panel.rs:414-443` — `paper_2` background,
1.5px ink border, hard drop shadow, micro-label + serif title + body text,
positioned via `iced::widget::stack!` rather than following the cursor).
Content:

- One line per gate, plain English: `"CI — passing"`, `"Review — changes
  requested"`, `"Mergeable — blocked on review"` (derived by combining
  `mergeable: Failing` with which of `ci`/`review` is also failing, since
  "not mergeable" alone isn't informative about *why*).
- For `Spawning`/`Working` (no `gate_status` yet): a single explanatory
  line, e.g. `"No PR opened yet"`.
- For `Done`/`Terminated`/`Interrupted`: the last-known gate line(s) plus
  the retention line from the badge below (so the full explanation is
  available on hover even where the badge is also always-visible).

Wiring: each of `sidebar.rs`'s `tree_row`, `fleet_board.rs`'s
`session_card`, and `pr_list.rs`'s `pr_row` wraps its existing status-dot
element in a `mouse_area(...).on_enter/on_exit` pair (the same hover-tracking
shape `brain_pinboard.rs` already uses for `Message::BrainHoverEntry`) firing
a new `Message::GateHover(Option<SessionId>)`, stored on `App` as
`hovered_gate: Option<SessionId>`, read by each view's render function to
decide whether to stack the tooltip.

**Pending-deletion badge (sidebar row + fleet card only — not the PR list,
which has no natural "row" concept for a terminated session once its PR is
merged/closed).** A small text label rendered next to the status dot,
visible only when `session.terminal_at.is_some()`:

```rust
fn retention_label(terminal_at: i64, retention: &SessionRetentionConfig, now: i64) -> String {
    let remaining_ms = terminal_at + retention.retention_millis() - now;
    if remaining_ms <= 0 { "Removing shortly".into() }
    else { format!("Removing in {}", humanize_duration(remaining_ms)) }
}
```

`humanize_duration` renders exactly one unit, the coarsest that's still
≥1: days if `remaining_ms >= 86_400_000`, else hours if
`>= 3_600_000`, else minutes (`"in 2d"`, `"in 18h"`, `"in 5m"`) — a small
new helper, not a dependency, since existing duration formatting in the
codebase (cost/uptime displays) is bespoke per-call-site already. `now` is supplied via `App`'s existing tick-driven redraw (Iced
re-evaluates render functions continuously enough for a coarse countdown;
no new timer needed). `retention.done_retention_days` is already loaded
into `AppConfig` (`config.rs:116-138`) and accessible from `App`.

Sessions terminated by direct user action (`terminate_session`) have
`terminal_at: None` (`events.rs:169-176`) and are purged on the very next
5s GC tick (`poller.rs:891-897`) — for these, no badge is shown (there's no
meaningful countdown; they're gone within moments), which also means the
badge's mere presence is itself informative: seeing it means "this was an
automatic merge-done/natural-process-exit, sitting in its grace period,"
not "this was terminated" — no separate visual distinction needed beyond
the existing status-dot color.

## Error handling

- `GateStatus` is only ever computed from data already successfully fetched
  in `poll_github`'s current tick; if a GitHub call fails partway through
  (`poller.rs:608-611`, `658-661` already tolerate individual failures by
  falling back to empty/default values), the gate computation uses whatever
  partial data is available the same way `derive_session_status` already
  does — no new failure mode introduced.
- A session with `gate_status: None` (never enriched, or legacy row) renders
  the tooltip's `Spawning`/`Working` fallback line regardless of its actual
  status, rather than panicking or showing a blank tooltip.
- If `SessionFields` on an incoming `Event::SessionUpdated` is empty (should
  not happen, but defensively), `merge_from` is a no-op — never worse than
  today's behavior of doing nothing.
- Retention label math (`terminal_at + retention_millis() - now`) uses `i64`
  and saturates at "Removing shortly" for any zero/negative remainder rather
  than displaying a negative duration.

## Testing

- `types.rs`: `Session::merge_from` unit tests — given two sessions with
  disjoint changed fields and a `SessionFields` mask covering only one of
  them, confirm the untouched field survives the merge (this is the
  regression test PR #57's fix lacked at the general level: two
  out-of-order `Event::SessionUpdated`s touching disjoint fields must not
  stomp each other regardless of arrival order).
- `poller.rs`: `derive_gate_status` unit tests mirroring the existing
  `derive_session_status` table — each `(ci, has_changes_requested,
  mergeable)` combination maps to the expected `GateStatus`; `since` carries
  forward when the combination is unchanged and resets when it changes;
  terminal sessions' gate is not recomputed (mirrors
  `derive_status_preserves_done`/`derive_status_preserves_terminated`,
  `poller.rs:1026-1046`).
- `store.rs`: round-trip a `Session` with `gate_status: Some(...)` through
  `upsert_session`/`get_session`, and confirm a legacy-shaped row (column
  absent) deserializes to `gate_status: None`.
- UI: `retention_label` unit tests for boundary values (exactly at
  retention limit, just past it, days vs. hours vs. minutes formatting).
- Manual verification in the running app: hover a status dot in the
  sidebar, fleet board, and PR list and confirm the tooltip renders the
  correct gate breakdown for a live `PrOpen`/`CiFailed`/`ReviewPending`/
  `Mergeable` session; confirm a `Done` session shows a "Removing in _N_"
  badge in both sidebar and fleet board that counts down correctly and
  disappears once the retention sweep purges the row.
