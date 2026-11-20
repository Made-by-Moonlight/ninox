//! Ancestry-gate registration for `HumanConsoleIdentity` — the record that
//! marks a `ninox-gui`/`ninox-tui` process as genuinely human-launched (see
//! `docs/superpowers/specs/2026-10-26-user-request-design.md`).
//!
//! An agent (orchestrator or worker) could try to spawn its own
//! `ninox-tui`/`ninox-gui` and have it register as a human console to spoof
//! the escalation channel `user-request` relies on. `register_human_console`
//! closes that off the same way `orchestrator_auth` closes off identity
//! spoofing: real process ancestry can't be forged by an env var or CLI
//! flag. Any process descending from a pid this store already knows is an
//! agent root — a registered `OrchestratorRuntimeIdentity`, or a live
//! `Session`'s own pid — is refused, before any row is written.

use crate::{runtime, store::Store, types::HumanConsoleIdentity};
use anyhow::Result;

pub fn register_human_console(store: &Store, process_name: &str) -> Result<HumanConsoleIdentity> {
    let known_agent_roots = known_agent_root_pids(store)?;
    anyhow::ensure!(
        !descends_from_any(&runtime::process_ancestor_pids(), &known_agent_roots),
        "refusing to register a human console: caller descends from a known agent process"
    );

    let process = runtime::current_process_identity();
    let identity = HumanConsoleIdentity {
        id: format!("hci-{}-{}", process.pid, process.created_at),
        root_pid: process.pid,
        root_created_at: process.created_at,
        process_name: process_name.to_string(),
        registered_at: crate::lifecycle::poller::now_millis(),
    };
    store.register_human_console_identity(&identity)?;
    Ok(identity)
}

/// Every pid this store already recognizes as an agent's root process: each
/// persisted orchestrator's runtime root, plus every non-terminal session's
/// own pid. A terminal session's recorded pid is history, not a live process
/// — after a reboot the OS can hand it to anything, including the human's
/// own shell, so counting it would refuse a genuine console.
fn known_agent_root_pids(store: &Store) -> Result<std::collections::HashSet<u32>> {
    let mut pids: std::collections::HashSet<u32> = store
        .list_orchestrator_runtime_identities()?
        .into_iter()
        .map(|o| o.root_pid)
        .collect();
    pids.extend(
        store.list_sessions()?.into_iter().filter(|s| !s.status.is_terminal()).filter_map(|s| s.pid),
    );
    Ok(pids)
}

/// Intersects the caller's single ancestry walk against every known root,
/// so the `ps` cost is one walk regardless of fleet history size.
fn descends_from_any(ancestors: &[u32], known_roots: &std::collections::HashSet<u32>) -> bool {
    ancestors.iter().any(|pid| known_roots.contains(pid))
}

/// The one check `Store::answer_user_request` gates on: is the CALLING
/// process itself a registered `HumanConsoleIdentity`? Unlike
/// `register_human_console`'s ancestry gate (which runs once, at
/// registration time), this runs on every answer attempt, matching the
/// caller's own live `current_process_identity()` against the persisted
/// rows — the same `root_pid` + `root_created_at` (within 2000ms)
/// comparison `orchestrator_auth.rs`'s `runtime_identity_matches` uses.
pub fn verify_caller_is_registered_console(store: &Store) -> Result<()> {
    let me = runtime::current_process_identity();
    let registered = store.list_human_console_identities()?;
    anyhow::ensure!(
        registered
            .iter()
            .any(|h| h.root_pid == me.pid && (h.root_created_at - me.created_at).abs() <= 2_000),
        "caller is not a registered human console"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Orchestrator, OrchestratorRuntimeIdentity, Session, SessionStatus};
    use tempfile::tempdir;

    fn open_store() -> Store {
        Store::open(tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    /// Same as `open_store`, but also hands back the db file's path so a
    /// spawned child process can open the identical store.
    fn open_store_at() -> (Store, std::path::PathBuf) {
        let path = tempdir().unwrap().keep().join("t.db");
        (Store::open(&path).unwrap(), path)
    }

    fn session_with_pid(id: &str, pid: u32) -> Session {
        Session {
            id: id.to_string(), orchestrator_id: None,
            name: id.to_string(), repo: String::new(), status: SessionStatus::Working,
            agent_type: "claude-code".to_string(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: Some(pid),
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    /// Persists `id` as an orchestrator with a runtime identity rooted at
    /// `root_pid`.
    fn register_orchestrator_root(store: &Store, id: &str, root_pid: u32) {
        store.upsert_orchestrator(&Orchestrator { id: id.to_string(), name: id.to_string(), created_at: 0 }).unwrap();
        store.register_orchestrator_runtime(&OrchestratorRuntimeIdentity {
            orchestrator_id: id.to_string(),
            runtime_id: format!("test:{id}"),
            server_epoch: "test-epoch".to_string(),
            physical_tmux_name: id.to_string(),
            pane_id: format!("%{id}"),
            root_pid,
            root_created_at: 0,
            registered_at: 0,
        }).unwrap();
    }

    #[test]
    fn registers_normally_with_no_known_agent_ancestry() {
        let store = open_store();

        let identity = register_human_console(&store, "ninox-tui").unwrap();

        let expected = runtime::current_process_identity();
        assert_eq!(identity.root_pid, expected.pid);
        assert!(
            (identity.root_created_at - expected.created_at).abs() <= 2_000,
            "root_created_at should match the current process's own start time (within `ps`'s \
             one-second resolution): {} vs {}",
            identity.root_created_at,
            expected.created_at
        );
        assert_eq!(identity.process_name, "ninox-tui");
        assert_eq!(store.list_human_console_identities().unwrap(), vec![identity]);
    }

    #[test]
    fn verify_caller_fails_with_no_registered_identity() {
        let store = open_store();
        assert!(verify_caller_is_registered_console(&store).is_err());
    }

    #[test]
    fn verify_caller_fails_against_a_different_fake_pid() {
        let store = open_store();
        store
            .register_human_console_identity(&HumanConsoleIdentity {
                id: "hci-fake".to_string(),
                root_pid: std::process::id().wrapping_add(999_999),
                root_created_at: 0,
                process_name: "ninox-tui".to_string(),
                registered_at: 0,
            })
            .unwrap();
        assert!(verify_caller_is_registered_console(&store).is_err());
    }

    #[test]
    fn verify_caller_succeeds_after_register_human_console() {
        let store = open_store();
        register_human_console(&store, "ninox-gui").unwrap();
        assert!(verify_caller_is_registered_console(&store).is_ok());
    }

    #[test]
    fn refuses_registration_when_an_ancestor_pid_is_a_known_orchestrator_root() {
        let store = open_store();
        register_orchestrator_root(&store, "orch-1", std::process::id());

        let result = register_human_console(&store, "ninox-tui");

        assert!(result.is_err());
        assert!(store.list_human_console_identities().unwrap().is_empty());
    }

    #[test]
    fn refuses_registration_when_an_ancestor_pid_is_a_live_worker_session_root() {
        let store = open_store();
        store.upsert_session(&session_with_pid("worker-a", std::process::id())).unwrap();

        let result = register_human_console(&store, "ninox-tui");

        assert!(result.is_err());
        assert!(store.list_human_console_identities().unwrap().is_empty());
    }

    #[test]
    fn a_terminal_sessions_stale_pid_does_not_block_registration() {
        let store = open_store();
        for (id, status) in [
            ("done", SessionStatus::Done),
            ("dead", SessionStatus::Terminated),
            ("interrupted", SessionStatus::Interrupted),
        ] {
            store.upsert_session(&Session { status, ..session_with_pid(id, std::process::id()) }).unwrap();
        }

        register_human_console(&store, "ninox-tui").expect("only live sessions' pids are agent roots");
    }

    #[test]
    fn descends_from_any_intersects_one_ancestry_walk_against_all_roots() {
        let roots: std::collections::HashSet<u32> = [7, 42, 99].into_iter().collect();
        assert!(descends_from_any(&[1000, 500, 42, 1], &roots));
        assert!(!descends_from_any(&[1000, 500, 1], &roots));
        assert!(!descends_from_any(&[], &roots));
    }

    /// This test's own fully-qualified name, as the test harness reports and
    /// matches it — needed so the `--exact` filter below re-runs only this
    /// test in the spawned child, not the whole suite.
    const SELF_TEST_NAME: &str = "human_console_auth::tests::agent_spawned_ninox_tui_cannot_register";

    #[test]
    fn agent_spawned_ninox_tui_cannot_register() {
        // Re-exec guard: when a child of this test spawns the very same
        // test binary, it inherits this env var and must short-circuit here
        // — before falling through to the parent logic below, which would
        // otherwise spawn further children forever.
        if let Ok(db_path) = std::env::var("NINOX_TEST_CHILD_REGISTER") {
            let store = Store::open(&db_path).unwrap();
            match register_human_console(&store, "ninox-tui") {
                Ok(_) => println!("NINOX_TEST_RESULT:OK"),
                Err(_) => println!("NINOX_TEST_RESULT:ERR"),
            }
            std::process::exit(0);
        }

        let exe = std::env::current_exe().unwrap();

        // A child of an agent that registered itself as a known orchestrator
        // root (rooted at this test process's own pid, which every real
        // child genuinely descends from) must be refused.
        let (store_a, db_path_a) = open_store_at();
        register_orchestrator_root(&store_a, "orch-1", std::process::id());
        let output_a = std::process::Command::new(&exe)
            .args([SELF_TEST_NAME, "--exact", "--nocapture"])
            .env("NINOX_TEST_CHILD_REGISTER", &db_path_a)
            .output()
            .expect("spawning the test binary as a child must succeed");
        let stdout_a = String::from_utf8_lossy(&output_a.stdout);
        assert!(
            stdout_a.contains("NINOX_TEST_RESULT:ERR"),
            "a process descending from a registered agent root must be refused: {stdout_a}"
        );

        // A sibling child against a store with no agent root registered at
        // all must succeed, even though its real OS ancestry is identical.
        let (_, db_path_b) = open_store_at();
        let output_b = std::process::Command::new(&exe)
            .args([SELF_TEST_NAME, "--exact", "--nocapture"])
            .env("NINOX_TEST_CHILD_REGISTER", &db_path_b)
            .output()
            .expect("spawning the test binary as a child must succeed");
        let stdout_b = String::from_utf8_lossy(&output_b.stdout);
        assert!(
            stdout_b.contains("NINOX_TEST_RESULT:OK"),
            "a process with no known-agent root on record must be able to register: {stdout_b}"
        );
    }
}
