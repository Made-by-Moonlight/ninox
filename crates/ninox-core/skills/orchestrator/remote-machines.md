---
name: remote-machines
description: Attach an SSH-reachable machine's ninox sessions so they show up alongside local ones — add, list, and remove remote machine profiles.
---

# Remote Machines

Ninox can track sessions running on another machine you can reach over
SSH, using nothing but your own existing SSH setup (`~/.ssh/config`,
agent, keys, `known_hosts`) — ninox stores no credentials of its own.

## Adding a machine

    ninox machine add <host> [--label <name>] [--remote-session <name>]

`<host>` is anything `ssh` itself accepts: `user@host`, a bare host, or a
`~/.ssh/config` alias. This:

1. Connects over SSH (you may be prompted for a passphrase/host-key
   confirmation the first time, exactly as a normal `ssh` would).
2. Probes the remote end for a compatible `ninox` binary and a running
   service. If anything needs installing or updating, you are shown
   exactly what will change and asked to approve it explicitly — the
   default answer is **No**, nothing is installed or replaced without
   your say-so.
3. Lists sessions already running on that machine so you can pick one to
   track, or offers to spawn a `default` orchestrator there if none exist.
4. Saves the result as a machine profile — just connection metadata (an
   id, a label, the SSH target, and the tracked remote session name).
   Nothing about the remote session's content is ever stored locally.

## Listing and removing

    ninox machine list [--json]
    ninox machine remove <id>

`machine remove` only deletes the saved profile; it never touches
anything on the remote machine itself.

## Scope

This only reaches a machine's *already-running, already-persistent*
`ninox` service (the same `ninox service install` headless daemon you'd
run locally) — it does not keep a connection open for interactive
attach/reconnect yet. That's follow-up work.
