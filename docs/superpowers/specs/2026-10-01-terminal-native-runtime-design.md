# Terminal-Native Runtime and Durable Fleets

**Status:** Future plan — not scheduled. Captures the agreed direction so
later implementation plans can be cut from it.

**Implementation target:** the extended edition of Ninox, not this
repository. File and function references below point at this repository's
code and may have moved or grown there; treat them as orientation, not
exact locations.

## Context

Ninox is a desktop app (Iced) that shows agent sessions through an embedded
terminal. Every session runs in a pane on a private tmux server (`-L ninox`).
To display a pane, the app spawns a hidden `tmux attach` client on a PTY it
owns (`ninox-core/src/client.rs`), feeds that output into
`alacritty_terminal`, and paints the resulting grid on an Iced canvas
(`ninox-app/src/components/terminal.rs`).

That pipeline runs two terminal emulators in series:

```
agent → tmux (emulator #1, re-encodes output) → hidden attach PTY
      → alacritty_terminal (emulator #2) → Iced canvas
```

Roughly a sixth of the repository's history is terminal rendering fixes:
flicker, partial repaints, split synchronized frames, cold-start glyphs,
history hydration, BiDi, wheel routing, selection and copy. The flicker
investigation (`docs/terminal-live-flicker-evidence.md`) shows the root
cause: tmux re-renders the inner application and splits one synchronized
update into several outer commits, so Ninox has to infer frame boundaries
from a stream that no longer carries them. History hydration through
`capture-pane` is another workaround for not owning terminal state.

Separately, restarts are expensive. The tmux server survives an app restart
but not a reboot. On startup every dead session becomes `Interrupted`
(`reconcile_live_sessions_at_startup_with`), resume is manual and per
session, and a resumed orchestrator wakes with a stale picture of its fleet
and has to rediscover worker state with `ninox workers`.

## Goals

1. Eliminate the terminal-rendering bug class by removing the double
   emulation, not by patching around it.
2. Run entirely in the user's terminal as a TUI — no GPU window, no bundled
   fonts.
3. Make the runtime a long-lived server with the UI as a client, so closing,
   crashing or upgrading the UI never touches agents.
4. Make engine upgrades and crashes never kill agent processes.
5. After a machine reboot, restore the whole fleet in one ordered operation,
   and hand each orchestrator an accurate briefing instead of making it
   rediscover state.

## Non-goals

- Keeping processes alive across a reboot. Impossible; recovery relies on
  harness-level conversation resume (`claude --resume <uuid>`).
- A plugin system.
- Windows support in the first iteration (the design must not preclude it).
- Pixel-level parity with the Iced UI. The brain reader in particular
  becomes a plainer terminal markdown view.

## Architecture

```
ninox-ptyd (PTY host — tiny, stable, rarely upgraded)
 ├─ spawns harness processes on PTYs it owns (no tmux)
 ├─ one terminal emulator per pane, fed raw PTY bytes
 ├─ scrollback + periodic screen checkpoints to disk
 └─ local socket: spawn / kill / write input / resize / read screen / subscribe

ninox server (engine — upgraded freely)
 ├─ existing ninox-core: sessions, orchestrators, workers, worktrees,
 │  brain, lifecycle, cost, PRs, inbox
 ├─ SessionBackend implemented over ninox-ptyd
 ├─ fleet restore + recovery briefings
 └─ client socket API (JSON): control, events, input, screen frames

ninox (TUI client — ratatui)
 └─ fleet board, sidebar, agent panes, modals; sends input, receives frame diffs
```

### 1. PTY host (`ninox-ptyd`)

The only process that holds agent PTY master fds. Its surface is small and
versioned so the engine can be upgraded, restarted, or crash without
touching agents; on reconnect the engine re-adopts panes by stable pane id.

- Spawns processes directly with the environment the engine provides
  (`NINOX_SESSION_ID`, role, socket path, etc.).
- Feeds raw PTY output into one emulator per pane. Hidden panes keep
  parsing so state is always current, but never trigger presentation work.
- Keeps scrollback in memory and periodically checkpoints each pane's
  recent screen to disk for post-reboot display.
- Upgrading the host itself is rare; when needed, it hands live fds to its
  successor over a Unix socket (`SCM_RIGHTS`). Loss of the host falls back
  to fleet restore (§5).

### 2. Terminal emulation

- Start with `alacritty_terminal` — pure Rust, already a dependency, and
  not the source of today's bugs (tmux in front of it was).
- Hide it behind a narrow `TerminalEngine` trait (feed bytes, resize, read
  cells/cursor/modes, scrollback, snapshot) so it can be swapped for another
  engine later without touching the rest of the system.
- Because the emulator sees the application's own bytes, synchronized
  output (DEC 2026) arrives intact; there are no re-encoded or split frames
  to reassemble, and no `capture-pane` hydration.

### 3. Engine and `SessionBackend`

`ninox-core/src/tmux.rs` exposes ~24 operations the engine relies on
(create/kill session, per-session env, caller pane identity, `send_keys`,
`paste_buffer`, `wake_idle_session`, history capture, …). These move behind
a `SessionBackend` trait:

- `TmuxBackend` — today's behaviour, used during migration.
- `PtydBackend` — the target.

Consequences of the PTY-host backend:

- Caller identity (`ninox` CLI invoked from inside a worker) comes from
  injected env vars plus a ptyd lookup, not tmux pane queries.
- Messaging writes directly to the pane's PTY; idle detection can use the
  emulator snapshot instead of keystroke-verification workarounds.
- The tmux config writer, hidden attach client (`client.rs`), output framer
  and viewport-tail hydration are deleted.

### 4. Client protocol and TUI

- The engine renders each client's view into a virtual ratatui buffer and
  streams only the diff as ANSI to the client, which writes it to the real
  terminal. This makes detach/reattach, multiple simultaneous clients and
  remote viewing fall out naturally. The protocol should be designed for
  this from day one even if an early prototype renders client-side.
- Pane content is composited by Ninox, so the fleet board can show live
  previews of several agents, layouts are unconstrained, and mouse and
  keyboard are both first-class.
- Keybindings use a single prefix key that cannot collide with a user's own
  tmux, since Ninox no longer runs inside or alongside tmux.
- UI state (selection, modals, scroll position, layout choice) belongs to
  the client; shared facts (sessions, status, PRs, cost, pane metadata)
  belong to the server and are exposed through the API. New shared
  behaviour must not be reachable only through a private client path.

### 5. Durable fleets

#### 5.1 Restart matrix

| Restart | Agents | Target behaviour |
|---|---|---|
| UI closed / crashed | Keep running | Reattach; server repaints from live state |
| Engine upgrade / crash | Keep running (held by ptyd) | Engine re-adopts panes on start |
| ptyd upgrade | Keep running (fd handoff) | Best effort; falls back to fleet restore |
| Machine reboot | Die | Automatic ordered fleet restore + briefings |

#### 5.2 Fleet state is authoritative in the store

Everything an orchestrator needs to resume must be persisted as it happens,
not held only in its conversation:

- orchestrator ↔ worker ownership (exists)
- worker incarnations, worktree, branch (exists)
- PR state (exists)
- inbox / undelivered messages (exists)
- **new:** each worker's task brief as issued by the orchestrator
- **new:** outstanding `request-work` items and what the orchestrator is
  waiting on
- last known status and the time of interruption

#### 5.3 Ordered fleet restore

One operation, replacing per-session manual resume:

1. **Validate** each interrupted session's workspace: worktree present,
   branch as recorded, clean or dirty. Report anomalies; never guess.
2. **Resume workers first** (`--resume <claude_session_id>`), so the fleet
   is live before its orchestrator wakes.
3. **Resume orchestrators last**, with a recovery briefing as their first
   input, generated from the store. Example:

   > You were interrupted at 14:02 (machine reboot). Workers A and B were
   > resumed and are continuing tasks X and Y. Worker C had finished; its
   > PR #41 is open and CI is green. Worker D is Retained with uncommitted
   > changes in `<path>`. Two request-work items are pending: … Resume
   > coordination from here; do not re-inspect workers unless something
   > above looks wrong.

4. **Redeliver** undelivered inbox messages.
5. Harnesses without resume-by-id restart fresh in their workspace and
   receive the same briefing (conversation lost, context not).

#### 5.4 Autostart and restore policy

- A systemd user unit (Linux) / launchd agent (macOS) starts ptyd and the
  engine at login.
- `restore_policy` in config:
  - `manual` — default; matches the existing decision that an orchestrator
    must never act unattended without consent.
  - `prompt` — the TUI opens with "Restore fleet (N workers, M
    orchestrators)?"
  - `auto` — restore immediately after boot.

#### 5.5 Screen checkpoints

Restored panes display their last checkpointed screen (dimmed, marked
restored) until the resumed harness repaints, so context is visible
immediately.

## Phasing

Each phase is independently shippable.

1. **Ordered fleet restore + recovery briefing on the current tmux
   backend.** Delivers the restart improvement immediately; the logic moves
   to the new backend unchanged later. Includes the new store fields in
   §5.2.
2. **`SessionBackend` seam.** Refactor `tmux.rs` callers behind the trait;
   no behaviour change.
3. **`ninox-ptyd` + `PtydBackend`**, validated headlessly with replay tests
   built from captured Claude Code / Codex output. Largest technical risk —
   prove it before committing to the rest.
4. **Client protocol + TUI MVP:** fleet board, single agent pane,
   spawn modal, attach/detach.
5. **TUI parity:** layouts, live previews, PRs, notifications, settings,
   brain, inspector.
6. **Autostart, restore policy, screen checkpoints, ptyd fd handoff.**
7. **Cutover:** `PtydBackend` becomes default; remove `TmuxBackend`, the
   Iced app, `alacritty` canvas code, and the tmux config writer.

## Risks

| Risk | Mitigation |
|---|---|
| Emulator correctness for complex agent UIs | Use a mature engine; replay tests from real captures; `TerminalEngine` trait allows swapping |
| Render cost scales with panes × clients | Hidden panes never trigger presentation; benchmark 1 vs 15 populated panes from phase 3 |
| ptyd crash kills every agent | Keep it minimal and panic-safe; no engine logic in it; fleet restore as backstop |
| Migrating live tmux fleets | Cutover lets tmux sessions finish or resumes them on the new backend via §5.3 |
| Unattended orchestrators after `auto` restore | Default stays `manual`; `auto` is explicit opt-in |

## Open questions

- Server-side vs client-side rendering for the first TUI prototype.
- Whether ptyd and the engine ship as one binary with two modes or two
  binaries.
- Exact briefing format and whether orchestrators acknowledge it via a CLI
  call (`ninox fleet ack`) so the engine knows recovery completed.
