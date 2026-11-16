---
name: worker-status
description: report your activity state (working/idle/blocked) and declare dependencies on other workers.
---

# Report Your Status and Dependencies

Ninox shows a live fleet view of every worker's activity. Your
working/idle state is tracked automatically by hooks — you only need to
speak up for the states the hooks can't infer.

## Declare when you're blocked

If you cannot make progress until something outside your control changes
(another worker's PR, a review, an external system), say so:

```bash
ninox worker-status set blocked --note "waiting on worker-auth's schema PR"
```

The note is shown to the operator and the orchestrator — make it say what
you're waiting FOR, not just that you're waiting. The blocked state
survives the end of your turn; your next prompt automatically returns you
to working, or clear it yourself:

```bash
ninox worker-status set working
```

## Declare dependencies on other workers

If your task depends on another worker's task, register the edge (id or
name, from `ninox worker-status list`):

```bash
ninox worker-status depend <session> --note "needs its migration merged"
```

Branch stacking (your PR based on another worker's branch) is detected
automatically — `depend` is for dependencies git can't see. Remove an edge
that no longer holds:

```bash
ninox worker-status undepend <session>
```

## See the fleet

```bash
ninox worker-status list          # activity + dependency edges, live sessions only
ninox worker-status list --json   # same, machine-readable
```

## Reporting to your orchestrator

Your activity state is not a message. Text you write in your own pane
(a read-back, a question, "PR is open") never reaches your orchestrator.
Every report must go through `ninox send`:

```bash
ninox send <orchestrator> "read-back: ..."
```

`ninox send` prints a receipt:

- `delivered to <id> via <mechanism> (message N)`: it arrived.
- `queued for <id>: ...` (exit 0): the target hasn't started yet; it gets
  the message when it does.
- `queued, target unreachable: ...` (exit 75): the target has no live
  pane right now; the running Ninox daemon or app retries for 24h. Do not
  resend.
- Any other non-zero exit: not sent. Read the error.

If you sit idle owing a reply, Ninox nudges you once and tells your
orchestrator. To see what was sent to you, including anything a mechanism
failed to inject:

```bash
ninox messages --to-me --since 2h
```
