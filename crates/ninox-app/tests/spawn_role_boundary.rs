use std::{path::Path, process::Command};

use tempfile::tempdir;

// Unlike `completion_command` below, this doesn't need an isolated
// `TMUX_TMPDIR`: `reject_recursive_worker_spawn` runs as the very first
// statement of `spawn` handling, before any tmux interaction, so the
// ambient-socket hazard that motivates that isolation never comes into play
// here. If `spawn` ever grows a tmux check ahead of that role rejection,
// this helper will need the same treatment.
fn denied_spawn(
    root: &Path,
    execution_role: Option<&str>,
    legacy_caller_type: Option<&str>,
) -> std::process::Output {
    let config = root.join("config.toml");
    std::fs::write(&config, "").unwrap();
    let missing_workspace = root.join("missing-workspace");

    let mut command = Command::new(env!("CARGO_BIN_EXE_ninox"));
    command
        .arg("--db")
        .arg(root.join("state/ninox.db"))
        .args([
            "spawn",
            "--prompt",
            "delegate again",
            "--workspace",
            missing_workspace.to_str().unwrap(),
            "--name",
            "nested-worker",
        ])
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("NINOX_CONFIG", config)
        .env("NINOX_ORCHESTRATOR_ID", "parent-orchestrator")
        .env_remove("NINOX_EXECUTION_ROLE")
        .env_remove("NINOX_CALLER_TYPE");
    if let Some(role) = execution_role {
        command.env("NINOX_EXECUTION_ROLE", role);
    }
    if let Some(caller_type) = legacy_caller_type {
        command.env("NINOX_CALLER_TYPE", caller_type);
    }
    command.output().unwrap()
}

fn assert_denied_without_allocation(root: &Path, output: &std::process::Output) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("worker sessions cannot spawn workers")
            && stderr.contains("ask the orchestrator"),
        "{stderr}"
    );
    assert!(
        !root.join("state").exists(),
        "denial must happen before database allocation"
    );
}

fn completion_command(
    root: &Path,
    args: &[&str],
    execution_role: &str,
    legacy_caller_type: &str,
) -> std::process::Output {
    // The spawned binary is production `ninox`, not a `deps/`-built test
    // binary, so `tmux::socket()` treats it as the real app and looks for a
    // caller pane on the shared `-L ninox` server (see tmux.rs's
    // `is_test_binary` doc comment). Without an isolated `TMUX_TMPDIR`, that
    // lookup resolves to the OS-default socket directory — the exact one a
    // real, ambient Ninox session on the host also uses — so running this
    // test from inside a live Ninox worker session lets it find a real pane
    // and authorize against it instead of correctly finding none.
    let tmux_tmp = root.join("tmux-isolated");
    std::fs::create_dir_all(&tmux_tmp).unwrap();

    Command::new(env!("CARGO_BIN_EXE_ninox"))
        .arg("--db")
        .arg(root.join("state/ninox.db"))
        .args(args)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("TMUX_TMPDIR", &tmux_tmp)
        .env("NINOX_EXECUTION_ROLE", execution_role)
        .env("NINOX_CALLER_TYPE", legacy_caller_type)
        .env("NINOX_SESSION", "worker")
        .env("NINOX_WORKER_INCARNATION", "incarnation")
        .env("NINOX_ORCHESTRATOR_ID", "orchestrator")
        .output()
        .unwrap()
}

#[test]
fn worker_runtime_role_wins_over_inherited_orchestrator_identity() {
    let root = tempdir().unwrap();
    let output = denied_spawn(root.path(), Some("worker"), Some("orchestrator"));

    assert_denied_without_allocation(root.path(), &output);
}

#[test]
fn legacy_worker_runtime_is_still_denied_without_allocation() {
    let root = tempdir().unwrap();
    let output = denied_spawn(root.path(), None, Some("worker"));

    assert_denied_without_allocation(root.path(), &output);
}

#[test]
fn orchestrator_runtime_reaches_normal_spawn_validation() {
    let root = tempdir().unwrap();
    let output = denied_spawn(root.path(), Some("orchestrator"), Some("orchestrator"));

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("worker sessions cannot spawn workers"), "{stderr}");
    assert!(
        root.path().join("state/ninox.db").exists(),
        "orchestrator must pass the role boundary"
    );
}

#[test]
fn completion_commands_use_authoritative_runtime_role() {
    let complete_root = tempdir().unwrap();
    let complete = completion_command(
        complete_root.path(),
        &["complete", "canonical summary"],
        "orchestrator",
        "worker",
    );
    assert!(!complete.status.success());
    assert!(
        String::from_utf8_lossy(&complete.stderr)
            .contains("completion is only available inside the current worker runtime")
    );

    let receive_root = tempdir().unwrap();
    let receive = completion_command(
        receive_root.path(),
        &["receive-completion", "completion-id"],
        "worker",
        "orchestrator",
    );
    assert!(!receive.status.success());
    assert!(
        String::from_utf8_lossy(&receive.stderr)
            .contains("worker inspection is only available inside the owning orchestrator session")
    );

    let worker_root = tempdir().unwrap();
    let worker = completion_command(
        worker_root.path(),
        &["complete", "canonical summary"],
        "worker",
        "orchestrator",
    );
    assert!(!worker.status.success());
    assert!(
        String::from_utf8_lossy(&worker.stderr)
            .contains("worker owner is not a persisted orchestrator")
    );

    let orchestrator_root = tempdir().unwrap();
    let orchestrator = completion_command(
        orchestrator_root.path(),
        &["receive-completion", "completion-id"],
        "orchestrator",
        "worker",
    );
    assert!(!orchestrator.status.success());
    let stderr = String::from_utf8_lossy(&orchestrator.stderr);
    assert!(
        stderr.contains("caller is not running inside a private Ninox pane")
            || stderr.contains("caller session is not a persisted orchestrator"),
        "{stderr}"
    );
}
