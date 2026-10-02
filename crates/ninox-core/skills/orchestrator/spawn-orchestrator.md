---
name: spawn-orchestrator
description: Use ONLY when the user explicitly asks for another orchestrator. Never spawn one on your own initiative — work you decided to delegate goes to a worker instead.
---

# Spawn an Orchestrator — Only When Asked

You can stand up another orchestrator. You almost never should.

An orchestrator is a *peer*, not a subordinate: it coordinates its own fleet
of workers, spends its own context, and costs money for as long as it runs.
Nothing about your own workload justifies creating one.

## The Rule

**Spawn an orchestrator only when the user explicitly asks for one.**

If you are thinking "this would go faster with another orchestrator", the
answer is a worker (`{{NINOX_BIN}} spawn`). Workers are the unit of
delegation — always.

The command enforces this with a flag you must pass deliberately:

```bash
{{NINOX_BIN}} spawn-orchestrator \
  --name "billing-migration" \
  --prompt "Coordinate the billing migration: <goal, scope, constraints>" \
  --user-requested
```

`--user-requested` is your assertion that the user asked for this
orchestrator in this conversation. Without it the command refuses. Do not
pass it to get around the refusal.

| Thought | Reality |
|---|---|
| "Two orchestrators would parallelize this" | Spawn more workers instead. |
| "This work is a separate concern" | Separate concern, same fleet. Spawn a worker. |
| "The user would probably want one" | Probably isn't asked. Ask them. |
| "I'll spawn one and mention it after" | The user decides, before. |

## What the new orchestrator gets

- `--name` is slugified into its session ID (`"Billing Migration"` →
  `billing-migration`), which must not collide with an existing session.
- `--prompt` is delivered as its opening brief once its harness is ready,
  with a footer telling it that it reports back to you. Omit it to start it
  empty and follow up with `{{NINOX_BIN}} send <id> "..."`.
- It inherits the same brain, the orchestrator skills, and its own workspace
  under the orchestrator root.

It is a peer, so it does not appear under you on the fleet board and you
cannot reap it — `{{NINOX_BIN}} reap` only ever touches your own workers.
