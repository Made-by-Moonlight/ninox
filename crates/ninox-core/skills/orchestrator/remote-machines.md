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

## Keeping it up to date

    ninox machine update <id>

Checks whether `<id>`'s remote binary matches this machine's own `ninox`
version, over the profile's existing SSH connection. If it's already a
match, this is a no-op. Otherwise it's approval-gated exactly like `machine
add` — you're shown the version change and asked to proceed, default
**No** — and on approval installs the matching build (reusing the same
cross-arch-aware install as `machine add`), then brings the remote's
long-running daemons onto it without dropping any live session: if
`ninox-ptyd` was already running there, it's live-upgraded in place (the
same `SCM_RIGHTS` pane handoff a local upgrade uses, triggered over SSH —
every pane survives; a failure here aborts the command, since the binary
is already installed and this step can be retried by hand once the
underlying issue is fixed); if nothing was running, there's nothing to
preserve and this step is skipped. The headless engine (`ninox-server`) is
then restarted — it holds no pane state of its own, so a plain restart is
safe, but only once the remote's service is confirmed to kill just its own
process rather than everything sharing its cgroup (systemd) or process
group (launchd); an older, not-yet-reinstalled service is reported as a
warning rather than restarted, until `ninox service install` is re-run
there to pick up the safer unit/plist. Finally, that machine's live session
panes are restarted so each one re-resolves the new binary for anything it
shells out to `ninox` for (hooks, statusline, the `nx` alias).

## Staying connected

A background health probe watches every enabled machine profile's SSH
connection and reconnects automatically after a network blip, sleep, or
SSH failure — with exponential backoff (capped around two minutes) so a
sustained outage doesn't hammer `ssh`. A background reconnect only ever
discovers; it never installs or replaces anything on the remote host
(that always goes through the explicit, approval-gated flow above). You'll
see `Machine unreachable` / `Reconnected` notifications as connectivity
changes.

## Scope

This only reaches a machine's *already-running, already-persistent*
`ninox` service (the same `ninox service install` headless daemon you'd
run locally) — interactive attach (`ninox connect`/`ninox pane attach` to
a remote pane) is follow-up work.
