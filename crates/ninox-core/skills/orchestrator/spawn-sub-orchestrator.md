---
name: spawn-sub-orchestrator
description: Spawn a real sub-orchestrator nested under you — hard-capped by depth. Distinct from spawn-orchestrator (a peer); use only for genuine multi-level delegation.
---

# Spawn a Sub-Orchestrator — Real Delegation, Hard-Capped

You can hand a whole section of work to a sub-orchestrator: it manages its
own worker pool semi-autonomously, the same way you do. This is different
from `spawn-orchestrator`, which stands up an independent *peer* — a
sub-orchestrator is your child, and it reports back to you.

## The Rule

Use this only when a piece of work is genuinely large enough to need its
own coordinator — a whole section with several workers and its own
sub-plan, not something you could just hand to one worker.

```bash
{{NINOX_BIN}} spawn-sub-orchestrator \
  --name "billing-migration-payments" \
  --prompt "Own the payments section of the billing migration: <brief>"
```

Before choosing between a sub-orchestrator and plain workers, run
`{{NINOX_BIN}} worker-type list`: it shows every type you may spawn with
when to use it. Spawn a sub-orchestrator only when the section matches the
Orchestrator description (coordinating several workers on a multi-part
task); hand smaller pieces to the type whose description fits, `Implementor`
by default. The same goes for briefing the sub-orchestrator: tell it to pick
its own workers' types from that list.

`--name` is slugified into its session ID, same as every other spawn
command. `--orchestrator-id` defaults to `NINOX_ORCHESTRATOR_ID` (your own
id) — you don't need to pass it explicitly.

## The Depth Cap

Nesting is hard-capped (configurable via `ninox config set
nested_orchestration.max_depth <n>`, default 3). Once a chain would exceed
the cap, the command refuses outright — it will not silently truncate or
let you through.

**When you hit the cap: spawn a plain worker instead**, with a brief that
tells it to report back up the tree via a message rather than spawning
further:

```bash
{{NINOX_BIN}} spawn \
  --name "payments-reconciliation" \
  --prompt "<task>. Report progress and findings back to {{NINOX_BIN}} send <parent-id> \"...\"  — you may not spawn a sub-orchestrator; this chain is already at the depth cap." \
  --workspace /absolute/path/to/repo
```

## What the New Sub-Orchestrator Gets

- It inherits your brain set (narrow it with `--brain <name>`, repeatable,
  and `--brain-write <name>` — only ever to brains you read), the
  orchestrator skills, and its own workspace under the orchestrator root —
  same as `spawn-orchestrator`.
- Unlike a peer orchestrator, it *does* appear under you in the fleet tree,
  and its own `NINOX_ORCHESTRATOR_ID` is set to its own id — so anything
  *it* spawns (worker or further sub-orchestrator) reports to it, not to
  you directly.
- Message it the same way you message a worker: `{{NINOX_BIN}} send <id>
  "..."`.
