---
name: fleet-recovery
description: understand a fleet recovery briefing after a reboot or crash, and acknowledge it with `ninox fleet ack`.
---

# Fleet Recovery

When the machine reboots (or the terminal host dies), Ninox restores the
fleet in one ordered pass: your workers are resumed first, then you are, and
the first input you receive is a **recovery briefing** starting with
`[Ninox fleet recovery briefing]`. It is generated from Ninox's own store,
not from your conversation, and tells you:

- when you were interrupted and why (e.g. machine reboot);
- each worker's state: resumed and continuing its task, restarted fresh and
  re-briefed (its harness could not resume the conversation), finished with
  its PR and CI state, not restored because of a workspace anomaly, or
  Retained with uncommitted changes;
- pending `request-work` items and whether they were delivered;
- how many inbox messages are still waiting for you.

Treat the briefing as accurate. Resume coordination from it; do not
re-inspect every worker unless a line looks wrong. A worker listed as
**not restored** has a workspace problem Ninox refused to guess about
(missing worktree, wrong branch) — decide whether to fix it or reap it.

## Acknowledge recovery

Once you have taken stock, mark recovery complete:

```bash
{{NINOX_BIN}} fleet ack
```

This records that you are back in control, and marks the delivered
request-work items the briefing listed as handed over to you, so they do
not reappear in a future briefing.

## Inspect the fleet

```bash
{{NINOX_BIN}} fleet status          # interrupted sessions, anomalies, recovery state
{{NINOX_BIN}} fleet status --json   # machine-readable
{{NINOX_BIN}} fleet brief <id>      # re-print a briefing without acting
```

Never run `{{NINOX_BIN}} fleet restore` yourself unless the user asks — restoring
sessions is the user's decision.
