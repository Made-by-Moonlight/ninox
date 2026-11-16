# MLOPS-5321: persist machine connection health

Spec: `docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md`
§"Node content and appearance" — remote nodes need a connection-status
badge (green=connected, amber=reconnecting, red=unreachable).

## Current state

`Poller::poll_remote_machine_health` (crates/ninox-core/src/lifecycle/poller.rs)
already probes each enabled `MachineProfile` and tracks reachability in an
in-memory `remote_machine_health: HashMap<String, MachineHealthState>`
(`backoff_secs`, `next_attempt_at`, `unreachable_notified`), surfaced only as
one-shot `MachineUnreachable`/`MachineReachable` notifications. The
function's own doc comment flags the store-persistence half as explicit
follow-on work — this is that follow-on.

A parallel PR (#241, MLOPS-5313) adds node polish to the TUI fleet graph.
It started out only wiring a `machine_id` badge, but by the time it merged
into `v1` it had grown its own placeholder `MachineHealth` enum and full
green/amber/red badge rendering in `graph.rs`, explicitly scaffolded for
this ticket to fill in (`SessionNodeContent::machine_health`'s own doc
comment named MLOPS-5321). #241 merged into `v1` mid-way through this
branch's review; once that happened the "if #241 has merged when you get
there, populate that field" branch applied, so this PR was rebased onto it
and finishes the wiring: `ninox-core::types::MachineHealth` replaces
`graph.rs`'s local placeholder enum, and `GraphState::sync_fleet` now
passes `Row::machine_health` through to `SessionNodeContent::machine_health`
instead of a hardcoded `None`.

`ninox-server`'s `/sessions` route serves a machine's own view of its own
sessions to the *orchestrating* side's `poll_remote_sessions` (pulled over
the SSH tunnel) — the TUI never calls it directly; it always reads
`store.list_sessions()`/`store.list_orchestrators()` locally and builds
`Row` itself. So machine health, which only the probing side ever knows,
belongs in that side's local store, not behind a new `ninox-server` route.

## Three-state mapping

No existing signal distinguishes a brief blip from a sustained outage other
than the backoff itself (doubles 5s→120s, capped — `remote_health::next_backoff_secs`).
Reusing that existing, already-meaningful threshold rather than inventing a
new one:

- `Connected` — no recorded failure, or the probe just succeeded.
- `Reconnecting` — probe failing, backoff still below the 120s cap (still
  plausibly transient).
- `Unreachable` — backoff has hit the 120s cap (sustained outage).

## Changes

1. **`ninox-core/src/types.rs`** — new `MachineHealth` enum: `Connected` /
   `Reconnecting` / `Unreachable`, `Serialize`/`Deserialize` snake_case,
   mirroring `NotificationKind`'s style. Exported via the crate's existing
   `pub use types::*`.
2. **`ninox-core/src/store.rs`** — new `machine_health` table
   (`machine_id TEXT PRIMARY KEY, status TEXT NOT NULL`), created via
   `CREATE TABLE IF NOT EXISTS` (idempotent against existing DBs, no `ALTER
   TABLE` migration needed since it's a new table, not a new column).
   `Store::set_machine_health`, `Store::machine_health` (returns
   `HashMap<String, MachineHealth>`), `Store::remove_machine_health` —
   same enum-as-string round-trip as `upsert_session`/`list_sessions`.
3. **`ninox-core/src/lifecycle/poller.rs`** — `note_machine_reachable` and
   `note_machine_unreachable` persist the mapped `MachineHealth` via the
   new store methods; pruning a removed profile's in-memory state
   (`poll_remote_machine_health`'s `retain`) also calls
   `remove_machine_health` for each pruned id. Update the stale "never
   touches the store" doc comments/invariant test that predate this.
   `Session`/`sessions` table untouched — `poll_remote_machine_health_never_touches_session_rows`
   still holds (it only asserted no *session* row changes).
4. **`ninox-tui/src/tui/state.rs`** — `Row` gets `pub machine_health:
   Option<MachineHealth>`.
5. **`ninox-tui/src/tui/mod.rs`** — `build_rows` takes the store's
   `machine_health` map and looks each row's `session.machine_id` up in it;
   `refresh` fetches the map once per tick and passes it through.
6. **`ninox-tui/src/tui/graph.rs`** — `ninox_core::types::MachineHealth`
   replaces the local placeholder enum #241 shipped for this; `sync_fleet`
   passes `row.machine_health` into `SessionNodeContent` instead of a
   hardcoded `None`. (The earlier `ninox request-work` follow-on filed for
   this hookup is now superseded — it's done here instead.)

## Test plan

- Store: round-trip `set_machine_health`/`machine_health`/`remove_machine_health`.
- Poller: extend the existing backoff/recovery test to assert the stored
  `MachineHealth` at each step (first failure → `Reconnecting`, backoff
  pinned at the 120s cap → `Unreachable`, recovery → `Connected`); extend
  the pruning test to assert the store row is also removed.
- `build_rows`: a session whose `machine_id` matches an entry in the health
  map gets that `machine_health`; no match (including `machine_id: None`)
  gets `None`.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` all green before opening the PR.
