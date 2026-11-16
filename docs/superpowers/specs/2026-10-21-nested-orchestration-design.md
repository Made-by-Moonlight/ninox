# Nested Orchestration: Spawn Authorization, Lineage, and the Depth Cap (Ninox "v1" next-gen line)

**Status:** Design spec — not scheduled, no implementation. Lives on the
long-lived `v1` branch.

**Implementation target:** `ninox-core` (`orchestrator_auth.rs`,
`store.rs`, `types.rs`) and the CLI spawn commands (`ninox-ui-shared/src/cli.rs`
— the former `ninox-app/src/main.rs` location, superseded by the
ninox-gui/ninox-tui split in PR #194). File and line references below
reflect the code as of this writing; treat them as orientation, not
guaranteed locations.

## Context

Item 7's fleet-overview UI/UX vision spec (merged) designed the UI and
depth-cap *policy* for "orchestrators of orchestrators" — real nested
delegation where an orchestrator spawns a sub-orchestrator that itself
spawns workers or further sub-orchestrators, hard-capped at depth 3 by
default. That spec explicitly does not cover backend spawn mechanics.
This spec does.

### A pre-existing authorization gap, found during this investigation — not nested-orchestration-specific, must be fixed regardless

`run_spawn` and `run_spawn_orchestrator`/`spawn_orchestrator_common`
call **no caller-identity verification at all**. They take
`--orchestrator-id` (or fall back to the ambient
`NINOX_ORCHESTRATOR_ID` env var) and trust it outright. This is
inconsistent with `ninox reap` (`caller_is_orchestrator`) and `ninox
plan` (`orchestrator_auth::authorize_orchestrator`), both of which
already verify the caller is a real, persisted orchestrator (via
tmux-pane identity + process-ancestry cross-check) before trusting an
orchestrator-id claim. **Today, any session can run `ninox spawn
--orchestrator-id <any-id>` and have it accepted with zero verification
it owns or is that orchestrator.** This predates and is independent of
nested orchestration — it is a real gap in already-shipped behavior,
and must be fixed as a prerequisite to anything in this spec, not
designed around.

### What else exists today

- **No lineage/depth concept anywhere in the data model.** `Session`
  has only a flat `orchestrator_id: Option<OrchestratorId>` pointing to
  an immediate parent; `Orchestrator` has no parent/lineage field at
  all. `is_orchestrator(id)` is pure flat set membership
  (`SELECT EXISTS(...) FROM orchestrators WHERE id=?1`).
  `NINOX_ORCHESTRATOR_ID` is always the immediate spawner's own session
  id, never accumulated into a chain.
- **`ninox spawn-orchestrator` already exists, but builds a peer, not a
  child.** It explicitly sets the new orchestrator's `orchestrator_id`
  to `None` and gives it its own independent `orchestrators` row with
  no back-reference to who spawned it — its own doc comment says "stand
  up a peer orchestrator session." This is the correct, intentional
  behavior for its existing use case (a human explicitly asking for
  another top-level orchestrator — see the `spawn-orchestrator` skill's
  own "use ONLY when the user explicitly asks" rule) and should **not**
  be changed. Nested delegation needs a new, separate capability
  alongside it, not a replacement.
- **Nothing technically stops a worker from spawning today — it's pure
  convention.** Worker worktrees are never seeded with
  `spawn-worker`/`spawn-orchestrator` skill files (only
  `setup_orchestrator_root` seeds those), so a worker's agent is simply
  never told the capability exists. There is no enforced boundary.
  Workers additionally run with `--dangerously-skip-permissions`
  (fully unattended) — a worker that did call a spawn command would do
  so completely unprompted, a materially different risk profile than
  an interactive orchestrator doing the same thing.
- The worker-permission-model spec (merged, unimplemented) is scoped
  entirely to tool-call permission lists (a merge gate, standing allow/
  deny rules) and worker-vs-own-forked-subagent containment — it says
  nothing about granting or withholding spawn/orchestrate capability.

## Goals

- Close the spawn-authorization gap: every spawn path (`ninox spawn`,
  `ninox spawn-orchestrator`, and the new nested-spawn capability below)
  verifies the caller via the same `orchestrator_auth`-style check
  `ninox reap`/`ninox plan` already use, before trusting any claimed
  orchestrator id.
- A new, explicit nested-spawn capability lets a verified orchestrator
  spawn a sub-orchestrator with a real parent link and enforced depth,
  distinct from the existing peer `spawn-orchestrator` (which keeps its
  current behavior and use case unchanged).
- Enforce item 7's depth cap (default 3, configurable) at spawn time:
  a request that would exceed it is rejected outright with a clear
  error — not silently truncated or allowed through.
- Gate the nested-spawn capability by an explicit, auditable permission
  (per session/worker-type), not by whether skill files happen to be
  seeded into a worktree — closing the "nothing stops a worker, it's
  just not conventionally told about it" gap.

## Non-goals

- Changing `spawn-orchestrator`'s existing peer semantics or its
  "only when the user explicitly asks" usage convention.
- The auto-planning/sectioning algorithm that decides *when* to spawn a
  sub-orchestrator (a separate, not-yet-specced item).
- Headless/unattended orchestrator autostart generally — flagged as a
  related open question in the worker-permission-model spec, not
  resolved here.

## Design

### Fix the authorization gap first

`run_spawn`, `run_spawn_orchestrator`, and the new nested-spawn command
all call `orchestrator_auth::authorize_orchestrator` (or an equivalent
caller-verification check reusing its tmux-pane/process-ancestry
primitives) before trusting any `--orchestrator-id`/
`NINOX_ORCHESTRATOR_ID` claim — exactly as `ninox reap`/`ninox plan`
already do. This is a prerequisite fix to already-shipped behavior, not
an optional hardening step gated on the rest of this spec.

### Lineage: reuse the existing parent pointer, walk it for depth

No new field on `Session` for the common case. A nested sub-orchestrator
reuses the existing `orchestrator_id: Option<OrchestratorId>` pointer,
now pointing at its parent *orchestrator's* id (today this field is
always `None` for an orchestrator row, an invariant this spec changes
for the nested case specifically). Depth is computed by walking that
pointer chain at spawn time — bounded by the cap itself, so at most 3
hops are ever walked — rather than storing a separate, independently
driftable depth counter. A sub-orchestrator's own children see
`NINOX_ORCHESTRATOR_ID` set to the *sub-orchestrator's* id (same
immediate-parent convention as today), not the root's; tracing the full
chain to the root happens via repeated lookups when needed (e.g. for
the tree-sidebar breadcrumb item 7 designed), not via a flattened list
carried through the environment.

### Depth-cap enforcement at spawn time

The nested-spawn path walks the chain from the requested parent,
counts hops to the root, and rejects outright (a clear, reported error)
if spawning would exceed the configured cap. Per item 7's design, the
caller is expected to spawn a plain worker instructed to report upward
instead, once capped — enforcing the cap is this spec's job; generating
that fallback instruction is left to whatever calls this capability
(most likely the eventual auto-planning algorithm).

### Permission gating

Calling the nested-spawn capability requires an explicit, enabled
permission on the calling session's worker-type profile (reusing the
worker-permission-model spec's scoped-profile concept, and item 7's
worker-type concept) rather than relying on which skill files happen to
be seeded. A root orchestrator has it by default; a plain worker does
not unless its worker-type profile explicitly grants it. Reconciling
exactly how this permission is named/represented across the item-7
worker-type spec and the worker-permission-model spec is left to the
implementation plan rather than inventing a third, parallel permission
concept here.

## Testing

- A test that `ninox spawn`/`spawn-orchestrator`/nested-spawn with a
  forged or unowned `--orchestrator-id` is rejected — mirroring
  existing `authorize_orchestrator` test coverage for `reap`/`plan`.
- A test that nested-spawn at exactly the cap (3 levels deep) is
  rejected with a clear error, and one level under the cap succeeds.
- A test that a worker-type profile without the nested-spawn permission
  is rejected even when given an otherwise-valid parent/orchestrator id.

## Open questions

- Exact mechanism for representing "this worker-type profile grants
  spawn permission" — needs reconciling the item-7 worker-type spec and
  the worker-permission-model spec rather than a third concept.
- Whether the depth cap should be global config or a per-root-
  orchestrator override — already flagged as open in item 7's spec,
  not resolved here either.
- Interaction with headless/unattended orchestrator autostart (flagged
  in the worker-permission-model spec's own open questions) — not
  resolved here.
