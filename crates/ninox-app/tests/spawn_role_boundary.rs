use std::{path::Path, process::Command};

use tempfile::tempdir;

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
