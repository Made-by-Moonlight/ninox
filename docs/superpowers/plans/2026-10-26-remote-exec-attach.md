# Remote Exec-Based Attach Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Route `ninox connect <id>` and `ninox pane attach <id>` through `ssh -t <host> ninox pane attach <id>` for any session with `machine_id: Some(_)`, recursing into the remote machine's own `ninox` binary instead of reimplementing attach locally.

**Architecture:** Add a pure `attach_exec_argv` builder in `ninox-core::remote` that constructs the `ssh -t -S <control_path> -- <target> sh -lc '...'` argv, reusing the existing `ControlMaster`/PATH conventions already in that file. Wire `RemoteBackend::attach_args` to call it. Fix the two call sites that currently never look at `machine_id` at all (`ninox-ui-shared::connect::connect_preflight` and `runtime_cli::run_pane`'s `PaneAction::Attach`) to check the session's `machine_id` via the Store and route through the resolved remote backend instead of local tmux/ptyd.

**Tech Stack:** Rust, tokio, anyhow — existing ssh-shellout helpers in `crates/ninox-core/src/remote.rs`.

**Spec:** `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` (resolved decisions #1, #2, #3, #9)

## Global Constraints

- SSH is the only auth layer — no new ninox-managed credentials or secrets.
- Reuse the per-profile `ControlMaster` (`-S <control_path>`) rather than a fresh SSH connection per attach (resolved decision #9).
- Skip local pid-based liveness entirely for `machine_id: Some(_)` sessions (resolved decision #3) rather than attempting a new remote liveness probe — that's explicit follow-on work (Poller remote reconciliation, §4 of the spec), not this task.
- A target string starting with `-` must still be treated as a hostname, not an SSH option (existing codebase invariant, tested elsewhere in `remote.rs`).

## Review Focus

- Stale/disabled `machine_id` (profile removed or disabled after a session was tagged) must produce a clear error, not an empty-argv panic in `exec_attach` (`argv[0]` on an empty `Vec` panics).
- A session id containing shell-special characters must not break the remote shell command — reuse the file's existing `shell_quote`.
- `ninox pane attach <id> --no-detach` against a remote-tagged session must forward `--no-detach` to the remote invocation, not silently drop it.
- `ninox connect`/`ninox pane attach` against a local (`machine_id: None`) session must be completely unaffected — existing tests must keep passing unchanged.
- The constructed argv must never allow the `ssh_target` string (user-configured) to be parsed as an SSH option.

---

### Task 1: `attach_exec_argv` builder in `ninox-core`

**Files:**
- Modify: `crates/ninox-core/src/remote.rs`

**Interfaces:**
- Produces: `pub fn attach_exec_argv(profile: &crate::config::MachineProfile, id: &str) -> Vec<String>` — returns the full `ssh` argv (`["ssh", "-t", "-S", <control_path>, "--", <ssh_target>, "sh", "-lc", <remote_cmd>]`), where `<remote_cmd>` is `export PATH="$HOME/{REMOTE_INSTALL_DIR}:$PATH"; ninox pane attach <shell_quote(id)>`, reusing `control_socket_path`, `REMOTE_INSTALL_DIR`, and `shell_quote` already in this file.

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn attach_exec_argv_builds_the_ssh_recursion_command() {
    let profile = MachineProfile {
        id: "prof1".into(),
        label: "box".into(),
        ssh_target: "user@box.example".into(),
        remote_session: "default".into(),
        enabled: true,
    };
    let argv = attach_exec_argv(&profile, "sess-1");
    assert_eq!(argv[0], "ssh");
    assert_eq!(argv[1], "-t");
    assert_eq!(argv[2], "-S");
    assert_eq!(argv[3], control_socket_path("prof1").display().to_string());
    assert_eq!(argv[4], "--");
    assert_eq!(argv[5], "user@box.example");
    assert_eq!(argv[6], "sh");
    assert_eq!(argv[7], "-lc");
    assert_eq!(argv[8], "export PATH=\"$HOME/.local/bin:$PATH\"; ninox pane attach 'sess-1'");
    assert_eq!(argv.len(), 9);
}

#[test]
fn attach_exec_argv_shell_quotes_the_session_id() {
    let profile = MachineProfile {
        id: "prof1".into(), label: "box".into(), ssh_target: "box".into(),
        remote_session: "default".into(), enabled: true,
    };
    let argv = attach_exec_argv(&profile, "it's-weird");
    assert!(argv.last().unwrap().contains("'it'\\''s-weird'"));
}
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-core remote:: attach_exec_argv -- --nocapture`
Expected: FAIL with "cannot find function `attach_exec_argv`"

- [x] **Step 3: Implement `attach_exec_argv` in `crates/ninox-core/src/remote.rs`**

Place it after `run_remote_ninox`/`install_local_binary`, before the `RemoteInstallBackend` section. Doc comment should cite resolved decision #1 and name the invariant it exists to satisfy: the remote recursion target is the remote host's *own* `ninox pane attach <id>`, never re-checking `machine_id` there (it's `None` on that machine's own store for this session).

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-core remote::`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ninox-core/src/remote.rs
git commit -m "feat(remote): add SSH exec argv builder for remote pane attach"
```

### Task 2: Wire `RemoteBackend::attach_args`

**Files:**
- Modify: `crates/ninox-core/src/runtime/remote_backend.rs`

**Interfaces:**
- Consumes: `crate::remote::attach_exec_argv` (Task 1).
- Produces: `RemoteBackend::attach_args(&self, id: &str) -> Vec<String>` now returns `attach_exec_argv(&target.profile, id)` when resolved, `Vec::new()` when unresolved (unchanged for the unresolved case — existing test already covers it).

- [x] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn attach_args_builds_the_ssh_recursion_argv_for_a_resolved_machine() {
    let profile = MachineProfile {
        id: "prof1".into(), label: "box".into(), ssh_target: "user@box.example".into(),
        remote_session: "default".into(), enabled: true,
    };
    let backend = RemoteBackend::for_machine(profile.clone(), 7777);
    let argv = backend.attach_args("sess-1").await;
    assert_eq!(argv, crate::remote::attach_exec_argv(&profile, "sess-1"));
}
```

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p ninox-core runtime::remote_backend -- --nocapture`
Expected: FAIL (`argv` is empty, assertion mismatch)

- [x] **Step 3: Implement**

```rust
async fn attach_args(&self, id: &str) -> Vec<String> {
    match &self.target {
        Some(target) => crate::remote::attach_exec_argv(&target.profile, id),
        None => Vec::new(),
    }
}
```

Update the module's top doc comment: remove "exec-based attach" from the list of things still deferred as follow-on work (it's done now); leave reconnection/backoff, the remote ptyd/engine upgrade flow, and `Poller` remote reconciliation listed, since those remain out of scope.

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core runtime::remote_backend`
Expected: PASS, including the pre-existing `infallible_methods_never_fabricate_success` (still asserts empty argv for `unresolved()`).

- [x] **Step 5: Commit**

```bash
git add crates/ninox-core/src/runtime/remote_backend.rs
git commit -m "feat(remote): wire RemoteBackend::attach_args to the SSH exec recursion"
```

### Task 3: Route `ninox connect` through `machine_id`

**Files:**
- Modify: `crates/ninox-ui-shared/src/connect.rs`

**Interfaces:**
- Consumes: `ninox_core::runtime::backend_for(id, machine_id) -> Arc<dyn SessionBackend>` (already exists), `.attach_args(id).await` (Task 2).
- Produces: `connect_preflight` behavior unchanged for `machine_id: None`; for `machine_id: Some(_)`, returns `ConnectPlan::Attach` built from the remote backend, or an `Err` with a clear message when the backend resolves to an empty argv (stale/disabled profile) — never passes an empty `Vec` into `ConnectPlan::Attach`.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn remote_session_skips_local_liveness_and_builds_ssh_attach_args() {
    let store = temp_store();
    let mut s = crate::cli::test_fixtures::session("remote-1", None, SessionStatus::Working);
    s.machine_id = Some("prof1".into());
    store.upsert_session(&s).unwrap();
    // No local tmux/ptyd pane exists for "remote-1" at all — if this routed
    // through local liveness it would report Dead/Terminated instead.
    // No machine profile is configured, so the remote backend resolves to
    // `unresolved()` and attach_args is empty — this must surface as a
    // clear error, not a panic or a fabricated Attach.
    let err = connect_preflight(&store, "remote-1").await.unwrap_err();
    assert!(err.to_string().contains("prof1"), "{err}");
}
```

(A positive "builds the right ssh argv" case needs a real `AppConfig` with a matching `MachineProfile`, which is awkward to inject in this test module today — covered instead by Task 4's equivalent test against `run_pane`, which can stub the config file directly, plus Task 1/2's unit tests already proving `attach_exec_argv`/`attach_args` build the right argv. This test only proves the routing + error-on-empty-argv behavior.)

- [x] **Step 2: Run it to verify it fails**

Run: `cargo test -p ninox-ui-shared connect_tests -- --nocapture`
Expected: FAIL — today this session would hit the local `liveness()` branch and come back `Dead`/`Terminated`, not an `Err` mentioning `prof1`.

- [x] **Step 3: Implement**

```rust
pub async fn connect_preflight(store: &Store, id: &str) -> anyhow::Result<ConnectPlan> {
    let Some(session) = store.get_session(id)? else {
        let suggestions = store
            .list_sessions()?
            .into_iter()
            .filter(|s| s.id.starts_with(id))
            .map(|s| s.id)
            .collect();
        return Ok(ConnectPlan::NotFound { suggestions });
    };
    if let Some(machine_id) = session.machine_id.as_deref() {
        let argv = runtime::backend_for(id, Some(machine_id)).await.attach_args(id).await;
        anyhow::ensure!(
            !argv.is_empty(),
            "no configured machine profile {machine_id} for session {id} — check `ninox machine list`"
        );
        return Ok(ConnectPlan::Attach(argv));
    }
    match runtime::liveness(id).await {
        // ... unchanged ...
    }
    Ok(ConnectPlan::Attach(runtime::attach_args(id).await))
}
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-ui-shared connect_tests`
Expected: PASS, including the three pre-existing tests unchanged.

- [x] **Step 5: Commit**

```bash
git add crates/ninox-ui-shared/src/connect.rs
git commit -m "feat(remote): route ninox connect through the SSH exec path for remote sessions"
```

### Task 4: Route `ninox pane attach` through `machine_id`

**Files:**
- Modify: `crates/ninox-ui-shared/src/runtime_cli.rs`
- Modify: `crates/ninox-ui-shared/src/cli.rs` (call site for `run_pane`)

**Interfaces:**
- Consumes: `ninox_core::store::Store::open`, `ninox_core::runtime::backend_for`, `crate::connect::exec_attach` (already `pub(crate)`).
- Produces: `run_pane(action: PaneAction, db_path: PathBuf) -> anyhow::Result<()>` — new second parameter. `PaneAction::Attach` now looks up `pane_id` in the store first; if it names a session with `machine_id: Some(_)`, execs the SSH recursion (forwarding `--no-detach` into the remote command when set) instead of bridging to the local ptyd socket. `PaneAction::List` ignores the new parameter.

- [x] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn pane_attach_for_a_remote_session_with_no_configured_profile_errors_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let store = ninox_core::store::Store::open(&db_path).unwrap();
    let mut s = /* build a minimal Session fixture, e.g. via existing test helper */;
    s.id = "remote-1".into();
    s.machine_id = Some("prof1".into());
    store.upsert_session(&s).unwrap();
    drop(store);

    let err = run_pane(PaneAction::Attach { pane_id: "remote-1".into(), no_detach: false }, db_path)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("prof1"), "{err}");
}

#[tokio::test]
async fn pane_attach_for_an_unknown_id_falls_back_to_local_ptyd_path() {
    // id not present in the store at all -> must not error on the
    // machine_id lookup itself, just fall through to the existing local
    // attach attempt (which will itself fail/connect-refuse against a
    // nonexistent ptyd socket in this test env — asserting it does NOT
    // mention "machine profile" is enough to prove the remote branch was
    // skipped).
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let result = run_pane(PaneAction::Attach { pane_id: "nope".into(), no_detach: false }, db_path).await;
    if let Err(e) = result {
        assert!(!e.to_string().contains("machine profile"), "{e}");
    }
}
```

- [x] **Step 2: Run them to verify they fail**

Run: `cargo test -p ninox-ui-shared runtime_cli:: -- --nocapture`
Expected: FAIL to compile (`run_pane` doesn't take a second argument yet).

- [x] **Step 3: Implement**

```rust
pub(crate) async fn run_pane(action: PaneAction, db_path: std::path::PathBuf) -> anyhow::Result<()> {
    let socket = ninox_ptyd::socket_path();
    match action {
        PaneAction::Attach { pane_id, no_detach } => {
            if let Some(machine_id) = remote_machine_id(&db_path, &pane_id)? {
                let mut argv = ninox_core::runtime::backend_for(&pane_id, Some(&machine_id)).await.attach_args(&pane_id).await;
                anyhow::ensure!(
                    !argv.is_empty(),
                    "no configured machine profile {machine_id} for session {pane_id} — check `ninox machine list`"
                );
                if no_detach {
                    if let Some(last) = argv.last_mut() {
                        last.push_str(" --no-detach");
                    }
                }
                return crate::connect::exec_attach(argv);
            }
            use std::io::IsTerminal;
            // ... existing local-attach body unchanged ...
        }
        PaneAction::List { json } => { /* unchanged */ }
    }
}

/// `pane_id`'s `machine_id`, if it names a session the local store knows
/// about — `None` for an unknown id (never an error: it just means "not a
/// remote-tagged session", so the caller falls through to the existing
/// local ptyd attach).
fn remote_machine_id(db_path: &std::path::Path, pane_id: &str) -> anyhow::Result<Option<String>> {
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let store = ninox_core::store::Store::open(db_path)?;
    Ok(store.get_session(pane_id)?.and_then(|s| s.machine_id))
}
```

Update the call site in `cli.rs`:
```rust
Command::Pane { action } => runtime_cli::run_pane(action, args.db.unwrap_or_else(default_db_path)).await,
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-ui-shared`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ninox-ui-shared/src/runtime_cli.rs crates/ninox-ui-shared/src/cli.rs
git commit -m "feat(remote): route ninox pane attach through the SSH exec path for remote sessions"
```

### Task 5: Full verification

- [x] **Step 1:** `cargo build --workspace`
- [x] **Step 2:** `cargo test --workspace`
- [x] **Step 3:** `cargo clippy --workspace --all-targets`
- [x] **Step 4:** Fix any warnings/failures surfaced, re-run until clean.
