# Remote Sessions (SSH-Connected Machines)

**Status:** Design spec — not scheduled, no implementation. Captures a
proposed direction so an implementation plan can be cut from it later.

**Implementation target:** this repository (`ninox-core`, `ninox-app`,
`ninox-server`, `ninox-ptyd`). File and line references below reflect the
code as of this writing; treat them as orientation, not guaranteed
locations.

## Context

Every ninox session today lives on the one machine running the local
daemon. There is no concept anywhere in the code of "another machine's
sessions":

- `ninox-server` is an in-process Axum HTTP/WebSocket server
  (`crates/ninox-server/src/server.rs:13-38`) bound to `127.0.0.1` only. Its
  liveness check is a raw TCP connect to that port
  (`ninox-core/src/daemon.rs:23-32`) — there is no registry of other
  machines' daemons, nor a way to reach one.
- `ninox-ptyd` is the real, shipped (not aspirational — released since
  v0.28.1) PTY host described in
  `docs/superpowers/specs/2026-10-01-terminal-native-runtime-design.md`. It
  listens on a local unix-domain socket
  (`crates/ninox-ptyd/src/lib.rs:26-37`), gated by unix peer-credential
  uid matching (`host.rs:450-452`) — same-machine, same-user by
  construction. The default session backend today is still tmux
  (`ninox-core/src/runtime/mod.rs:46`, `-L ninox`,
  `ninox-core/src/tmux.rs:21-33`); ptyd is opt-in via `[runtime] backend =
  "ptyd"`. Both backends are exclusively local.
- The `Session` struct (`ninox-core/src/types.rs:80-182`) and its SQLite
  schema (`ninox-core/src/store.rs:16-29`) have no host/machine field at
  all — every row is implicitly "this machine." `pid` is a bare OS pid
  checked via `kill(pid, 0)` (`lifecycle/probe.rs:13-24`), and
  `workspace_path` is a bare local filesystem path used directly for `git
  diff` (`ninox-server/src/routes/sessions.rs:37-53`).
- `ninox list`, `ninox connect <id>`, and `ninox pane attach <id>`
  (`main.rs:148-183`, `243-247`, short-circuiting per CLAUDE.md before
  tmux-config/wrapper setup) all resolve against the process-local
  `Runtime::current()` singleton (`runtime/mod.rs:188-191`) and finish with
  a literal local `exec()` of `tmux` or the local `ninox` binary
  (`ninox-app/src/connect.rs:61-74`). None of this plumbing has a place to
  carry "which machine."

[Herdr](https://herdr.dev) (an unrelated third-party terminal/agent session
manager — public docs read for research only, not installed or run) solves
the equivalent problem for its own tmux-like runtime:

- `herdr machine add <host>` builds entirely on the user's existing SSH
  setup. Herdr "does not store passwords, private keys, agent tickets, or
  SSH control sockets in the catalog" — authentication stays with OpenSSH.
  Saved profiles hold only an opaque ID, a label, the SSH target, an
  explicit remote session name, and an enabled flag.
- Adding a machine auto-discovers already-running remote sessions (or
  offers a `default` one), and installs/updates the remote binary only
  with explicit approval ("the default answer is No"); background
  reconnects only ever discover, never auto-install.
- Remote sessions surface in the same sidebar as local ones, labeled by
  hostname or a custom `--label`.
- Reconnection after network interruption, sleep, or SSH failure is
  automatic, backed off, capped around two minutes, and driven by health
  probes — and a reconnect never steals the user's active selection away
  from the machine they're using.
- Agent forwarding is the user's own `ForwardAgent yes` in their SSH
  config; herdr does not enable it for them.

This document proposes the ninox equivalent: `ninox machine add <host>`,
making a remote machine's `ninox-server`/`ninox-ptyd` sessions appear in
the local `ninox list`, the desktop sidebar, and the TUI, with the same
"zero credentials of our own" posture.

## Goals

1. Let a user attach a remote machine's ninox sessions to their local
   `ninox list` / sidebar / TUI, driven entirely by their existing SSH
   access — no ninox-managed credentials, keys, or passwords anywhere.
2. Auto-discover already-running sessions on a newly-added machine, with
   an explicit, user-approved path to install/update the remote ninox
   binary when it's missing or incompatible. Background reconnects only
   discover; they never install.
3. Reconnect automatically after network blips, sleep, or SSH failure,
   with backoff and health probing, and without ever changing which
   session the user currently has selected/attached.
4. Keep the saved machine profile to opaque connection metadata only (id,
   label, SSH target, remote session name, enabled flag) — no session
   content, no secrets.
5. Make it explicit, in this document, everywhere ninox's current code
   hard-assumes a single local machine, rather than silently working
   around it.

## Non-goals

- Implementing any of this (spec only).
- Any ninox-native authentication, credential storage, or secrets
  management — SSH is the only auth layer, by design, mirroring Herdr.
- Any other Herdr feature unrelated to remote machines/sessions (its
  plugin system, its own agent-skill system, etc.).
- Cross-machine process migration/"moving" a live session from one host to
  another — out of scope; a remote session always runs, and is owned by,
  the machine it was spawned on.

## Proposed design

### 1. Security model: SSH is the only authentication layer

Ninox implements no auth of its own for remote machines, matching Herdr
exactly:

- `ninox machine add <host>` shells out to the user's own `ssh`/OpenSSH
  stack — whatever `~/.ssh/config`, agent, keys, and `known_hosts` the user
  already has. No ninox-managed key material, no stored passwords, no
  cached SSH control sockets persisted by ninox itself.
- The saved machine profile (see §3) is pure connection metadata: an
  opaque id, a label, the SSH target string (`user@host` or a
  `~/.ssh/config` alias), the remote session name, and an enabled flag.
  Nothing else is stored.
- Agent forwarding (needed for git auth / signing inside a remote pane) is
  opt-in entirely via the user's own `ForwardAgent yes` for that host.
  Ninox never sets or overrides this.

This satisfies the organization's "no plaintext secrets committed/stored"
constraint by construction — there is nothing for ninox to store that
could leak.

### 2. Transport: reaching a remote `ninox-server`/`ninox-ptyd` over SSH

Both existing backends are reachable only locally today:
`ninox-server` binds `127.0.0.1` (`server.rs:19`), and `ninox-ptyd` listens
on a local unix-domain socket gated by peer-credential uid matching
(`host.rs:450-452`). Neither should change its bind/ACL model — instead, a
remote machine is reached by tunneling through the SSH connection the user
already has:

- **HTTP/WS API**: an SSH local port forward (`ssh -L
  <local-port>:127.0.0.1:<remote-port> <host>`) exposes the remote
  machine's `ninox-server` REST/WebSocket API on a local ephemeral port,
  as if it were local. This requires no change to `ninox-server` itself.
- **ptyd unix socket**: OpenSSH's unix-domain-socket forwarding
  (`ssh -L <local-socket-path>:<remote-socket-path> <host>`, supported
  since OpenSSH 6.7) does the same for `ninox-ptyd`'s socket, preserving
  its existing same-uid peer-credential check — the peer on the remote
  end is the forwarded connection, authenticated as the SSH-connecting
  user.
- One SSH connection per machine profile, held open with
  `ControlMaster`/`ControlPersist` (mirroring Herdr's approach of sharing
  one connection across discovery, sidebar, and attach), carrying both
  forwards plus a healthcheck channel (§5).

This keeps "ninox implements no auth" literally true: the tunnel is a
transport detail, entirely delegated to OpenSSH.

### 3. Machine profiles: opaque config, not session state

A new `AppConfig` section follows the existing opt-in-feature shape
(`InboxMessagingConfig`/`PrWatchConfig`, `ninox-core/src/config.rs:244-272`),
placed before the `harnesses` field per the scalars-before-tables
constraint:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteMachinesConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub machines: Vec<MachineProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineProfile {
    pub id: String,              // opaque, generated at `machine add` time
    pub label: String,           // hostname by default, or user's --label
    pub ssh_target: String,      // "user@host" or an ssh-config alias
    pub remote_session: String,  // explicit remote session/orchestrator name
    pub enabled: bool,
}
```

This lives in config (not the SQLite store) because it's user-authored
connection identity, exactly like the existing `harnesses` map — not
session/runtime state. Session *rows* discovered on a remote machine still
live in the local store (§4), tagged with the profile's `id`.

### 4. Session representation: a `machine_id` field, nothing else new

`Session` (`ninox-core/src/types.rs`) gains one new optional field:

```rust
pub machine_id: Option<String>,  // None = local; Some(id) = MachineProfile.id
```

with a matching nullable SQLite column, added the same way the schema's
other `#[serde(default)]` fields were added incrementally. `None` is the
overwhelmingly common case and changes nothing for local sessions.

The `Poller` (`ninox-core/src/lifecycle/poller.rs`) gains a remote
reconciliation path per enabled, connected machine profile: instead of
polling the local tmux/ptyd backend, it polls the *remote* machine's
`ninox-server` `/api/v1/sessions` over the tunnel (§2) and
read→apply→writes those rows into the local store exactly as
`update_live_session_row` does today, tagged with `machine_id`. No new
store-write pattern is needed — this reuses the existing one.

### 5. `ninox machine add <host>`: discovery and install flow

Mirrors Herdr's flow precisely:

1. Connect over SSH to `<host>` (prompting for `--label`,
   `--remote-session` overrides as Herdr does).
2. Probe for a compatible `ninox` binary and a running `ninox-server` on
   the remote end (e.g. `ssh <host> ninox --version` / port-probe
   equivalent to `daemon.rs:23-32`, run remotely).
3. If the remote binary is missing or incompatible: **prompt for explicit
   approval before installing or replacing anything**, default **No**
   (matching Herdr's "setup asks before stopping it... default answer is
   No"). Background reconnects (§6) never take this branch — only
   interactive `machine add`/`machine update` do.
4. List already-running remote sessions (via the remote `ninox list
   --json`, reached over the same tunnel used for ongoing polling); if
   several exist, prompt the user to choose one (or several?) to track;
   if none exist, offer to spawn a `default` orchestrator remotely.
5. Persist the resulting `MachineProfile` to config (§3); the next poll
   tick picks up its sessions via §4.

### 6. Reconnection: backoff, health probes, never stealing selection

- A background health probe per enabled machine profile (lightweight,
  e.g. a periodic no-op over the held SSH control connection, or a
  port-probe through the forward) detects network interruption, sleep, or
  SSH failure.
- On failure, reconnection retries with exponential backoff capped around
  two minutes, mirroring Herdr. A reconnect **only discovers** — it never
  installs or replaces the remote binary; that always requires the
  explicit, interactive `machine add`/`machine update` path (§5.3).
- Reconnection updates store rows for that `machine_id` (marking sessions
  unreachable/reachable) but never touches local UI selection state.
  Selection already lives client-side (sidebar/TUI selection state, not
  the store), so this is naturally satisfied as long as reconnection logic
  only writes session rows and never reaches into UI state — worth an
  explicit invariant/test when implemented.

### 7. CLI, capability registration, and UI surfacing

- New CLI subcommands: `ninox machine add <host>`, `ninox machine list`,
  `ninox machine remove <id>`, following the existing `Connect`/`Pane`/
  `List` pattern of short-circuiting early in `main.rs` since they're
  agent/user-facing and don't need the tmux-config/wrapper/self-shim setup.
- One new `Capability` entry in `ninox-core/src/capabilities.rs`'s
  `REGISTRY`, gated by `enabled: |cfg| cfg.remote_machines.enabled`
  exactly like the existing `watch-pr` entry's `cfg.pr_watch.enabled` gate,
  with real markdown under `crates/ninox-core/skills/orchestrator/` (and/or
  `worker/` if workers should be able to request a machine be added).
- Desktop sidebar (`ninox-app/src/components/sidebar.rs`) and TUI
  (`ninox-app/src/tui/`) both already have an established per-row
  "metadata slot" (the `retention_badge` pattern at `sidebar.rs:322-335`;
  the TUI's `Row`/group-header scheme in `tui/state.rs`). A machine
  label/badge is a small addition to that existing slot once `Session`
  carries `machine_id` — grouping "by machine" is an analogous partition
  to the existing orchestrator/standalone grouping
  (`sidebar.rs:202-236`), not a new UI primitive.

## Open questions — assumptions this breaks, not hand-waved

These are current hard local-machine assumptions in the codebase that a
remote-sessions implementation must resolve; this document intentionally
does not resolve them, since doing so is implementation, not spec:

1. **`exec()`-based attach.** `ninox connect`/`ninox pane attach` end with
   a local `exec()` of `tmux` or the local `ninox` binary
   (`connect.rs:61-74`, `runtime_cli.rs:90-106`). For a remote session this
   has to become "SSH into the host and exec there" instead — changing the
   attach path from a local `execve` replace to a long-lived SSH child
   process. What does Ctrl-C, resize propagation (SIGWINCH), and detach
   look like through that extra hop?
2. **`Runtime::current()` is a local singleton** wrapping the local
   backend with no host parameter anywhere in `SessionBackend`'s trait
   methods (`runtime/mod.rs:105-174`, `188-191`). Does a remote machine get
   a third `SessionBackend` impl (`RemoteBackend`) that proxies over the
   tunnel, or does it bypass `SessionBackend` entirely and talk to the
   remote host's own `ninox-server` API? The two approaches have different
   implications for code reuse vs. architectural cleanliness.
3. **PID-based liveness.** `is_pid_alive` (`probe.rs:13-24`) calls `kill(pid,
   0)` directly — meaningless for a pid on another machine. Remote
   liveness has to come from the remote machine's own poller/health
   probe, not a local syscall.
4. **`workspace_path` is a bare local path**, used directly for `git diff`
   (`sessions.rs:37-53`) and report rendering. A remote session's
   workspace lives on the remote filesystem; today's code would silently
   produce an empty diff rather than erroring. Does this route through
   `ssh <host> git diff` instead, and does that imply git must also be
   installed/configured on the remote machine?
5. **Live-upgrade "handoff" is `SCM_RIGHTS` fd-passing**
   (`ninox-ptyd/src/handoff.rs`), which only works over a local unix
   socket — it cannot cross the SSH tunnel. Engine/ptyd upgrades on a
   remote machine must be a self-contained operation on that machine, with
   the local ninox only ever reconnecting its client afterward; this
   document doesn't attempt to specify that remote upgrade flow.
6. **tmux's `-L ninox` socket name is local-server-scoped**
   (`tmux.rs:21-33`). If a remote machine's default backend is still tmux,
   does "remote sessions" only work with the ptyd backend remotely, or
   does tmux-over-SSH also need support? Narrowing remote support to
   ptyd-only machines would simplify this considerably but needs an
   explicit decision.
7. **Binary distribution across architectures.** Ninox currently ships
   prebuilt Apple-silicon binaries plus a CodeArtifact cargo registry
   (recent `ninox update` work). The install-with-approval flow (§5.3)
   needs a real answer for a remote Linux/x86 host — is there a prebuilt
   artifact to fetch, or does install fall back to `cargo install` on the
   remote machine (requiring a remote Rust toolchain)?
8. **Socket-forwarding portability.** OpenSSH's unix-domain-socket
   `-L`/`-R` forwarding needs verification across the OS/OpenSSH-version
   matrix ninox users actually have (older OpenSSH, Windows SSH clients)
   before being load-bearing for the ptyd transport.
9. **Multiple concurrent remote sessions per machine.** Herdr multiplexes
   one SSH connection across all activity on a host via
   `ControlMaster`/`ControlPersist`. Does ninox's tunnel (§2) need to
   multiplex multiple forwarded ports/sockets (one HTTP/WS forward plus one
   ptyd-socket forward, potentially one per concurrently-attached pane), and
   what's the connection-count ceiling before this needs its own pooling?
10. **Where `machine_id` flows through the brain/cost/PR subsystems.**
    Session-adjacent features (brain harvesting, cost accounting, PR
    watch) all implicitly assume a local workspace and local git today;
    this document flags that they'll need the same remote-awareness as
    §4's diff example, but doesn't enumerate each one.
