# MLOPS-5309 follow-up: real ghost proposals

Goal: ghost plans can be created, stored, shown and accepted, and the prompt
agent stops claiming actions it never ran. Two PRs against `v1`:

1. **Core + CLI** (this PR): store, validation, accept/discard engine,
   poller step, expiry, prompt-agent brief and permissions, skills.
2. **UI**: GUI Fleet ghost islands/nodes, detail panel Accept/Discard with
   inline confirm, Plans tab list, reply-drawer ghost cards; TUI fleet
   ghosts, plan list, `a` / `s` / `x` keys, reply-panel ghost cards.

## Store (`ninox-core`)
- `ghost_plans`: id, proposer, created_at, expires_at, status
  (pending/accepted/discarded/expired), target_parent (nullable: new
  top-level orchestrators), title, decided_by, decided_at.
- `ghost_nodes`: (plan_id, node_id), parent_node_id, worker_type, name,
  brief, workspace, model, brain (`BrainSet` JSON), dependencies,
  await_done, linear_ref, state (ghost/waiting/spawning/spawned/failed/
  discarded/expired), session_id, error/note.
- Node transitions are conditional UPDATEs, so a node is decided and
  spawned by exactly one caller. A plan's status settles from its nodes once
  none is undecided.

## Validation
`ghost_plan::validate_proposal` wraps `auto_plan::validate_plan` (types,
depth, cycles, lineage) and adds: unique non-reserved session names,
non-empty briefs, workspace on non-Orchestrator nodes, top-level nodes must
be Orchestrators when there's no parent, and parent type `can_spawn`.
`prepare_proposal` checks the target orchestrator and resolves each node's
brain set, narrowed from the proposer's own set.

## Authorization
`ghost_plan::authorize_decision`, fed by
`orchestrator_auth::resolve_live_caller_session_id`: the user (no managed
caller) may decide any plan; an orchestrator only plans attached under
itself or a descendant; the prompt agent and workers never. The prompt
agent's permissions also allow `plan propose/list/show` and deny
`plan accept/discard`.

## Accept engine (`ninox-ui-shared::ghost_accept`)
- `accept` / `discard` take the caller id and authorize inside.
- `advance` spawns ready `waiting` nodes top-down through
  `spawn_orchestrator_common_opts` / `admit_and_launch_worker` (extracted
  from `ninox spawn`), so session limits queue them and brain sets, worker
  types and models apply as for any spawn.
- Not-ready nodes wait; `Poller::with_plan_advancer` runs `advance_all` each
  tick (expiry sweep, stale-claim recovery, advance).

## CLI
`ninox plan propose [--file F]`, `plan list [--pending]`, `plan show <id>`,
`plan accept <id> [--nodes …]`, `plan discard <id> [--nodes …]`,
`plan generate --propose`. All print the existing plan JSON envelope.

## Tests
Core: propose/list/show, validation errors, depth, authorization, readiness
(parent, queued, await_done, discarded deps), single-winner transitions,
expiry, stale-claim recovery, turn-window matching. UI-shared: prompt agent
refused, user accept queues through the normal path and is idempotent,
dependency order through the poller step, parent-closure refusal, expired
accept refused, end-to-end worker launch with a stand-in harness.
