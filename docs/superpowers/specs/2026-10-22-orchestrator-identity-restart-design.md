# Orchestrator Identity Restart & Re-registration Design (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-core` (`orchestrator_auth.rs`,
`store.rs`, `lifecycle/probe.rs`), `ninox-ui-shared`
(`spawn_util.rs`'s `relaunch_in_place`, shared by `ninox fleet restore`
and `ninox restart`). File and line references below reflect the code
as of this writing; treat them as orientation, not guaranteed
locations.

## Context

`crates/ninox-core/src/orchestrator_auth.rs`'s `authorize_orchestrator`
cross-checks a caller's claimed `NINOX_ORCHESTRATOR_ID` against a
persisted `OrchestratorRuntimeIdentity` row (`types.rs:326-335`:
`orchestrator_id`, `runtime_id`, `server_epoch`, `physical_tmux_name`,
`pane_id`, `root_pid`, `root_created_at`, `registered_at`) —
`server_epoch`/`physical_tmux_name`/`pane_id`/`root_pid`/
`root_created_at` must all match the caller's live
`runtime::PaneIdentity`, plus a live process-ancestry check via
`runtime::caller_descends_from(persisted.root_pid)`. That row
self-registers exactly once, the first time an orchestrator authorizes
from a pane whose physical tmux name matches its own id
(`store::register_migrated_orchestrator_runtime`, an
`ON CONFLICT(orchestrator_id) DO NOTHING` insert gated on the
session's recorded `pid` matching the claimed root pid and no row
existing yet). `store::register_orchestrator_runtime` also exists
(`store.rs:524-559`) but has no production caller — it unconditionally
upserts (`ON CONFLICT(orchestrator_id) DO UPDATE SET ...`, overwriting
every field with no check at all) and today only test fixtures call it
directly.

This write-once design is deliberate, not an oversight: an
`OrchestratorRuntimeIdentity` row exists specifically so a hijacked or
maliciously-respawned pane — one an attacker controls, claiming the
same orchestrator id — cannot silently take over an existing
orchestrator's trusted identity. Once registered, the row is
permanently pinned to one pane/process/server-epoch triple.

The consequence: any event that changes those — a tmux pane recreated
after a crash or terminal restart, a server-epoch rollover, or
anything the restart/fleet-restore flow does (see "Restart mechanics"
below) — makes every subsequent `authorize_orchestrator` (and, per
below, `attest_sender`) call for that orchestrator fail the identity
cross-check *forever*. No code path today ever clears or re-registers
a row once one exists.

Before the daemon-attested sender-identity-attestation feature
(`docs/superpowers/specs/2026-10-21-sender-identity-attestation-design.md`,
implemented on the not-yet-merged `v1-sender-identity-impl` branch),
this gap only broke `ninox plan register` and `ninox reap` — a narrow,
occasional annoyance. On `v1-sender-identity-impl`,
`authorize_orchestrator`'s self-registration/matching logic was
factored out into `persisted_or_migrated_identity` and
`runtime_identity_matches`, and `messaging::deliver_message`'s
`attest_sender` now reuses `persisted_or_migrated_identity` to verify
*every* orchestrator-sent inter-session message
(`orchestrator_auth.rs`'s `attest_sender`, `SenderAttestation::render`).
That worker's own final-review pass flagged this as a **pre-existing
gap newly made visible, not newly introduced, by that feature**: after
a pane/process restart, every single message that orchestrator sends
now renders `[ninox: sender identity could not be verified]`
(`SenderAttestation::render`'s fallback line) instead of the occasional
command failure this used to be. This spec addresses the gap itself,
independent of exactly when `v1-sender-identity-impl` lands.

### Restart mechanics

Four call sites actually tear down and relaunch a live orchestrator's
pane under its existing session id, all funneling through the same
shared primitive, `spawn_util::relaunch_in_place`: `ninox fleet
restore` (`ninox-ui-shared/src/fleet.rs:511`), the on-demand `ninox
restart` (`ninox-ui-shared/src/restart.rs:255`, inside
`restart_one_inner`), the desktop app's Resume action
(`ninox-gui/src/app.rs:2063`), and the TUI's resume action
(`ninox-tui/src/tui/mod.rs:1065`). `relaunch_in_place` calls
`ninox_core::runtime::kill_session(&id)` to kill whatever stale pane
exists, recreates the workspace if needed, then calls
`launch_interactive_session` to spawn a fresh pane under the same
session id (resuming the harness conversation when possible). All four
callers already know `is_orchestrator: bool` and the session's full
pre-relaunch `Session` row (including its previously-recorded `pid`) —
since the clearing logic this spec adds lives inside
`relaunch_in_place` itself rather than in any one caller, all four
paths are transparently covered by the same fix, with no per-caller
duplication. `ninox-core/src/fleet/startup.rs` itself is policy-only —
it computes whether anything is awaiting restore and flags/
auto-triggers a restore (`on_engine_startup`, `restore_summary`) but
does not relaunch any process; the actual relaunch sits in
`fleet.rs`/`restart.rs`/`ninox-gui`/`ninox-tui`/`spawn_util.rs` as
described above.

**Note on `ninox restart`'s own access control:** tracing
`restart_one_inner` end to end (`cli.rs:877` → `restart::run_cli` →
`restart_one_inner` at `restart.rs:217`) shows it has no check that
the calling process owns or is the target orchestrator — it only
requires the target session to exist and be live
(`runtime::has_session`) and that no fleet restore currently holds the
restore lock. Any local process can already run `ninox restart
<any-live-session-id>` today, including another orchestrator's. That
is real and pre-existing, but it is a *restart-authorization* gap, not
an *identity* gap: it lets a caller force a pane to be killed and
relaunched, not control what gets registered once it comes back up
(see Goals and the core design question below). Fixing who may
trigger a restart is out of scope here — see Non-goals.

## Goals

- Let a legitimate orchestrator whose pane/process was intentionally
  restarted by ninox itself (`ninox fleet restore`, `ninox restart`,
  the GUI/TUI resume actions) regain a working, verifiable runtime
  identity automatically, without manual intervention.
- Preserve the existing anti-spoofing guarantee precisely as it
  actually holds: no caller can ever control *what* a persisted
  `OrchestratorRuntimeIdentity` row is replaced with, or get its own
  code to run inside the respawned pane. A row is only ever cleared
  after ninox's own `relaunch_in_place` confirms, via a real liveness
  probe, that the specific process root it already names is dead —
  and the replacement identity is never asserted by whatever caller
  triggered the relaunch; it is captured later, lazily, from whichever
  pane `launch_interactive_session` (ninox's own spawn code, never
  caller input) actually started, the same way first-use migration
  already works today. This is deliberately *not* a claim that
  triggering a relaunch itself requires authorization — see the note
  in "Restart mechanics" above and the Non-goals below.
- One shared mechanism, living inside `relaunch_in_place` itself, so
  all four callers that already share it (`ninox fleet restore`,
  `ninox restart`, and the GUI/TUI resume actions) are covered
  transparently rather than needing duplicated logic per caller.

## Non-goals

- Cross-machine identity — out of scope here, same as the
  sender-identity-attestation design's non-goal; this spec is
  same-machine only, consistent with `orchestrator_auth`'s existing
  scope.
- Worker identity. Workers have no equivalent persistence problem:
  `attest_sender` verifies a worker sender by comparing its live
  pane's `physical_tmux_name` against the claim
  (`orchestrator_auth.rs`'s `attest_sender` doc comment), with no
  self-registered row to go stale. This spec is specifically about the
  orchestrator's self-registered persisted identity.
- Any change to `attest_sender`'s relationship categories or the
  delivered-message wording (`SenderAttestation::render`) — those stay
  exactly as `v1-sender-identity-impl` defines them.
- Adding a caller-ownership/authorization check to `ninox restart` (or
  the GUI/TUI resume actions) itself. `restart_one_inner` today lets
  any local caller force-restart any live session, including another
  orchestrator's, with no ownership check — a real, pre-existing gap,
  but a restart-*authorization* gap, not an identity-*integrity* one:
  this spec's guarantee (no caller can control what a cleared row is
  replaced with) holds regardless of who is allowed to trigger the
  relaunch. Fixing who may call `ninox restart` is separate follow-on
  work, not addressed here.

## The core design question

Under what conditions is it safe to let an orchestrator's persisted
identity be replaced, given the row exists specifically to stop a
hijacked/respawned pane from impersonating a trusted orchestrator?

Options considered:

1. **Re-register on any mismatch, unconditionally.** Rejected outright
   — this is exactly the hole the write-once design prevents. A pane
   recreated by an attacker with `physical_tmux_name` equal to a real
   orchestrator id would satisfy today's migration-eligibility check
   (`physical_tmux_name == orchestrator_id` plus process ancestry from
   *a* root) the same way a legitimate restart would.

2. **A grace/cooldown window after the mismatch is first observed.**
   Rejected as the deciding mechanism. A timer measures elapsed time,
   not legitimacy — it neither confirms the old process is gone nor
   confirms who is asking. An attacker can simply wait out any fixed
   window. (A cooldown may still be useful as defense-in-depth around
   *how often* ninox retries re-registration attempts, but it cannot
   be the safety property itself.)

3. **Server-epoch change alone as sufficient restart evidence**,
   since `server_epoch` is already one of the matched fields and is
   daemon-controlled, not caller-controlled. Rejected as *sufficient*
   on its own: a daemon restart bumps the epoch for every orchestrator
   at once, including ones whose panes never crashed and whose
   identity is still perfectly valid — treating an epoch bump alone as
   restart proof would re-open the migration path for every
   orchestrator simultaneously, including to any attacker pane that
   happens to share a name with one of them, immediately after any
   daemon restart. Epoch remains one of the fields `runtime_identity_matches`
   checks for an already-registered identity, but is not by itself
   grounds for clearing a row.

4. **Gate re-registration on confirming the *old* identity's root
   process is actually dead — not merely mismatched — combined with
   confining the clearing code itself to ninox's own restart flow
   (`relaunch_in_place`), which runs as ninox's own trusted code (not
   caller-supplied) and already has in hand the exact orchestrator id
   and old pid it just killed. Accepted**, as the combination of both
   halves (note: this is a claim about where the *clearing code* runs
   and what it's allowed to do, not a claim that *triggering* a
   relaunch is itself authorization-gated — see the sub-bullet below
   and "Restart mechanics"):
   - *Death, not absence*: "the live fields no longer match" is not
     evidence the old process is gone — it could just as well mean an
     attacker is calling from a different process. The codebase
     already has the right primitive for genuine liveness:
     `lifecycle::probe::is_pid_alive` (a `kill(pid, 0)` probe, already
     used by the poller to decide when a session has actually died).
     Re-registration is gated on `!is_pid_alive(persisted.root_pid)`
     for the *specific* pid the current row names, not on any
     mismatch in the fields a spoofed caller could also fail to match.
   - *Confined to ninox's own relaunch code, not asserted by the
     triggering caller*: the clearing operation is only ever invoked
     from inside `relaunch_in_place` — never as a side effect of
     `authorize_orchestrator`/`attest_sender` observing a mismatch, and
     never exposed as a command of its own. `relaunch_in_place` itself
     is reachable from four call sites (`ninox fleet restore`, `ninox
     restart`, and the GUI/TUI resume actions — see "Restart
     mechanics"), and at least one of those (`ninox restart`) has no
     caller-ownership check today, so *triggering* a relaunch is not
     itself access-controlled. What is guaranteed regardless of who
     triggers it: the old-process-death check runs as ninox's own
     code, against the pid its own row already names — never a claim
     the triggering caller gets to make — and the row this clears can
     only ever be repopulated by ninox's own subsequent, independently
     re-verified self-registration, from whichever pane
     `launch_interactive_session` (ninox's own spawn code) actually
     started. A caller that can force a relaunch can force an
     orchestrator's pane to be killed and restarted — a pre-existing,
     separate availability gap (see Non-goals) — but it still cannot
     make the cleared row point at a pane or process of its own
     choosing.

Both conditions are necessary; neither alone is sufficient. Liveness
alone (a process merely happening to be dead, for any reason) says
nothing about whether this is a legitimate ninox-initiated restart.
Confining the clearing code to `relaunch_in_place` alone, without the
liveness check, would let a restart racing against a still-live old
pane (e.g. a stuck `kill_session` call) silently discard a still-valid
identity. Together, they bound re-registration to exactly the case
this spec exists to fix: a relaunch *ninox's own code* performed, of a
*specific* orchestrator, whose *old* process root is *confirmed* gone
— regardless of whether the call that triggered that relaunch was
itself from an authorized caller, which (per "Restart mechanics") it
is not always today.

## Design

### New store method: `delete_dead_orchestrator_runtime`

```rust
/// Clears orchestrator_id's persisted runtime identity iff a row exists
/// and its root_pid is exactly expected_root_pid — never a wildcard
/// delete. Returns whether a row was actually cleared.
pub fn delete_dead_orchestrator_runtime(
    &self,
    orchestrator_id: &str,
    expected_root_pid: u32,
) -> Result<bool>;
```

The `expected_root_pid` guard prevents a TOCTOU race: if some other
migration already replaced the row between the caller reading it and
calling this method (e.g. a legitimate concurrent re-registration), a
delete keyed only on `orchestrator_id` could blow away a *different*,
already-valid row. Keying the delete on the exact pid the caller
already confirmed dead makes it a no-op in that case instead.

This is a new, narrowly-scoped method, not a relaxation of
`register_orchestrator_runtime` (which stays production-uncalled, as
today) or of `register_migrated_orchestrator_runtime`'s
`ON CONFLICT DO NOTHING` semantics (which stay exactly as the
race-recovery fix in #200 left them).

### Trigger: inside `relaunch_in_place`, orchestrator path only

`spawn_util::relaunch_in_place` already knows `req.is_orchestrator` and
the pre-relaunch `req.session` (including its previously-recorded
root pid). Add, immediately after the existing
`ninox_core::runtime::kill_session(&id)` call and before (or
concurrently with) workspace/launch setup, when `req.is_orchestrator`
is true:

1. Read the orchestrator's current persisted identity via
   `store.orchestrator_runtime_identity(&id)`. If there is none,
   there is nothing to clear — proceed as today (a fresh orchestrator
   still hits the ordinary first-use migration path).
2. If a row exists, poll `lifecycle::probe::is_pid_alive(persisted.root_pid)`
   with a short bounded retry (mirroring the existing
   `wait_for_prompts`/`Duration`-based wait pattern already used
   elsewhere in `fleet.rs`), since `kill_session` may return before the
   OS has actually reaped the process.
3. If the old root pid is confirmed dead within that window, call
   `store.delete_dead_orchestrator_runtime(&id, persisted.root_pid)`.
4. If the wait times out without confirming death, do nothing — leave
   the row exactly as it is. The relaunch still proceeds (the new pane
   comes up either way); the orchestrator simply keeps failing its
   identity check until a later restart attempt gets another chance to
   confirm the old process is gone. This is a deliberate fail-closed
   choice (see "Interim behavior").

No new eager write of a *new* identity is needed. Once the stale row
is cleared, the existing lazy self-registration path —
`persisted_or_migrated_identity` (on `v1-sender-identity-impl`; today's
equivalent logic inline in `authorize_orchestrator`) — naturally
re-fires the next time the orchestrator takes any authorized action
(its first `ninox plan register`, `ninox send`, or any other
`authorize_orchestrator`/`attest_sender` call) from the new pane,
exactly the same way it bootstraps a brand-new orchestrator today.
This reuses the already-reviewed, already race-tested registration
path (the concurrent-registration fix landed in #200) instead of
introducing a second, parallel way to write an
`OrchestratorRuntimeIdentity` row.

### Why not expose this as a command or capability

The clearing operation is deliberately *not* added to the capability
registry (`ninox-core/src/capabilities.rs`), not given a `ninox`
subcommand, and not reachable from any agent-facing surface. It is
only ever called from inside `relaunch_in_place`. Exposing a general
"re-register my identity" command — even one gated on some check —
would reintroduce a caller-reachable path to clear a trusted
orchestrator's row, which is precisely what this design avoids by
construction.

## Interim behavior

Until this is implemented, today's behavior — a restarted
orchestrator's identity check fails permanently, rendering
`unverifiable`/`[ninox: sender identity could not be verified]` (or,
pre-`v1-sender-identity-impl`, a hard failure from `ninox plan
register`/`ninox reap`) — is **safe-by-default**. It fails closed, not
open: no caller gains any trust it shouldn't have; the cost is purely
availability/UX (an orchestrator that should be trusted is, until this
ships, treated the same as an unverified one would be). This spec
describes closing a usability gap, not an active vulnerability in the
current code.

## Testing

- **Legitimate restart re-registers successfully.** Register an
  orchestrator's identity against a real (test-process-owned) pid,
  kill that exact process, run the `relaunch_in_place` sequence (or a
  unit test exercising the same three steps directly:
  `orchestrator_runtime_identity` → `is_pid_alive` →
  `delete_dead_orchestrator_runtime`), then drive
  `persisted_or_migrated_identity`/`authorize_orchestrator` with a new
  `PaneIdentity` whose `physical_tmux_name` still matches the
  orchestrator id and whose pid is the current (self) process —
  assert it re-migrates and verifies, exactly as a brand-new
  orchestrator's first use does today.
- **Non-legitimate caller cannot force a clear.** Two angles:
  - `delete_dead_orchestrator_runtime` called with a stale/incorrect
    `expected_root_pid` (not the row's actual `root_pid`) is a no-op —
    asserts `Ok(false)` and the original row unchanged — proving the
    TOCTOU guard.
  - `authorize_orchestrator`/`attest_sender` themselves still never
    call any clearing path on a mere field mismatch: a caller
    presenting a live identity that doesn't match the persisted row
    (without going through `relaunch_in_place`) still gets rejected
    with the row left completely untouched, identical to current
    behavior — a regression test pinning that `authorize_orchestrator`
    has no route to clearing state.
- **Liveness-wait timeout leaves the row stale.** Old pid still alive
  when the bounded wait expires: `delete_dead_orchestrator_runtime` is
  never called, the row survives unchanged, and a subsequent
  `authorize_orchestrator` call for that orchestrator still fails
  exactly as before — guards against ever skipping the liveness
  confirmation for the sake of making the relaunch "just work."
- **All four relaunch callers share the fix with no duplicated
  logic.** Since the clearing behavior lives inside
  `relaunch_in_place` itself rather than in any one caller, the tests
  above already cover `ninox fleet restore`, `ninox restart`, and the
  GUI/TUI resume actions (`ninox-gui/src/app.rs:2063`,
  `ninox-tui/src/tui/mod.rs:1065`) identically — none of the four
  re-implements its own version of the kill+relaunch sequence. Worth a
  regression test (or an architectural lint) confirming none of the
  four ever bypasses `relaunch_in_place` to kill and relaunch an
  orchestrator pane directly, so a future caller can't silently skip
  this fix by reimplementing the relaunch inline.

## Open questions

- Exact bounded-wait duration for confirming the old process is dead
  before giving up for this attempt — needs tuning against real
  `kill_session` teardown latency per backend (tmux vs ptyd), left to
  the implementation plan.
- Whether `ninox restart` (single-session, interactive) and `ninox
  fleet restore` (batch) should surface different user-facing
  messaging when a stale identity is detected and cleared, versus when
  the liveness wait times out and the row is deliberately left stale.
- Whether a ptyd-hosted orchestrator pane (`runtime/ptyd_backend.rs`)
  needs a different liveness signal than `root_pid` for the same
  guarantee, given it may resolve `PaneIdentity` differently than the
  tmux backend.
