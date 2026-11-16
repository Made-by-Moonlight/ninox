use std::sync::Arc;

use super::mock::MockBackend;
use super::*;

fn runtime(tmux: &Arc<MockBackend>, ptyd: Option<&Arc<MockBackend>>) -> Runtime {
    Runtime::new(tmux.clone(), ptyd.map(|p| p.clone() as Arc<dyn SessionBackend>))
}

#[tokio::test]
async fn existing_sessions_go_to_ptyd_only_when_ptyd_knows_the_pane() {
    let tmux = MockBackend::new(Backend::Tmux, &["legacy"]);
    let ptyd = MockBackend::new(Backend::Ptyd, &["fresh"]);
    let rt = runtime(&tmux, Some(&ptyd));

    assert_eq!(rt.for_session("fresh").await.kind(), Backend::Ptyd);
    assert_eq!(rt.for_session("legacy").await.kind(), Backend::Tmux);
    assert_eq!(rt.for_session("unknown").await.kind(), Backend::Tmux);
}

#[tokio::test]
async fn without_a_ptyd_backend_everything_is_tmux() {
    let tmux = MockBackend::new(Backend::Tmux, &[]);
    let rt = runtime(&tmux, None);

    assert_eq!(rt.for_session("x").await.kind(), Backend::Tmux);
    assert_eq!(rt.for_new(Backend::Ptyd).kind(), Backend::Tmux);
}

#[tokio::test]
async fn a_machine_id_routes_to_remote_regardless_of_local_backend_state() {
    let tmux = MockBackend::new(Backend::Tmux, &["fresh"]);
    let ptyd = MockBackend::new(Backend::Ptyd, &["fresh"]);
    let rt = runtime(&tmux, Some(&ptyd));

    // Without a machine_id, local dispatch (ptyd-knows-it) applies as usual.
    assert_eq!(rt.for_existing("fresh", &SessionKindData::Local).await.kind(), Backend::Ptyd);
    // With one, remote wins even though a local backend also knows the id.
    let remote = SessionKindData::Remote { machine_id: "m1".into() };
    assert_eq!(rt.for_existing("fresh", &remote).await.kind(), Backend::Remote);
    assert_eq!(rt.for_existing("unknown", &remote).await.kind(), Backend::Remote);
}

#[tokio::test]
async fn new_sessions_follow_the_configured_backend() {
    let tmux = MockBackend::new(Backend::Tmux, &[]);
    let ptyd = MockBackend::new(Backend::Ptyd, &[]);
    let rt = runtime(&tmux, Some(&ptyd));

    assert_eq!(rt.for_new(Backend::Ptyd).kind(), Backend::Ptyd);
    assert_eq!(rt.for_new(Backend::Tmux).kind(), Backend::Tmux);
}

#[tokio::test]
async fn for_new_with_a_hand_edited_remote_config_errors_honestly_instead_of_silently_using_tmux() {
    // `[runtime] backend` never legitimately holds `Remote` (it's only ever
    // selected per session via `machine_id`), but `Backend` derives
    // `Deserialize` with no narrower validation, so a hand-edited config
    // file could still produce one. `for_new` must not silently fall back
    // to tmux for it.
    let tmux = MockBackend::new(Backend::Tmux, &[]);
    let rt = runtime(&tmux, None);
    assert_eq!(rt.for_new(Backend::Remote).kind(), Backend::Remote);
}

#[tokio::test]
async fn kill_of_a_ptyd_pane_also_clears_a_same_named_tmux_session_best_effort() {
    let tmux = Arc::new(MockBackend {
        fail_kill: true,
        ..MockBackend::raw(Backend::Tmux, &["dup"])
    });
    let ptyd = MockBackend::new(Backend::Ptyd, &["dup"]);
    let rt = runtime(&tmux, Some(&ptyd));

    rt.kill_session("dup").await.expect("a tmux failure after a ptyd kill is not an error");
    assert_eq!(ptyd.calls(), vec!["kill dup"]);
    assert_eq!(tmux.calls(), vec!["kill dup"]);
}

#[tokio::test]
async fn kill_of_an_unknown_pane_goes_to_tmux_and_propagates_its_errors() {
    let tmux = Arc::new(MockBackend {
        fail_kill: true,
        ..MockBackend::raw(Backend::Tmux, &[])
    });
    let ptyd = MockBackend::new(Backend::Ptyd, &[]);
    let rt = runtime(&tmux, Some(&ptyd));

    assert!(rt.kill_session("gone").await.is_err());
    assert!(ptyd.calls().is_empty());
}

#[tokio::test]
async fn list_merges_both_backends_with_ptyd_winning_duplicates() {
    let tmux = MockBackend::new(Backend::Tmux, &["a", "dup"]);
    let ptyd = MockBackend::new(Backend::Ptyd, &["dup", "b"]);
    let rt = runtime(&tmux, Some(&ptyd));

    let listed: Vec<(String, Backend)> =
        rt.list_sessions().await.unwrap().into_iter().map(|s| (s.id, s.backend)).collect();
    assert_eq!(
        listed,
        vec![
            ("dup".into(), Backend::Ptyd),
            ("b".into(), Backend::Ptyd),
            ("a".into(), Backend::Tmux),
        ]
    );
}

#[tokio::test]
async fn caller_identity_prefers_ptyd_then_falls_back_to_tmux() {
    let ident = |name: &str| PaneIdentity {
        physical_tmux_name: name.into(),
        pane_id: format!("%{name}"),
        pane_pid: 1,
        pane_created_at: 0,
        server_epoch: "e".into(),
    };
    let tmux = Arc::new(MockBackend {
        identity: Some(ident("t")),
        ..MockBackend::raw(Backend::Tmux, &[])
    });
    let ptyd_none = MockBackend::new(Backend::Ptyd, &[]);
    assert_eq!(
        runtime(&tmux, Some(&ptyd_none)).current_pane_identity().await.unwrap(),
        Some(ident("t"))
    );

    let ptyd = Arc::new(MockBackend {
        identity: Some(ident("p")),
        ..MockBackend::raw(Backend::Ptyd, &[])
    });
    assert_eq!(
        runtime(&tmux, Some(&ptyd)).current_pane_identity().await.unwrap(),
        Some(ident("p"))
    );
}

#[tokio::test]
async fn liveness_is_unknown_only_while_ptyd_cannot_answer() {
    let tmux = MockBackend::new(Backend::Tmux, &["on-tmux"]);
    let ptyd = MockBackend::new(Backend::Ptyd, &["on-ptyd"]);
    let rt = runtime(&tmux, Some(&ptyd));
    assert_eq!(rt.liveness("on-ptyd").await, Liveness::Live);
    assert_eq!(rt.liveness("on-tmux").await, Liveness::Live);
    assert_eq!(rt.liveness("gone").await, Liveness::Dead);

    // Mid-upgrade / slow start: the host doesn't answer.
    *ptyd.answering.lock().unwrap() = false;
    ptyd.panes.lock().unwrap().clear();
    assert_eq!(rt.liveness("on-ptyd").await, Liveness::Unknown, "must not be reconciled dead");
    assert_eq!(rt.liveness("on-tmux").await, Liveness::Live, "tmux still answers for its own panes");

    let without_ptyd = runtime(&tmux, None);
    assert_eq!(without_ptyd.liveness("gone").await, Liveness::Dead);
}

#[tokio::test]
async fn after_a_reboot_the_host_is_started_and_its_missing_panes_are_dead() {
    let tmux = MockBackend::new(Backend::Tmux, &[]);
    let ptyd = Arc::new(MockBackend {
        answering: std::sync::Mutex::new(false),
        starts_on_ensure: true,
        ..MockBackend::raw(Backend::Ptyd, &[])
    });
    let rt = runtime(&tmux, Some(&ptyd));
    assert_eq!(rt.liveness("was-on-ptyd").await, Liveness::Unknown);

    rt.prepare_liveness(Backend::Ptyd).await;
    assert_eq!(ptyd.calls(), vec!["ensure configured=true"]);
    assert_eq!(rt.liveness("was-on-ptyd").await, Liveness::Dead);
}

#[test]
fn tail_lines_trims_trailing_blanks_and_keeps_the_last_n() {
    let raw = "one\ntwo\nthree\n\n   \n";
    assert_eq!(tail_lines(raw, None), "one\ntwo\nthree");
    assert_eq!(tail_lines(raw, Some(2)), "two\nthree");
    assert_eq!(tail_lines(raw, Some(10)), "one\ntwo\nthree");
    assert_eq!(tail_lines("\n\n", Some(3)), "");
}

#[test]
fn test_binaries_reach_ptyd_only_through_a_temp_dir_socket() {
    let temp = Path::new("/var/folders/xy/T");
    assert!(ptyd_allowed_for(false, None, temp), "the real app always may");
    assert!(!ptyd_allowed_for(true, None, temp));
    assert!(
        !ptyd_allowed_for(true, Some(Path::new("/Users/me/Library/Application Support/ninox/ptyd.sock")), temp),
        "the real socket exported into every ptyd pane must not count as an override"
    );
    assert!(ptyd_allowed_for(true, Some(Path::new("/var/folders/xy/T/.tmpAbc/ptyd.sock")), temp));
}

#[test]
fn this_test_binary_resolves_ptyd_to_tmux() {
    if std::env::var_os(ninox_ptyd::SOCKET_ENV).is_some() {
        return; // an isolated host was deliberately provided
    }
    assert!(!ptyd_allowed());
    assert_eq!(effective_backend(Backend::Ptyd), Backend::Tmux);
    assert!(Runtime::current().ptyd.is_none());
}

#[test]
fn embedded_attach_args_disables_detach_only_for_pane_attach() {
    let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        super::embedded_attach_args(argv(&["ninox", "pane", "attach", "s1"])),
        argv(&["ninox", "pane", "attach", "--no-detach", "--follow-size", "s1"]),
    );
    let tmux = argv(&["tmux", "-L", "ninox", "attach-session", "-t", "s1"]);
    assert_eq!(super::embedded_attach_args(tmux.clone()), tmux);
    let already = argv(&["ninox", "pane", "attach", "--no-detach", "--follow-size", "s1"]);
    assert_eq!(super::embedded_attach_args(already.clone()), already);
}

#[test]
fn viewer_attach_args_flag_only_tmux_attaches_ignore_size() {
    let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let tmux = argv(&["tmux", "-L", "ninox", "attach-session", "-t", "=s1"]);
    let viewer = super::viewer_attach_args(tmux.clone());
    assert_eq!(&viewer[..tmux.len()], &tmux[..]);
    assert_eq!(&viewer[tmux.len()..], ["-f", "ignore-size"]);
    let pane = argv(&["ninox", "pane", "attach", "s1"]);
    assert_eq!(super::viewer_attach_args(pane.clone()), pane);
}

#[test]
fn current_process_identity_matches_the_running_test_process() {
    let id = current_process_identity();
    assert_eq!(id.pid, std::process::id());
    assert!(id.created_at > 0);
}

/// A stub session-kind behavior overriding only `pid_is_local`, proving
/// `SessionReactions::pid_is_local` dispatches generically through any
/// implementor rather than through a hardcoded kind check — mirroring
/// `reactions.rs`'s `StubReactions` test for the same trait's other methods.
struct StubRemoteLikeReactions;
impl SessionReactions for StubRemoteLikeReactions {
    fn pid_is_local(&self) -> bool {
        false
    }
}

#[test]
fn pid_is_local_defaults_true_and_is_overridable_by_any_session_reactions_impl() {
    assert!(MockBackend::raw(Backend::Tmux, &[]).pid_is_local(), "every current local kind defaults to true");
    assert!(!StubRemoteLikeReactions.pid_is_local(), "a remote-hosted kind must be able to override it to false");
}

fn mock_session(machine_id: Option<&str>) -> crate::types::Session {
    crate::types::Session {
        id: "s1".into(), orchestrator_id: None, name: "my-fix".into(),
        repo: "org/repo".into(), status: crate::types::SessionStatus::Working,
        agent_type: "claude-code".into(), cost_usd: 0.0,
        started_at: 0, pr_number: None, pr_id: None,
        workspace_path: None, pid: None,
        model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: None,
        summary: None,
        terminal_at: None, gate_status: None, merged_at: None,
        activity: Default::default(), activity_note: None, activity_since: None,
        machine_id: machine_id.map(str::to_string),
        worker_type: None,
        success_criteria: Vec::new(),
        completion_outcome: None,
        self_completed_at: None,
        cleanup_result: None, cleanup_at: None,
    }
}

#[test]
fn reaction_behavior_for_session_resolves_by_the_session_s_own_machine_id() {
    let _iso = crate::test_isolation::isolate();
    // A local session (`machine_id: None`) follows the configured backend —
    // test binaries are always forced onto tmux, so its pid is local.
    assert!(reaction_behavior_for_session(&mock_session(None)).pid_is_local());
    // A remote session's pid is never local, regardless of what
    // `[runtime] backend` says — this must not be the globally configured
    // backend's answer, but the session's own.
    assert!(!reaction_behavior_for_session(&mock_session(Some("m1"))).pid_is_local());
}

#[test]
fn a_scratch_ptyd_socket_too_long_for_unix_falls_back_to_the_temp_dir() {
    let short = std::path::Path::new("/tmp/s");
    assert_eq!(super::scratch_socket(short, std::path::Path::new("/tmp")), short.join("ptyd.sock"));
    let long = std::path::PathBuf::from(format!("/tmp/{}", "d".repeat(200)));
    let socket = super::scratch_socket(&long, std::path::Path::new("/var/t"));
    assert!(socket.starts_with("/var/t"), "{}", socket.display());
    assert!(socket.as_os_str().len() <= ninox_ptyd::max_socket_path_len());
}
