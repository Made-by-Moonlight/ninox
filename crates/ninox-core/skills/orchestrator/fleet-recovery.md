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
- If your briefing lists a pending user-request, call `ninox user-request wait <id>` to re-attach rather than asking again.
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

## Switching from a trial store

Sessions started under a separate store (a trial install, or an older
release's store) can be moved into the current one. They live in the
other store's database, so nothing is lost by switching, but only the user
decides to move them:

```bash
{{NINOX_BIN}} fleet import --from "<other store>/ninox.db" --dry-run   # what would come over
{{NINOX_BIN}} fleet import --from "<other store>/ninox.db"             # all or nothing
{{NINOX_BIN}} fleet restore --dry-run                                  # then resume them
```

- Stop the other store's daemon and app first, so no session runs in both.
- The source is only read. A copy is migrated, so an older release's store
  imports fine, and a store from a newer release is refused.
- Sessions that were live come in as Interrupted (cause "moved here from
  another Ninox store"). `fleet restore` resumes each one's conversation in
  its own worktree and branch, workers first. Orchestrator→worker links,
  PRs, plans, messages, requests and success criteria come with them.
- An id this store already has is skipped and reported. If it is the same
  session, it was already here. If it is a different session, it is
  skipped together with any workers it orchestrates, unless the user passes
  `--rename-conflicts` (imports it as `<id>-trial`).
- `--from-sessions-dir <dir>` also carries over undelivered inbox messages.
- Queued spawns that never started are listed, not imported. Re-spawn them.
