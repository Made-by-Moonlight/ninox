# Per-location concurrent session limit with queueing

Brief: `.claude/design-ref/fleet-overview-v1/BRIEF-session-limits.md`.

## Storage (compatible with older binaries)

An older binary reads an unknown `sessions.status` as `Working`
(`unwrap_or(Working)` in `Store::list_sessions`), so a `queued` status value
would put pid-less ghosts in front of its poller. Queued sessions therefore
never touch `sessions.status`:

- `session_queue(seq AUTOINCREMENT, id UNIQUE, location, state, kind,
  enqueued_at, claimed_at, claimed_by_pid, session JSON, payload JSON)`.
  - `state` is `queued` (waiting) or `launching` (holds a slot while a spawn
    runs).
  - `kind` is `spawn` (no sessions row yet) or `relaunch` (a restore of an
    existing `Interrupted` row).
  - `location` is `''` for local, otherwise the machine profile id.
- `held_messages(seq, to_id, tagged, attributed_from, defanged, kind,
  created_at)` holds `ninox send` messages to queued sessions.
- `SessionStatus::Queued` exists only in memory, for display. `upsert_session`
  refuses to write it.

Older binaries never read either table, so the worst case is that they don't
drain the queue.

## Admission (atomic)

`Store::admit(location, limit, no_queue, entry)` runs in one `BEGIN
IMMEDIATE` transaction:

1. Count the running sessions for the location: sessions rows that are
   non-terminal, on that location and not the prompt agent, plus `launching`
   reservations (not counted twice).
2. Admit when the limit is unlimited, or when there is a free slot and
   nobody is queued ahead. Admitting inserts a `launching` reservation.
3. Otherwise refuse when `no_queue` is set, else queue.

`Store::claim_queued(location, limit)` uses the same transaction shape to
move the oldest `queued` entries to `launching`. Reservations whose owner
pid is dead are released inside that transaction.

## Spawn paths

Each path is split into validate/admit and launch. When queued, the launch
inputs are serialised as a `QueuedLaunch` payload in ninox-ui-shared:

- `ninox spawn` (`run_spawn`) → `CliWorker`.
- `spawn_orchestrator_common` (CLI peer/sub-orchestrator, `orchestrate`, TUI
  dialog) → `Orchestrator`.
- `spawn_standalone_worker` (TUI dialog) → `Standalone`.
- GUI Spawn dialog: admitted spawns run the existing inline path unchanged;
  queued ones are stored as `Standalone`/`Orchestrator` payloads.
- Fleet restore: each step is admitted in restore order (workers first). The
  remainder is queued as `Relaunch`, and the launcher runs
  `restore(only=[id])`.
- The prompt agent bypasses all of this.

`--no-queue` turns "queue" into an error.

## Draining

`Poller::with_queue_launcher` follows the same injection pattern as
`with_fleet_restorer`. On every pid tick the poller re-reads `[limits]`,
claims the free slots for each location and spawns a launcher task for each
claimed entry. The task:

1. Launches the session.
2. Flushes held messages in order.
3. Releases the reservation.

Raising the limit takes effect on the next tick. Lowering it only gates
future starts.

`poll_pids` only notices a dead session through its pid, and CLI-spawned
workers never record one. So while a location is full and has sessions
waiting, the drain first runs the existing dead-session reconciliation on
that location's pid-less local sessions whose pane is gone. ACP sessions
and remote sessions are skipped.

`ninox spawn` does not drain the queue itself. The poller runs in the GUI
and the headless engine; a store served only by an older engine keeps its
queue until a current GUI or engine runs.

## Config / UI

- `[limits] max_concurrent_local = 6` (0 = unlimited).
- `[limits.remote] <machine-id> = n` (absent = unlimited). This lives in
  `[limits]` rather than on `MachineProfile`, whose serialised shape is
  pinned to pure connection metadata by a test.
- `ninox limits show|set`, saved through `AppConfig::update`; `set` is
  refused for worker sessions.
- `ninox config set` keeps its raw-document writer, which preserves keys
  that `AppConfig` doesn't know. It now also refuses to rewrite a config
  that no longer parses, matching `AppConfig::save`.
- GUI and TUI Settings get a "Session limits" section.
- "queued · #N" badges on islands and the Board.
- A "N/M running locally · K queued" counter in the GUI and TUI headers.
- `ninox list` shows queue positions. In `ninox list --json`,
  `sessions`/`orchestrators` stay raw store rows and waiting sessions
  appear only under `queue`.

## Hardening

Retention, the orphan sweep, merge detection, pid liveness and the remote
poll all iterate `sessions` rows. Fresh queued spawns have no row, and
queued relaunches stay `Interrupted`, which those passes already skip or
treat as terminal. A `relaunch` claim re-checks that the row is still
restorable, because an older binary may have restored it meanwhile.
