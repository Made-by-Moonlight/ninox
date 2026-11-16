# Resuming Interrupted Sessions After a Reboot

## Context

Ninox runs every orchestrator/worker as a `claude` (or other harness) CLI
process inside a detached pane on a private tmux server (`-L ninox`,
`crates/ninox-core/src/tmux.rs`). That server is configured with
`exit-empty off` so it survives with zero sessions as long as the machine
stays up — but a full reboot kills the server process itself, along with
every pane and the process running inside it.

On startup, `App::new` (`crates/ninox-app/src/app.rs`) already reconciles
the ledger against live tmux state: for every non-terminal session, if
`tmux::has_session` fails, the session is marked `Terminated`. This
correctly handles the common case (app crashes/relaunches, tmux server
survives — the session is still there and reattaches lazily on
`NavigateSession`) but is indistinguishable, today, from the reboot case:
either way the row just becomes `Terminated`, and the only way back is to
manually spawn a brand-new session by hand.

This was prompted by investigating a real instance of the reboot case
(2026-07-06): the private tmux server came back up empty, and the one
session in the ledger was already marked `Terminated`, with no path to
continue that conversation.

## Goal

When a reboot silently kills a session's tmux pane out from under it,
ninox should let the user pick that exact conversation back up — same
transcript, same context — with one click, instead of losing the row and
manually re-spawning from scratch.

## Non-goals

- No automatic/silent respawn on startup. An orchestrator resuming and
  autonomously acting (opening PRs, pushing commits) with nobody watching
  is a worse failure mode than a stale ledger row. Resume is always a
  user-initiated action (per-session or bulk "Resume all").
- No tmux-resurrect/tmux-continuum or any other tmux plugin. Investigated
  and rejected: those plugins only replay a pane's literal launch command;
  since `interactive_cmd` has no continuity flag today, that would just
  start a *new* blank conversation — no better than ninox's own ledger-
  driven respawn, while adding a third-party plugin to a deliberately
  isolated private tmux server.
- No `--continue`-based resume. Rejected during design: ninox workspace
  paths are not guaranteed unique forever — an orchestrator's workspace is
  `orchestrator_root/{slugified-name}`, and a worker's is
  `{repo}/.claude/worktrees/{session_id}` (also name-derived), both of
  which reuse the same directory across separate spawns when a name is
  reused. `--continue` resumes "the most recent conversation in this
  directory," which is ambiguous in exactly that case. The explicit
  `--session-id`/`--resume <uuid>` mechanism below sidesteps this
  entirely.
- No changes to harnesses other than `claude-code`'s spec data. Other
  harnesses simply don't get `resume_args`, so their sessions fall back to
  today's plain `Terminated` behavior on reboot — no Resume button, no
  regression.

## Approach

The `claude` CLI supports assigning an explicit conversation ID at spawn
time (`--session-id <uuid>`) and resuming that exact conversation later by
ID (`-r`/`--resume <uuid>`), independent of the working directory. Ninox
generates that UUID itself at spawn time, stores it on the session row,
and uses it to resume precisely — with no dependency on "most recent
conversation in this directory" and no filesystem scraping of
`~/.claude/projects/*` to guess which transcript belongs to which session.

## Architecture

### Data model

New nullable column on `sessions`:

```sql
ALTER TABLE sessions ADD COLUMN claude_session_id TEXT;
```

`None` for legacy rows (predate this feature) and for sessions spawned
under a harness with no `resume_args` (see below) — both cases simply
aren't resumable, which is the existing degrade-safely shape the codebase
already uses elsewhere (e.g. `catalogue_path`, `model`).

New `SessionStatus` variant:

```rust
pub enum SessionStatus {
    Spawning, Working, PrOpen, CiFailed,
    ReviewPending, Mergeable, Done, Terminated,
    Interrupted,
}
```

`Interrupted` means "process is gone, but a resumable transcript exists."
It is deliberately distinct from `Terminated` ("gone for good").

**Every existing `matches!(status, SessionStatus::Done | SessionStatus::Terminated)`
terminal-state guard must add `Interrupted` to the pattern.** This is not
optional polish — it's required for correctness. There are five such
guards today, all treating "terminal" as "nothing left to poll for this
session":

- `lifecycle/poller.rs`: `poll_pids`, `sync_sessions_metadata`,
  `poll_usage`, `poll_github` (one guard each).
- `app.rs`'s startup reconciliation task itself.

The sharp edge this closes: `poll_pids` runs every 5 seconds and marks any
non-terminal session `Terminated` if its stored `pid` is dead. An
`Interrupted` session's `pid` is, by definition, dead (that's *why* it's
`Interrupted`) — without adding `Interrupted` to `poll_pids`'s guard, the
very next tick after the startup task marks a session `Interrupted` would
immediately overwrite it back to `Terminated`, silently destroying the
resumable state seconds after it's set. `pid` itself is left stale/unused
on an `Interrupted` row until Resume overwrites it with the new pane's
pid.

### Harness registry

`HarnessSpec` (`crates/ninox-core/src/harness.rs`) gains:

```rust
pub resume_args: Vec<String>, // empty = harness can't resume
```

`claude-code`'s builtin spec changes:

- `interactive_args` and `worker_args` both gain
  `"--session-id".into(), "{session_id}".into()` — every spawn (orchestrator,
  standalone, or worker) is assigned a UUID from birth.
- New `resume_args: vec!["--dangerously-skip-permissions".into(),
  "--resume".into(), "{session_id}".into(), "--model".into(),
  "{model}".into()]` — `--dangerously-skip-permissions` because a resumed
  session has no one at the keyboard to click through permission prompts.

`{session_id}` is expanded by `expand_args` exactly like `{model}`/
`{prompt}` today (shell-quoted substitution, no dropped-flag handling
needed since ninox always has a session id to supply).

`HarnessRegistry::resume_cmd(agent: &AgentConfig, session_id: &str) ->
Option<String>` mirrors `worker_cmd`'s shape: `None` when
`spec.resume_args` is empty.

**Resume always goes through `resume_cmd`, never `worker_cmd`** — even for
sessions originally spawned as one-shot workers. The original prompt is
already in the transcript; replaying it via `worker_cmd`'s trailing
`-- {prompt}` would re-inject a stale instruction instead of continuing
from where the conversation left off.

### Spawn-time change

`spawn_util.rs`'s spawn paths (orchestrator, standalone, worker — all
three call sites that build a launch command) generate a UUID
(`uuid::Uuid::new_v4()`, promoting the already-transitive `uuid` crate to
a direct `ninox-core` dependency) before building the command, thread it
through as `{session_id}`, and persist it on the `Session` row at creation
alongside the other fields already set there.

### Startup reconciliation

`App::new`'s startup task (`crates/ninox-app/src/app.rs`) changes from:

```
tmux session gone → mark Terminated
```

to:

```
tmux session gone:
  claude_session_id present AND harness has resume_args → mark Interrupted
  otherwise                                              → mark Terminated (unchanged)
```

This is the only place resume-eligibility is decided. Everything
downstream (UI, resume action) trusts `SessionStatus::Interrupted` as "this
row has what it needs to resume."

### UI

- Fleet board / session list render `Interrupted` with its own badge,
  distinct from `Terminated`.
- A **Resume** action on an interrupted session's row/detail view, plus a
  bulk **"Resume all (N)"** control (natural home: wherever the existing
  notification/summary surface lives).
- Resume: build `resume_cmd` from the session's stored `agent_type`/
  `model`/`claude_session_id`, call `tmux::create_session` in the
  session's existing `workspace_path` (no mkdir/worktree recreation — the
  directory is still there), read back the new pane's pid the same way a
  fresh spawn does (`spawn_util.rs`'s existing pid-lookup-after-create
  pattern), set `status = Working`, and attach the UI client
  (`Message::ClientAttach`, same path `NavigateSession` already uses).

## Error handling

- Harness with no `resume_args` → session goes straight to `Terminated` on
  reboot detection, same as today; no Resume affordance is ever shown for
  it.
- `tmux::create_session` fails on Resume (e.g. tmux itself unavailable) →
  surface the error inline where the Resume action was triggered; the
  session stays `Interrupted` so the user can retry rather than the row
  being lost or silently flipped to some other state.
- Workspace directory missing (worktree pruned, orchestrator dir manually
  deleted) → `create_session`'s `-c <workspace>` fails at the tmux layer;
  same inline-error/stays-`Interrupted` handling as above — no special
  case needed.

## Testing

- `harness.rs`: `resume_cmd` unit tests — claude-code with/without model,
  a harness with empty `resume_args` returns `None`, `{session_id}`
  substitution is shell-quoted like `{model}`/`{prompt}`.
- `spawn_util.rs`: spawning a session persists a non-empty
  `claude_session_id`; the generated UUID appears in the actual launch
  command handed to `tmux::create_session`.
- `app.rs` startup reconciliation (regression-critical — this changes
  existing behavior): a session with `claude_session_id` + dead tmux →
  `Interrupted`; a legacy session (no `claude_session_id`) + dead tmux →
  `Terminated`, unchanged from today; a session under a harness with no
  `resume_args` + dead tmux → `Terminated` even if `claude_session_id`
  happens to be set.
- Resume action: given an `Interrupted` session, triggering Resume calls
  `tmux::create_session` with a command containing `--resume <uuid>`,
  updates status to `Working`, and updates `pid`. Resume when
  `tmux::create_session` errors leaves status `Interrupted` and surfaces
  the error.
- `poller.rs` regression test (the sharp edge above): an `Interrupted`
  session with a stale dead `pid` survives `poll_pids`, `poll_usage`,
  `poll_github`, and `sync_sessions_metadata` unchanged — none of them may
  flip it to `Terminated` or otherwise touch it.
