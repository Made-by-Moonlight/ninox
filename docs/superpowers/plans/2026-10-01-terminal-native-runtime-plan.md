# Terminal-Native Runtime — Implementation Plan

Spec: `docs/superpowers/specs/2026-10-01-terminal-native-runtime-design.md`.
Reference implementation studied: herdr (github.com/herdrdev/herdr).

## Scope decisions

- Phases 1–6 of the spec, plus the cutover *default switch* (`ptyd` becomes
  the default backend). **The Iced app is not deleted** and must keep
  working on both backends. `TmuxBackend` stays for live legacy sessions.
- **Bare `ninox` is all a user needs.** From a TTY it ensures ptyd and the
  engine daemon are running (spawning them detached if not) and opens the
  TUI. Without a TTY (macOS .app launch) it opens the Iced app as today.
  `ninox gui` forces the Iced app.
- Open questions resolved:
  - Rendering: the TUI composites pane screens client-side from ptyd
    `ScreenSnapshot`s pulled on `ScreenChanged` (server-owned terminal
    state, client-owned composition — herdr's newer "client-owned shell"
    model). `Raw` subscriptions cover bridges and remote viewing.
  - One binary: `ninox ptyd` (hidden) runs the host; the engine is
    `ninox --headless`.
  - Recovery is acknowledged with `ninox fleet ack`.

## Architecture

```
ninox ptyd            crates/ninox-ptyd   PTYs + alacritty_terminal per pane,
                                          Unix socket (protocol.rs), checkpoints
ninox --headless      ninox-core/app      engine; SessionBackend -> PtydBackend
ninox (TUI)           ninox-app/src/tui   fleet board + live panes; talks to the
                                          store/engine and to ptyd for pane I/O
ninox pane attach     ninox-ptyd/attach   raw bridge; what `attach_args` returns
                                          for ptyd sessions, so the Iced app and
                                          `ninox connect` work unchanged
```

## Contracts (shared by all workstreams)

- `crates/ninox-ptyd/src/{protocol,client,checkpoint,attach}.rs` define the
  API. Signatures are fixed; workstream A implements them. Additive changes
  only, and announce them.
- Socket: `ninox_ptyd::socket_path()` (`$NINOX_PTYD_SOCKET` override). Tests
  always use a tempdir socket and never touch the real one.
- Host argv: `[<ninox bin>, "ptyd"]`; log to `<data>/ninox/ptyd.log`.
- Pane id == ninox session id.
- Pane env: everything the tmux path sets today (`NINOX_SESSION`, …) plus
  `NINOX_PANE_ID=<id>`, `NINOX_PTYD_SOCKET=<path>`, `TERM=xterm-256color`,
  `COLORTERM=truecolor`; `env_remove` = `CLAUDECODE`,
  `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_SESSION_ID`,
  `CLAUDE_CODE_CHILD_SESSION`, `CLAUDE_CODE_MESSAGING_TOKEN`,
  `CODEX_THREAD_ID`, `TMUX`, `TMUX_PANE`. argv = `[$SHELL, "-l", "-c", cmd]`
  (same as tmux `new-session`).
- Backend selection: `[runtime] backend = "ptyd" | "tmux"` on `AppConfig`
  (default `ptyd`). New sessions use the configured backend. Operations on
  an existing session dispatch to ptyd if ptyd knows the pane, else tmux —
  no store column needed, and live tmux fleets keep working after upgrade.
- Prefix key (TUI and `pane attach` detach chord): `Ctrl-Space` (NUL byte),
  configurable via `[tui] prefix`. It must not be `Ctrl-b`/`Ctrl-a`, which
  collide with users' own tmux/screen. Detach = prefix, `d`.

## Workstreams (each on its own branch off `terminal-native-runtime`)

| | Branch | Owns | Spec |
|---|---|---|---|
| A | `tnr-ptyd` | `crates/ninox-ptyd/**` only | §1, §2, §5.5, fd handoff |
| B | `tnr-backend` | `tmux.rs` → `runtime/` seam, callers, identity, `ninox ptyd` / `ninox pane attach` CLI wiring, Iced attach/scrollback/web WS on ptyd, default switch | §3, phase 2, 7 (minus deletion) |
| C | `tnr-fleet` | store fields, `ninox-core/src/fleet/`, `ninox fleet …`, briefings, restore policy, autostart service, fleet capability skills | §5.1–5.4 |
| D | `tnr-tui` | `ninox-app/src/tui/**`, bare-`ninox` routing | §4, phases 4–5 |

Shared files (`main.rs`, `config.rs`, `lib.rs`, `Cargo.toml`) get minimal,
additive edits: add an enum variant / module line / config struct and put
the body in a new file, so merges stay mechanical.

## Verification

- `cargo test --workspace` and `cargo clippy --workspace --all-targets`
  (no new warnings) on every branch.
- ptyd: headless integration tests spawning real `sh`/`printf` processes on
  a tempdir socket; replay tests from recorded agent output
  (`crates/ninox-ptyd/tests/fixtures/`), including DEC 2026 synchronized
  output and alt-screen apps.
- End-to-end smoke script `scripts/tnr-smoke.sh` (isolated data dir + socket):
  start ptyd, spawn a session, `ninox read`, attach bridge, restart engine
  and confirm the pane survives, kill ptyd and run `ninox fleet restore
  --dry-run`.
