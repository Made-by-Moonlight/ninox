# Terminal mode: TUI + CLI-only operation

## Problem

Every ninox feature that matters — spawning orchestrators and workers,
session lifecycle, PR watching, messaging, the brain — already runs
outside the desktop GUI: sessions live on a private tmux server
(`tmux -L ninox`), state lives in a multi-process-safe SQLite store, and
the background services (lifecycle poller, ninox-server) are plain tokio
tasks. The Iced GUI is one view over those seams, but today it is the
*only* interactive view. A user on a remote box, over SSH, or who simply
prefers the terminal cannot list sessions, start an orchestrator and get
into it, or connect to a running worker without the desktop app.

`ninox --headless` already starts the poller and HTTP server without a
window, and `ninox spawn-orchestrator` is already a complete
app-independent spawn — but they are agent-facing plumbing, not a usable
human workflow, and headless mode silently skips the dead-session
reconciliation that only runs in the GUI's startup task.

## Goals

- Full interactive use of ninox from a terminal: list sessions, start
  an orchestrator and land inside it, connect to any worker or
  orchestrator, kill/reap.
- A ratatui TUI as the interactive front, plus thin one-shot
  subcommands for scripting.
- Background services keep working with no GUI: the `--headless`
  process is the daemon, auto-started on demand.
- No behavior change for desktop users: bare `ninox` on a machine with
  a display still opens the GUI.

## Non-goals (deferred, not rejected)

- `resume` / `re-file` from the terminal (`refile_plan` / `resume_plan`
  are already pure functions in `ninox-app`; exposing them later is
  thin).
- Settings/config editing from the terminal.
- Terminal-side notifications (the TUI's live board covers the main
  need; `Event::Notification` streaming can come later).
- New capability-registry entries: `list` / `connect` / `orchestrate` /
  `tui` are human-facing. Agents keep their existing `spawn` / `reap` /
  `send` / `open` surface.

## Design

### 1. Daemon: formalize `--headless` with auto-start

The process started by `ninox --headless` (poller + ninox-server) *is*
the daemon — there is no new infrastructure.

- **`ensure_daemon()`** (new, `ninox-core`): probe whether ninox-server
  is listening on the configured port. That TCP bind is the liveness
  check and the de-facto lock — whether the GUI or a headless process
  holds it, the services are running. If nothing is listening, spawn
  `ninox --headless` as a detached child (stdout/stderr to a log file
  under the ninox data dir) and wait briefly for the port to come up.
- Called at TUI startup and by `orchestrate`. `list` and `connect` read
  the store / tmux directly and do not require the daemon (they print a
  hint when it is down rather than blocking on it).
- **Reconciliation moves into the poller**: the dead-tmux-session →
  `Interrupted`/`Terminated` sweep currently runs only in `App::new`'s
  startup task (`app.rs:857-894`). It moves into `Poller::start` (first
  tick), so headless/TUI operation stops missing it. The GUI keeps
  working unchanged since it hosts the same poller.

### 2. One-shot subcommands

All three follow the existing short-circuit pattern in `main.rs`
(return before tmux-config / wrapper / self-shim setup, like
`capabilities` and `open`/`close`/`list --prs`), except `orchestrate`,
which needs the full setup path like `spawn-orchestrator`.

- **`ninox list [--json]`** — bare `ninox list` stops erroring
  ("nothing to list — pass --prs") and prints the session board:
  orchestrators with their workers nested, showing id, name, status,
  repo, PR number, and cost. `--json` emits the raw rows.
  `ninox list --prs` keeps its existing PR-watch meaning unchanged.
- **`ninox connect <session-id>`** — mirrors the GUI's
  `NavigateSession` handler: if `tmux::has_session` fails, mark the row
  `Terminated` (via the read→apply→write store contract) and report it;
  otherwise exec `tmux::attach_args(&id)` (which already handles the
  legacy default-socket fallback). tmux detach returns the user to
  their shell. Works identically for workers and orchestrators.
- **`ninox orchestrate <name> [--prompt <brief>] [--no-attach]`** — the
  human-facing sibling of the agent-facing `spawn-orchestrator`. The
  spawn body is shared (extracted from `run_spawn_orchestrator`); the
  differences are: no `--user-requested` flag (the invoker is the
  user), and after the spawn it execs the attach so the user lands
  inside the new orchestrator session. `--no-attach` instead prints the
  workspace directory and a `ninox connect <id>` hint. Passing
  `--prompt` keeps the existing wait-for-input-prompt + deliver-brief
  behavior; when attaching, the brief is delivered before the attach so
  it is never lost to a not-yet-drawn input box.

### 3. The TUI

- **Stack**: ratatui + crossterm, as a new `tui/` module in
  `ninox-app`. It reuses `Store`, `Engine`, `tmux`, and the spawn
  helpers — a second view over exactly the seams the Iced GUI uses.
- **Launch**:
  - `ninox tui` — always available.
  - Bare `ninox` with no display (`!has_display()`) and stdout a TTY
    opens the TUI instead of today's silent Ctrl-C wait. With no
    display and no TTY (e.g. under a service manager), behavior is
    unchanged: headless wait. `--headless` also stays unchanged and
    never opens the TUI.
  - The TUI calls `ensure_daemon()` at startup, so opening it is the
    only thing a CLI-only user has to do.
- **Main view**: live session board — orchestrators with nested
  workers, columns for status, repo, PR state, cost, and context usage.
  Data refreshes by polling the store on a ~1s tick (same data the GUI
  renders; SQLite reads on a WAL store are cheap and the store is the
  designed multi-process boundary).
- **Keybinds**:
  - `Enter` / `c` — connect: the TUI tears down the terminal (leave
    alternate screen, disable raw mode), runs `tmux attach` as a child
    process, and restores itself when the user detaches (the
    lazygit-shells-out pattern).
  - `n` — new orchestrator: prompt for name and optional brief, then
    spawn (shared body from §2) and offer to connect.
  - `d` — kill/terminate selected session, with confirm
    (`Engine::terminate_session`); `k` is taken by vim-style up.
  - `x` — reap finished workers of the selected orchestrator, with
    confirm (`Engine` reap path shared with `ninox reap`).
  - `p` — PR watches view (`store.list_pr_watches`).
  - `q` / `Esc` — quit.
- The TUI never writes stale snapshots: any row mutation goes through
  the same read→apply→write pattern as `update_live_session_row`.

### 4. Enabling refactor: move orchestrator-root setup to core

`setup_orchestrator_root` and `seed_orchestrator_skills` (currently in
`ninox-app/src/app.rs`) have no Iced dependencies. They move to
`ninox-core` so the GUI, the TUI, and the subcommands share one setup
implementation. The capability-registry invariant tests that pin seeded
files to the registry move (or point) with them; the registry itself is
untouched. The `spawn_util` worktree/seeding helpers stay in
`ninox-app` for now — every consumer in this feature lives in that
crate, so moving them would be churn without a consumer; they move when
a core-side consumer actually appears.

## Error handling

- `connect` to an unknown id: list near-miss ids (prefix match) and
  exit non-zero.
- `connect` to a dead session: mark `Terminated`, say so, exit
  non-zero.
- `orchestrate` duplicate name: existing duplicate-id guard and
  rollback semantics from `run_spawn_orchestrator` are preserved
  (delete both rows if tmux creation fails; never leave a ghost
  orchestrator row).
- `ensure_daemon()` spawn failure: warn and continue — `list`/TUI
  reading the store still works, statuses may be stale; the TUI shows a
  "daemon down" indicator rather than refusing to start.
- TUI panic safety: terminal state (raw mode, alternate screen) is
  restored via a panic hook so a crash never leaves the user's terminal
  broken.

## Testing

- `ninox list`: unit tests for grouping/formatting and `--json` output
  against a temp store; regression test that `list --prs` output is
  byte-identical to today.
- `ensure_daemon()`: port-probe logic tested against a listener on an
  ephemeral port; spawn path behind a trait/injection so tests don't
  fork real daemons.
- Reconciliation-in-poller: existing `reconciled_status_for_dead_session`
  tests move with the function; new test that a dead session is swept on
  the poller's first tick.
- TUI: state logic (selection movement, keybind dispatch, confirm
  flows) tested headlessly against a fake store; rendering smoke-tested
  with ratatui's `TestBackend`.
- Manual: attach/detach round-trip from TUI and `ninox connect`;
  `orchestrate` end-to-end including brief delivery; bare `ninox` on a
  display-less SSH session opens the TUI.

## Rollout / compatibility

- No config-format changes; no new config toggles (auto-start daemon is
  behavior, not opt-in — it only ever starts the same services the GUI
  would).
- `ninox list --prs`, `spawn-orchestrator`, and all agent-facing
  commands are unchanged.
- The only changed default: bare `ninox list` (previously an error) and
  bare `ninox` without a display on a TTY (previously a silent wait).
