# Remote ptyd/Engine Live Upgrade (MLOPS-5318) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After `ninox machine update`/`ninox machine add` installs a new binary on a remote machine that was already running an older one, bring the remote's long-running `ninox-ptyd` and headless-engine (`ninox-server`) daemons onto the new build without killing any live pane — by triggering the existing local SCM_RIGHTS live-upgrade handoff (`ninox ptyd --takeover`) over the already-open SSH control connection, the same way a local upgrade is triggered, and never performing the fd handoff itself locally.

**Architecture:** `ninox ptyd --takeover` (`ninox-ui-shared/src/runtime_cli.rs:47-57` → `ninox_ptyd::run_host_takeover` → `ninox-ptyd/src/handoff.rs`) is already the complete, tested local trigger for a live ptyd upgrade — a new process connects to the existing ptyd socket, and the old host hands off every pane's fd via `SCM_RIGHTS`, preserving the host's `epoch_ms` across the swap. Reusing it remotely only needs: (1) a way to run it *detached* over SSH, since it's a long-running foreground command and the existing `remote::run_remote_shell`/`run_remote_ninox` block until the remote command exits; (2) a way to tell when the handoff finished, by polling `ninox ptyd status` (already prints `pid`/`epoch_ms`) and comparing against the epoch observed before triggering; (3) a plain restart of the headless engine service, which holds no pane state of its own (`ninox-ptyd/src/lib.rs`'s module doc: "Keeping this crate free of engine logic is what lets the engine be upgraded or crash without touching agent processes") — `fleet::service` currently has no restart verb, only install/uninstall/status. No new local reconnect logic is needed: `RemoteBackend`'s SSH tunnel is re-established lazily per call (`remote_backend.rs:79-94`) and `Poller::poll_remote_machine_health`/`poll_remote_sessions` already retry every 5s with backoff (`poller.rs`, `lifecycle/remote_health.rs`) — once the remote daemons answer again, the existing polling picks them up with no new "reconnect" code.

**Tech Stack:** Rust (`ninox-core`, `ninox-ui-shared`), existing `anyhow`/`ensure!` error style, `cargo test -p ninox-core` / `-p ninox-ui-shared`.

**Spec:** `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` (§5, and resolved decision #5)

## Global Constraints

- The local ninox never participates in the SCM_RIGHTS fd handoff itself — it only triggers the remote `ninox ptyd --takeover` over SSH and polls for readiness (resolved decision #5).
- Live sessions/panes must survive the upgrade; detecting that they did *not* (a fresh ptyd epoch instead of a preserved one) must be a clear, named error, never silently reported as success.
- No ninox-managed credentials/secrets anywhere — SSH is the only auth layer. This plan adds no new storage of any kind.
- ptyd-only, macOS/Linux hosts only for v1 (spec resolved decisions #6, #8) — an unsupported remote OS for the engine-service restart must be a clear, named error.
- Do not run `cargo fmt` on the whole tree. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` must pass before opening the PR.
- Conventional commits, no co-authors in the commit body beyond what the harness appends automatically.
- Branch based on `origin/v1`, PR targeted at `v1`, ticket id `MLOPS-5318` in the branch name and in the PR title exactly as `MLOPS-5318: type(scope): description`.

## Review Focus

- A takeover that results in a *different* `epoch_ms` than before (a fresh start, not a true handoff) must surface as an explicit, named error — it means existing panes were lost, which is exactly the failure mode this ticket exists to prevent.
- Triggering `ninox ptyd --takeover` remotely must return to the caller promptly even though the remote process runs in the foreground forever by design — a naive `ssh host 'ninox ptyd --takeover'` would hang the CLI indefinitely.
- A remote machine with no previous `ninox-ptyd` running at all (binary replaced, daemon never started) must not be treated as a failure — there is nothing to preserve, so any running status is success.
- A failure to restart the headless engine service (unknown remote OS, `systemctl`/`launchctl` error) must not abort `machine update`/`machine add` outright — the pane-preserving ptyd handoff is the correctness-critical half of this ticket and should not be undone by an orthogonal engine-restart hiccup, so it is reported as a warning, matching the existing `restart_remote_sessions` best-effort style.
- The readiness poll must not falsely report success while the *old* host is still the one answering (same pid as before the trigger) — only a changed pid (plus a matching epoch, or no previous status at all) counts as the new process having taken over.

---

### Task 1: Headless-engine service restart commands

**Files:**
- Modify: `crates/ninox-core/src/fleet/service.rs`

**Interfaces:**
- Produces: `impl ServicePlatform { pub fn for_os(os: &str) -> Option<Self> }` — `"macos" -> Launchd`, `"linux" -> Systemd`, anything else `None`. `os` uses the same normalized vocabulary `remote::local_platform()`/`remote::detect_remote_platform` already produce (`std::env::consts::OS`-style: `"macos"`, `"linux"`).
- Produces: `pub fn restart_commands(platform: ServicePlatform, uid: u32) -> Vec<Vec<String>>` — mirrors `install_commands`/`uninstall_commands`'s shape (`service.rs:124-150`). Launchd: `[["launchctl", "kickstart", "-k", "gui/{uid}/io.ninox.engine"]]` (`-k` kills and restarts an already-running instance in one atomic command, unlike `install_commands`' bootout+bootstrap pair). Systemd: `[["systemctl", "--user", "restart", "ninox-engine.service"]]` (plain `enable --now` does *not* restart an already-active unit, which is why `install_commands` can't be reused here).
- Consumed by: Task 3's `restart_remote_engine_service`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn for_os_maps_macos_and_linux_only() {
    assert_eq!(ServicePlatform::for_os("macos"), Some(ServicePlatform::Launchd));
    assert_eq!(ServicePlatform::for_os("linux"), Some(ServicePlatform::Systemd));
    assert_eq!(ServicePlatform::for_os("windows"), None);
}

#[test]
fn restart_commands_kickstart_on_launchd() {
    let cmds = restart_commands(ServicePlatform::Launchd, 501);
    assert_eq!(cmds, vec![vec![
        "launchctl".to_string(), "kickstart".to_string(), "-k".to_string(), "gui/501/io.ninox.engine".to_string(),
    ]]);
}

#[test]
fn restart_commands_restarts_the_unit_on_systemd() {
    let cmds = restart_commands(ServicePlatform::Systemd, 501);
    assert_eq!(cmds, vec![vec![
        "systemctl".to_string(), "--user".to_string(), "restart".to_string(), "ninox-engine.service".to_string(),
    ]]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-core for_os_maps restart_commands`
Expected: FAIL to compile (`for_os`, `restart_commands` not found)

- [ ] **Step 3: Implement `ServicePlatform::for_os` and `restart_commands`**

Place `restart_commands` next to `install_commands`/`uninstall_commands` (`service.rs:124-150`), using the existing `LAUNCHD_LABEL`/`SYSTEMD_UNIT_NAME` constants exactly as those two do. Add `for_os` as an inherent method on `ServicePlatform`, next to the existing `current()` (`service.rs:17-26`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-core for_os_maps restart_commands`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/fleet/service.rs
git commit -m "feat(remote): add a headless-engine service restart command"
```

---

### Task 2: Remote ptyd status probing and a detached remote-command primitive

**Files:**
- Modify: `crates/ninox-core/src/remote.rs`

**Interfaces:**
- Produces: `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct PtydStatus { pub pid: u32, pub epoch_ms: u64 }`.
- Produces: `fn parse_ptyd_status_output(stdout: &str) -> Option<PtydStatus>` — parses `runtime_cli.rs`'s exact `"ptyd running: pid {pid}, epoch {epoch_ms}, {live} live / {n} panes, socket {path}"` format (`runtime_cli.rs:63-67`); `None` for anything else (including the `"ptyd not running..."` line).
- Produces: `pub fn probe_remote_ptyd_status(target: &str, control_path: &Path) -> Result<Option<PtydStatus>>` — runs `run_remote_ninox(target, control_path, "ptyd status")`; `Ok(None)` (not an error) when the command exits non-zero (not running) or output doesn't parse, mirroring `probe_remote_version`'s `Ok(None)` convention (`remote.rs:392-402`).
- Produces: `fn detached_script(inner_cmd: &str) -> String` — `format!("nohup sh -c {} </dev/null >/dev/null 2>&1 &", shell_quote(inner_cmd))`, wrapping `inner_cmd` in its own subshell so `nohup` applies to the whole compound command, not just its first word.
- Produces: `fn run_remote_detached(target: &str, control_path: &Path, inner_cmd: &str) -> Result<()>` — passes `detached_script(inner_cmd)` to `run_remote_shell`, `ensure!`s the *ssh client's own* exit (launching the background job) succeeded.
- Produces: `pub fn trigger_remote_ptyd_takeover(target: &str, control_path: &Path) -> Result<()>` — `run_remote_detached(target, control_path, &format!("export PATH=\"$HOME/{REMOTE_INSTALL_DIR}:$PATH\"; ninox ptyd --takeover"))`.
- Consumed by: Task 3's `wait_for_ptyd_takeover` (probe) and `upgrade_remote_daemons` in Task 4 (trigger).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parse_ptyd_status_output_extracts_pid_and_epoch() {
    let out = "ptyd running: pid 4821, epoch 1732000000123, 2 live / 3 panes, socket /tmp/ptyd.sock\n";
    assert_eq!(parse_ptyd_status_output(out), Some(PtydStatus { pid: 4821, epoch_ms: 1732000000123 }));
}

#[test]
fn parse_ptyd_status_output_none_when_not_running() {
    assert_eq!(parse_ptyd_status_output("ptyd not running (socket /tmp/ptyd.sock)\n"), None);
    assert_eq!(parse_ptyd_status_output(""), None);
}

#[test]
fn detached_script_backgrounds_a_nohup_subshell() {
    let script = detached_script("echo hi");
    assert!(script.starts_with("nohup sh -c "), "{script}");
    assert!(script.contains(&shell_quote("echo hi")), "{script}");
    assert!(script.ends_with(" &"), "{script}");
}

/// The one genuinely risky property of this mechanism: the ssh call must
/// return long before a slow remote command finishes.
#[test]
fn run_remote_detached_returns_before_the_command_finishes() {
    if !local_sshd_reachable() {
        eprintln!("skipping: no reachable localhost sshd");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let control_path = dir.path().join("c.sock");
    ensure_control_master("localhost", &control_path).unwrap();
    let marker = dir.path().join("done");
    let start = std::time::Instant::now();
    run_remote_detached(
        "localhost", &control_path,
        &format!("sleep 1 && touch {}", shell_quote(marker.to_str().unwrap())),
    ).unwrap();
    assert!(start.elapsed() < Duration::from_millis(500), "must return well before the 1s sleep finishes");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(std::time::Instant::now() < deadline, "detached command never ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    close_control_master("localhost", &control_path);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-core parse_ptyd_status_output detached_script run_remote_detached`
Expected: FAIL to compile (new items not found)

- [ ] **Step 3: Implement the five items above**

`parse_ptyd_status_output` splits on `"pid "` and `"epoch "` and takes each following token up to the next comma, trimmed, parsed as `u32`/`u64` respectively (same `.split(...).nth(1)?.split(',').next()?.trim().parse().ok()?` shape `parse_version_output`/`parse_uname_output` already use, `remote.rs:388-389`, `remote.rs:441-449`). Place the new functions near `probe_remote_version` (`remote.rs:392-402`) and `run_remote_ninox`/`run_remote_shell` (`remote.rs:285-312`) respectively.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-core parse_ptyd_status_output detached_script run_remote_detached`
Expected: PASS (the `run_remote_detached` test skips gracefully without a reachable localhost sshd)

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/remote.rs
git commit -m "feat(remote): add a detached-command primitive and ptyd status probing"
```

---

### Task 3: Readiness classification and the headless-engine remote restart

**Files:**
- Modify: `crates/ninox-core/src/remote.rs`

**Interfaces:**
- Consumes: `PtydStatus`, `probe_remote_ptyd_status` (Task 2); `ServicePlatform::for_os`, `restart_commands` (Task 1); `detect_remote_platform` (pre-existing, `remote.rs:450`).
- Produces: `enum PtydReadiness { NotYet, LiveUpgraded, FreshStart, PanesLost }` (private) and `fn classify_ptyd_status(status: &PtydStatus, previous: &Option<PtydStatus>) -> PtydReadiness` — `previous: None` → `FreshStart`; `previous: Some(p)` with `status.pid == p.pid` → `NotYet`; `status.epoch_ms == p.epoch_ms` (and pid differs) → `LiveUpgraded`; otherwise → `PanesLost`.
- Produces: `pub const PTYD_TAKEOVER_TIMEOUT: Duration = Duration::from_secs(30)` and a private `PTYD_TAKEOVER_POLL_INTERVAL: Duration = Duration::from_millis(300)` — the handoff's own internal timeouts (`ACK_TIMEOUT`/`RELEASE_TIMEOUT` 30s each, `RENAME_TIMEOUT` 5s in `handoff.rs:55-60`) bound the slow case; 30s external wait is comfortably above the common near-instant case.
- Produces: `fn wait_for_ptyd_ready_with<F: FnMut() -> Result<Option<PtydStatus>>>(probe: F, previous: Option<PtydStatus>, timeout: Duration, poll_interval: Duration) -> Result<PtydStatus>` — the pure polling loop (injectable probe for tests); `PanesLost` is a hard `bail!` (never retried — the handoff already finished one way or the other), `NotYet` keeps polling until `timeout`.
- Produces: `pub fn wait_for_ptyd_takeover(target: &str, control_path: &Path, previous: Option<PtydStatus>, timeout: Duration) -> Result<PtydStatus>` — `wait_for_ptyd_ready_with(|| probe_remote_ptyd_status(target, control_path), previous, timeout, PTYD_TAKEOVER_POLL_INTERVAL)`, with added `.with_context` naming `target`.
- Produces: `pub fn remote_uid(target: &str, control_path: &Path) -> Result<u32>` — `run_remote_shell(target, control_path, "id -u")`, parses the trimmed stdout as `u32`.
- Produces: `fn require_service_platform(os: &str) -> Result<ServicePlatform>` — `ServicePlatform::for_os(os).ok_or_else(|| anyhow!("no service-restart support for remote OS {os}"))`, pulled out as its own pure function so the error wording is unit-testable without a real remote.
- Produces: `pub fn restart_remote_engine_service(target: &str, control_path: &Path) -> Result<()>` — `detect_remote_platform` → `require_service_platform` → `remote_uid` → runs each `restart_commands` argv over SSH (joined with `shell_quote`-per-arg, via `run_remote_shell`), `ensure!`ing each succeeds.
- Consumed by: Task 4's `upgrade_remote_daemons`.

- [ ] **Step 1: Write the failing tests**

```rust
fn status(pid: u32, epoch_ms: u64) -> PtydStatus { PtydStatus { pid, epoch_ms } }

#[test]
fn classify_ptyd_status_covers_every_case() {
    assert_eq!(classify_ptyd_status(&status(2, 100), &None), PtydReadiness::FreshStart);
    assert_eq!(classify_ptyd_status(&status(1, 100), &Some(status(1, 100))), PtydReadiness::NotYet);
    assert_eq!(classify_ptyd_status(&status(2, 100), &Some(status(1, 100))), PtydReadiness::LiveUpgraded);
    assert_eq!(classify_ptyd_status(&status(2, 999), &Some(status(1, 100))), PtydReadiness::PanesLost);
}

#[test]
fn wait_for_ptyd_ready_with_retries_until_the_pid_changes() {
    use std::cell::RefCell;
    let calls = RefCell::new(vec![Ok(Some(status(1, 100))), Ok(Some(status(1, 100))), Ok(Some(status(2, 100)))].into_iter());
    let got = wait_for_ptyd_ready_with(
        || calls.borrow_mut().next().unwrap(),
        Some(status(1, 100)),
        Duration::from_secs(1), Duration::from_millis(1),
    ).unwrap();
    assert_eq!(got, status(2, 100));
}

#[test]
fn wait_for_ptyd_ready_with_errors_clearly_on_panes_lost() {
    let err = wait_for_ptyd_ready_with(
        || Ok(Some(status(2, 999))),
        Some(status(1, 100)),
        Duration::from_secs(1), Duration::from_millis(1),
    ).unwrap_err();
    assert!(err.to_string().contains("100") && err.to_string().contains("999"), "{err}");
}

#[test]
fn wait_for_ptyd_ready_with_times_out_when_never_ready() {
    let err = wait_for_ptyd_ready_with(
        || Ok(Some(status(1, 100))),
        Some(status(1, 100)),
        Duration::from_millis(20), Duration::from_millis(5),
    ).unwrap_err();
    assert!(err.to_string().contains("did not come back up"), "{err}");
}

#[test]
fn require_service_platform_errors_clearly_for_an_unsupported_os() {
    assert_eq!(require_service_platform("macos").unwrap(), ServicePlatform::Launchd);
    let err = require_service_platform("windows").unwrap_err();
    assert!(err.to_string().contains("windows"), "{err}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-core classify_ptyd_status wait_for_ptyd_ready_with`
Expected: FAIL to compile (new items not found)

- [ ] **Step 3: Implement the items above**

`wait_for_ptyd_ready_with`'s loop: call `probe()`; on `Ok(Some(status))`, match `classify_ptyd_status`: `LiveUpgraded | FreshStart` → `return Ok(status)`; `PanesLost` → `bail!` immediately (naming both epoch values); `NotYet` → fall through. On any other probe result (`Ok(None)` or `Err`), fall through too (a transient probe hiccup shouldn't abort early). After each iteration, check the deadline before sleeping `poll_interval`. `restart_remote_engine_service` has no interesting branching beyond what's already specified in Interfaces — implement it as described there, placing it near `probe_remote_version`/the other remote-probing functions.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-core classify_ptyd_status wait_for_ptyd_ready_with require_service_platform`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/remote.rs
git commit -m "feat(remote): add ptyd takeover readiness detection and engine-service restart"
```

---

### Task 4: Wire the live upgrade into `ninox machine update`/`machine add`

**Files:**
- Modify: `crates/ninox-ui-shared/src/machine.rs`
- Modify: `crates/ninox-core/skills/orchestrator/remote-machines.md`

**Interfaces:**
- Consumes: `remote::probe_remote_ptyd_status`, `remote::trigger_remote_ptyd_takeover`, `remote::wait_for_ptyd_takeover`, `remote::PTYD_TAKEOVER_TIMEOUT`, `remote::restart_remote_engine_service` (Tasks 2-3).
- Produces: `fn engine_restart_warning(host: &str, reason: &str) -> String` — same shape as the existing `restart_warning` (`machine.rs:172-174`), worded for the engine-service restart specifically.
- Produces: `fn upgrade_remote_daemons(target: &str, control_path: &Path) -> Result<()>` — replaces the body of both existing `if ... remote_version.is_some() { ... restart_remote_sessions(...) }` blocks (`run_update` at `machine.rs:124-127`, `run_add` at `machine.rs:268-271`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn engine_restart_warning_names_the_host_and_reason() {
    let msg = engine_restart_warning("box.example", "unit not found");
    assert!(msg.contains("box.example"), "{msg}");
    assert!(msg.contains("unit not found"), "{msg}");
    assert_ne!(msg, restart_warning("box.example", "unit not found"), "distinct wording from the session-restart warning");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ninox-ui-shared engine_restart_warning`
Expected: FAIL to compile (`engine_restart_warning` not found)

- [ ] **Step 3: Implement `engine_restart_warning` and `upgrade_remote_daemons`**

```rust
fn upgrade_remote_daemons(target: &str, control_path: &Path) -> Result<()> {
    println!("Live-upgrading ptyd on {target}...");
    let previous = remote::probe_remote_ptyd_status(target, control_path).unwrap_or(None);
    remote::trigger_remote_ptyd_takeover(target, control_path)?;
    remote::wait_for_ptyd_takeover(target, control_path, previous, remote::PTYD_TAKEOVER_TIMEOUT)?;
    println!("ptyd on {target} is live-upgraded.");

    match remote::restart_remote_engine_service(target, control_path) {
        Ok(()) => println!("Restarted the ninox engine service on {target}."),
        Err(e) => println!("{}", engine_restart_warning(target, &e.to_string())),
    }

    println!("Restarting live sessions on {target} to pick up the new build...");
    restart_remote_sessions(target, control_path);
    Ok(())
}
```

Replace the `run_update` block (`machine.rs:124-127`) with `upgrade_remote_daemons(&host, &control_path)?;` and the `run_add` block (`machine.rs:268-271`) with the same call, keeping each inside its existing `if remote_version.is_some()` (`run_update`) / `if install_needed && remote_version.is_some()` (`run_add`) guard unchanged — a from-scratch install still has nothing running to upgrade.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ninox-ui-shared engine_restart_warning`
Expected: PASS

- [ ] **Step 5: Document the behavior change**

Update `crates/ninox-core/skills/orchestrator/remote-machines.md`'s `machine update` section (added by MLOPS-5314/#239) to say it now live-upgrades `ninox-ptyd` (panes survive) and restarts the headless engine service, instead of the old wording that explicitly disclaimed live-reloading them.

- [ ] **Step 6: Full verification**

Run: `cargo fmt -p ninox-ui-shared -p ninox-core -- --check` (new code only — do not reformat whole files on a pre-existing diff), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
Expected: all PASS

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-ui-shared/src/machine.rs crates/ninox-core/skills/orchestrator/remote-machines.md
git commit -m "feat(remote): live-upgrade ptyd and restart the engine service in machine update/add"
```
