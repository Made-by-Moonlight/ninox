# Remote Upgrade Flow (`ninox machine update`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `ninox machine update <id>`, which detects version skew between the local `ninox` binary and an already-added remote machine's installed binary over the existing SSH `ControlMaster` tunnel, and — with explicit approval — upgrades the remote install to match.

**Architecture:** `ninox machine add` already has every low-level piece this needs: `remote::probe_remote_version` (skew detection) and `remote::install_remote_binary`/`RemoteInstallBackend` (cross-arch-aware install, the interface MLOPS-5315's distribution work plugs into — landed in `8d822cb`/#221). What's missing is the ability to run that same check-and-upgrade sequence against a machine profile *after* it's already been added, without re-running the whole interactive `add` flow. This plan extracts the install/verify/describe steps `run_add` already has into small shared helpers, then adds a new `MachineAction::Update` subcommand that reuses them against a looked-up profile. No new transport, no new install mechanism — this is new orchestration over existing primitives.

**Tech Stack:** Rust (`ninox-core`, `ninox-ui-shared`), `clap` derive for the CLI, existing `anyhow`/`ensure!` error style, `cargo test -p ninox-core` / `-p ninox-ui-shared`.

**Spec:** `docs/superpowers/specs/2026-10-05-remote-sessions-design.md` (§5, and resolved decisions #5 and #7)

## Global Constraints

- No ninox-managed credentials/secrets anywhere — SSH is the only auth layer (spec §1). This plan adds no new storage of any kind.
- Upgrading a remote binary always requires explicit, interactive approval with a default of **No** — never automatic, never from a background probe (spec §5.3, mirrored by naming `machine update` right alongside `machine add` in the already-shipped §6 doc comment in `remote-machines.md`).
- Don't rebuild cross-arch binary distribution — reuse `remote::RemoteInstallBackend` / `remote::install_remote_binary` exactly as `machine add` already does (this is the interface MLOPS-5315's distribution work plugs into).
- ptyd-only, macOS/Linux hosts only for v1 (spec resolved decisions #6, #8) — unchanged by this plan, already enforced inside `remote::install_remote_binary`.
- Do not run `cargo fmt` on the whole tree. `cargo clippy --workspace --all-targets` must be warning-free and `cargo test --workspace` must pass before opening the PR.
- Conventional commits, no co-authors in the commit body beyond what the harness appends automatically.

## Review Focus

- An unknown/typo'd profile id passed to `machine update` must fail with a clear "no machine profile with id X" error — never attempt an SSH connection with a garbage id.
- A remote already running the exact same version as local must print an "up to date" message and must not prompt for approval or touch the remote at all.
- Declining the approval prompt must leave the remote binary untouched and exit with a clear "aborted" error, not a partial install.
- A remote on a different OS/architecture than local must go through `install_remote_binary`'s prebuilt-fetch/cargo-install-fallback paths, never the same-arch binary-copy fast path — this is the exact MLOPS-5315 interface boundary the ticket calls out.
- If the remote reports an unexpected version right after install (install silently landed the wrong build), this must surface as a specific, named error — never be swallowed as success.

---

### Task 1: Extract shared install/verify/describe helpers from `run_add`

**Files:**
- Modify: `crates/ninox-ui-shared/src/machine.rs:109-167` (the `install_needed` block inside `run_add`)

**Interfaces:**
- Produces: `fn install_and_verify_binary(host: &str, control_path: &Path, local_version: &str) -> Result<remote::InstallMethod>` — resolves `canonical_exe()`/`local_platform()`, builds a `remote::SshInstallBackend`, calls `remote::install_remote_binary`, then re-probes and verifies the remote now reports exactly `local_version`.
- Produces: `fn describe_install_method(method: remote::InstallMethod, host: &str) -> String` — the three user-facing messages `run_add` currently prints inline for `CopyLocalBinary` / `FetchPrebuilt` / `CargoInstallFallback`.
- Produces: `fn version_mismatch_after_install(installed: Option<&str>) -> anyhow::Error` — the error `install_and_verify_binary` raises when the post-install probe doesn't match.
- Consumed by: Task 2's `run_update`.

This task is a pure refactor: `run_add`'s behavior and all its existing output must stay byte-for-byte identical. There is no spec-driven decision to make here beyond the signatures above, which this task fixes so Task 2 can reuse them.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn describe_install_method_names_the_host_for_each_method() {
    let host = "box.example";
    assert_eq!(
        describe_install_method(remote::InstallMethod::CopyLocalBinary, host),
        "Copied the local ninox binary to box.example:~/.local/bin/ninox."
    );
    assert!(describe_install_method(remote::InstallMethod::FetchPrebuilt, host).contains("installed a matching prebuilt binary"));
    assert!(describe_install_method(remote::InstallMethod::CargoInstallFallback, host).contains("cargo install"));
}

#[test]
fn version_mismatch_after_install_names_the_actual_version() {
    let err = version_mismatch_after_install(Some("0.31.0"));
    assert!(err.to_string().contains("0.31.0"), "{err}");
    let err = version_mismatch_after_install(None);
    assert!(err.to_string().contains("None"), "{err}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-ui-shared describe_install_method -- --include-ignored` and `cargo test -p ninox-ui-shared version_mismatch_after_install`
Expected: FAIL to compile (`describe_install_method`/`version_mismatch_after_install` not found)

- [ ] **Step 3: Implement the three functions and refactor `run_add` to call them**

`describe_install_method` and `version_mismatch_after_install` are pure string-formatting functions — copy the exact wording `run_add` already uses (see `machine.rs:151-166` for `describe_install_method`'s three messages, and `machine.rs:163-166`'s `ensure!` message for `version_mismatch_after_install`'s wording). `install_and_verify_binary` is `run_add`'s `install_needed` block body (`machine.rs:132-166`) moved verbatim into its own function, using `host`/`control_path`/`local_version` parameters in place of the enclosing scope's locals. Replace that block in `run_add` with a call to `install_and_verify_binary(&host, &control_path, local_version)?` followed by `println!("{}", describe_install_method(method, &host));`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-ui-shared describe_install_method version_mismatch_after_install`
Expected: PASS

- [ ] **Step 5: Confirm `run_add`'s existing test and behavior are unchanged**

Run: `cargo test -p ninox-ui-shared machine::` and `cargo clippy -p ninox-ui-shared --all-targets -- -D warnings`
Expected: PASS, no warnings. (No behavior test exists for `run_add`'s interactive flow — this is a visual diff check: `git diff` should show the `install_needed` block's logic moved, not changed.)

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-ui-shared/src/machine.rs
git commit -m "refactor(remote): extract shared binary install/verify helpers from machine add"
```

---

### Task 2: `ninox machine update <id>`

**Files:**
- Modify: `crates/ninox-ui-shared/src/machine.rs` (new `MachineAction::Update` variant, `run_update`, `find_profile` helper, dispatch in `run()`)
- Modify: `crates/ninox-core/skills/orchestrator/remote-machines.md` (document the new subcommand)

**Interfaces:**
- Consumes: `remote::probe_remote_version`, `remote::needs_binary_install`, `remote::ensure_control_master`, `remote::control_socket_path` (all pre-existing in `ninox_core::remote`); `install_and_verify_binary`, `describe_install_method` from Task 1.
- Produces: `fn find_profile<'a>(cfg: &'a AppConfig, id: &str) -> Result<&'a MachineProfile>` — looked up by exact `id` match against `cfg.remote_machines.machines`, erroring with `"no machine profile with id {id}"` (same wording `run_remove` already uses) when absent.
- Produces: `MachineAction::Update { id: String }` (clap subcommand variant) and `fn run_update(id: &str) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

```rust
fn fake_config_with(machines: Vec<MachineProfile>) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.remote_machines.machines = machines;
    cfg
}

#[test]
fn find_profile_returns_the_matching_profile() {
    let cfg = fake_config_with(vec![fake_profile("prof1", "box")]);
    let found = find_profile(&cfg, "prof1").unwrap();
    assert_eq!(found.ssh_target, "box");
}

#[test]
fn find_profile_errors_clearly_for_an_unknown_id() {
    let cfg = fake_config_with(vec![fake_profile("prof1", "box")]);
    let err = find_profile(&cfg, "nonexistent").unwrap_err();
    assert_eq!(err.to_string(), "no machine profile with id nonexistent");
}

#[test]
fn update_subcommand_parses_the_profile_id() {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: MachineAction,
    }
    let cli = Cli::parse_from(["ninox-machine", "update", "prof1"]);
    assert!(matches!(cli.action, MachineAction::Update { id } if id == "prof1"));
}
```

(`fake_profile` already exists in `machine.rs`'s test module per `default_label_uses_the_host_portion_of_user_at_host`'s neighbors in `remote.rs` — if `machine.rs`'s test module doesn't already have one, add it mirroring `remote.rs`'s `fake_profile` helper, `remote.rs:846-854`.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-ui-shared find_profile update_subcommand_parses`
Expected: FAIL to compile (`find_profile`, `MachineAction::Update` not found)

- [ ] **Step 3: Implement `find_profile`**

Mirror `run_remove`'s existing lookup (`machine.rs:70-74`) but return a borrowed `&MachineProfile` instead of removing it, with the identical error wording so both commands report a missing id the same way.

- [ ] **Step 4: Add the `Update` variant and wire `run()`**

```rust
/// Check an already-added machine for a version-skewed remote binary and,
/// with approval, upgrade it to match this machine's version
Update {
    /// Profile id (see `ninox machine list`)
    id: String,
},
```

Add `MachineAction::Update { id } => run_update(&id),` to `run()`'s match.

- [ ] **Step 5: Implement `fn run_update(id: &str) -> Result<()>`**

Sequence (mirrors `run_add`'s shape, minus the session-discovery/spawn steps, which are `machine add`'s job, not `machine update`'s):

1. `AppConfig::load()?`, then `find_profile(&cfg, id)?` to get the profile (clone `ssh_target`/`id` out before any mutable borrow concerns — `AppConfig` isn't being mutated here).
2. `let control_path = remote::control_socket_path(&profile.id);` then `remote::ensure_control_master(&profile.ssh_target, &control_path)?` (same interactive-capable connect `run_add` uses — a profile can be currently disconnected).
3. `let local_version = env!("CARGO_PKG_VERSION");` and `let remote_version = remote::probe_remote_version(&profile.ssh_target, &control_path)?;`
4. If `!remote::needs_binary_install(remote_version.as_deref(), local_version)`: print `"{host} is already on ninox {local_version}"` and `return Ok(())` — no prompt, no further SSH calls.
5. Otherwise print what will change (`"{host}: {old} -> {local_version}"` when `remote_version` is `Some`, or `"{host} has no ninox binary — install v{local_version}?"` when `None`) and call the existing `prompt_yes_no` — `bail!("aborted: update declined")` on a No, matching `run_add`'s abort wording style.
6. On approval: `println!("Updating ninox on {host}...")`, call `install_and_verify_binary(&profile.ssh_target, &control_path, local_version)?`, then `println!("{}", describe_install_method(method, &profile.ssh_target));`.
7. If `remote_version.is_some()` (there was a previous build with sessions that may be running under it): `println!("Restarting live sessions on {host} to pick up the new build...");` then best-effort `let _ = remote::run_remote_ninox(&profile.ssh_target, &control_path, "restart --all");` — identical to `run_add`'s existing restart step (`machine.rs:181-184`), since the remote's own `ninox restart` already performs whatever live-reload handoff a local restart does; this machine never participates in that handoff itself (spec resolved decision #5).

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p ninox-ui-shared find_profile update_subcommand_parses`
Expected: PASS

- [ ] **Step 7: Document the new subcommand**

Add a "Keeping it up to date" section to `crates/ninox-core/skills/orchestrator/remote-machines.md` (after "Listing and removing", before "Staying connected") documenting `ninox machine update <id>`: what it checks, that it's approval-gated the same as `machine add`, and that it restarts remote sessions on that host so they pick up the new build.

- [ ] **Step 8: Full verification**

Run: `cargo fmt -p ninox-ui-shared -- --check` (new code only — do not reformat the whole file if it reports pre-existing diffs elsewhere), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
Expected: all PASS

- [ ] **Step 9: Commit**

```bash
git add crates/ninox-ui-shared/src/machine.rs crates/ninox-core/skills/orchestrator/remote-machines.md
git commit -m "feat(remote): add ninox machine update for version-skew detection and upgrade"
```
