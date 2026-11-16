# Restart server (CLI, GUI, TUI)

**Goal:** restart the background daemon (`ninox --headless`) from the app after
a login (`aws sso login`, `gh auth login`, a Claude login) or a new build,
without losing running sessions.

## Why sessions survive

The daemon only runs the poller and the HTTP server. Panes are hosted
elsewhere:

- **ptyd:** the host runs in its own session (`setsid`, `ninox-ptyd/src/client.rs`).
- **tmux:** the tmux server daemonizes itself.

`crates/ninox-tui/tests/server_restart_live.rs` proves this on both backends
against a scratch daemon, which is ignored by default and run explicitly. It
checks that each pane's pid stays alive, its row is not marked Interrupted
after the new poller's first cycle, and `daemon.log` has no restore or
relaunch lines.

## Design

1. **Identity:** the server answers `GET /api/server/info` with version,
   build (`NINOX_BUILD_HASH` from `ninox-core/build.rs`), pid, port, db path
   and current background issues. The pid comes from here, so `lsof` is only
   a last resort, with a 2s timeout.
2. **Shutdown:** this goes over a Unix control socket, not HTTP. HTTP is
   loopback TCP with permissive CORS, so it can't tell who is calling. The
   socket gives the daemon the kernel-reported peer pid, and the daemon runs
   that pid through `orchestrator_auth::resolve_live_session_id_for_pid`.
   - A peer with no live managed session in its ancestry is the user, and is
     accepted.
   - Anything else is refused: an agent session, an unknown pid, another
     uid, or a store error.

   The socket lives in `$TMPDIR/ninox-ctl/` (0700) and is keyed by port and
   db path. The headless daemon now also stops gracefully on SIGTERM.
3. **Restart routine** (`ninox_ui_shared::server_restart`, shared by all
   three surfaces). Each step can be unit-tested through an `Ops` trait:
   - Refuse an agent caller up front.
   - Refuse if this process hosts the port itself (the GUI in-process), or
     if the daemon serves a different db.
   - Request shutdown. A refusal is final and never escalates to a signal.
   - If the control socket is unreachable (a daemon that predates it), send
     SIGTERM by the info pid.
   - Wait up to 15s. If it hasn't stopped, give up with an error. Never
     SIGKILL.
   - Spawn the self-shim's target binary with the same `--port`/`--db`.
   - Poll `/api/server/info` until a new pid answers.
4. **Surfaces:**
   - **CLI:** `ninox server restart`, alias `ninox service restart`. Prints
     old → new and live sessions before → after.
   - **TUI:** Settings › Engine › Restart server, with a confirm and a footer
     status. Server alerts show in the footer.
   - **GUI:** a server row in the Settings Fleet card with an inline
     confirm. Also a top-bar server dot (tooltip, click opens the confirm) and
     an alert banner with a Restart server button.
5. **Surfacing, harness logins first.** `ninox_core::harness_auth` holds a
   per-harness pattern table. Claude Code's patterns are "Not logged in",
   "Please run /login", "Invalid API key", "OAuth token has expired" and
   `authentication_error`; another harness adds a row. Failures are
   recorded in the store's `harness_auth` table, because they're detected
   in several processes:
   - **Session panes:** the poller scans the bottom of each live pane every
     60s, and only trusts the harness's own error lines.
   - **`claude -p` subprocesses:** brain harvest (which now falls back to
     stdout when stderr is empty) and `ninox plan generate`.
   - **TUI spawn errors.**

   `/api/server/info` folds those rows into its issues. The UIs show
   "Claude login expired: log in again (`claude /login`), then Restart
   server", with Restart server and Restart affected sessions (the existing
   relaunch path) buttons. As secondary alerts, the poller's update-check
   and GitHub failures become AWS and GitHub hints, and a version/build
   mismatch with the client becomes "Restart server". Nothing runs a login.
6. **What fixes what after a harness login.**
   - Claude Code reads its stored credentials (macOS Keychain or
     `~/.claude/.credentials.json`) when it starts. New sessions and the
     daemon's `claude -p` subprocesses pick up a fresh login without a
     server restart.
   - Running sessions keep the credentials they started with, so they need
     Restart affected sessions.
   - A server restart is needed when the token was captured in the daemon's
     environment (`CLAUDE_CODE_OAUTH_TOKEN`/`ANTHROPIC_API_KEY`, passed
     through to harvest), for a new build, and for the AWS/gh cases.
   - The tmux server keeps its own global environment, which a server
     restart doesn't change.

## Not covered

- If the GUI hosts the server in-process, it can't restart its own process.
  The button explains this and asks the user to relaunch the app.
- The GUI always runs its own in-process poller. A daemon restart doesn't
  refresh that poller's environment.
