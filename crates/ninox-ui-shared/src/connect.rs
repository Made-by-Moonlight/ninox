use ninox_core::{
    config::AppConfig, lifecycle::poller::{harness_running_elsewhere, reconcile_dead_session}, runtime, store::Store,
    types::{SessionKindData, SessionStatus},
};

pub enum ConnectPlan {
    Attach(Vec<String>),
    /// The tmux session is gone. `status` is what the row now holds:
    /// Interrupted (resumable from the app) or Terminated.
    Dead { id: String, status: SessionStatus },
    NotFound { suggestions: Vec<String> },
}

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
    // A remote session's pid is meaningless to a local liveness check
    // (resolved decision #3 of the remote-sessions design spec) — go
    // straight to the SSH exec recursion instead.
    match session.kind_data() {
        SessionKindData::Remote { machine_id } => {
            return Ok(ConnectPlan::Attach(remote_attach_argv(id, &machine_id).await?));
        }
        SessionKindData::Local => {}
    }
    match runtime::liveness(id).await {
        runtime::Liveness::Live => {}
        // A host mid-upgrade or still starting is not evidence the pane is
        // gone; reconciling now would burn a live session.
        runtime::Liveness::Unknown => anyhow::bail!("the ptyd host holding {id} isn't answering yet — try again in a moment"),
        runtime::Liveness::Dead => {
        // Same rule as the poller's startup sweep — Interrupted when the
        // harness can resume it — so a connect right after a reboot (before
        // any daemon has reconciled) never burns a resumable session to
        // Terminated. The helper re-reads the row after the await above.
        let registry = AppConfig::load_or_default().registry();
        let status = match reconcile_dead_session(store, &registry, id)? {
            Some(live) => live.status,
            None => match store.get_session(id)? {
                Some(s) if !s.status.is_terminal() && harness_running_elsewhere(&s, &registry) => anyhow::bail!(
                    "{id} has no pane in any host this ninox can reach, but its harness is still running — \
                     probably under another ninox's ptyd host or tmux server; attach from there"
                ),
                // Already terminal before we looked; report what's there.
                s => s.map(|s| s.status).unwrap_or(SessionStatus::Terminated),
            },
        };
        return Ok(ConnectPlan::Dead { id: id.to_string(), status });
        }
    }
    Ok(ConnectPlan::Attach(runtime::attach_args(id).await))
}

/// The SSH attach argv for `id`'s pane on `machine_id`'s machine, or a clear
/// error when no profile is configured for it — shared by `ninox connect`
/// (above) and `ninox pane attach` (`runtime_cli::run_pane`) so the
/// stale/unconfigured-machine-profile wording lives in exactly one place.
pub(crate) async fn remote_attach_argv(id: &str, machine_id: &str) -> anyhow::Result<Vec<String>> {
    let kind = SessionKindData::Remote { machine_id: machine_id.to_string() };
    let argv = runtime::backend_for(id, &kind).await.attach_args(id).await;
    anyhow::ensure!(
        !argv.is_empty(),
        "no configured machine profile {machine_id} for session {id} — check `ninox machine list`"
    );
    Ok(argv)
}

/// `pane attach` flags to forward to the remote: `--follow-size` only when
/// the caller wants it *and* the remote is known to support it (an older
/// remote rejects the unknown flag and the attach would fail outright).
pub(crate) fn remote_attach_flags(no_detach: bool, follow_size: bool, remote_supports_follow_size: bool) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if no_detach {
        flags.push("--no-detach");
    }
    if follow_size && remote_supports_follow_size {
        flags.push("--follow-size");
    }
    flags
}

/// [`remote_attach_argv`] for `ninox pane attach`, forwarding `--no-detach`
/// and, when the remote supports it, `--follow-size`. The capability probe
/// only runs when `follow_size` was requested.
pub(crate) async fn remote_pane_attach_argv(
    id: &str,
    machine_id: &str,
    no_detach: bool,
    follow_size: bool,
) -> anyhow::Result<Vec<String>> {
    // Validates the profile and gives the stale-profile error wording.
    remote_attach_argv(id, machine_id).await?;
    let profile = ninox_core::config::AppConfig::load_or_default()
        .remote_machines
        .machines
        .into_iter()
        .find(|m| m.id == machine_id && m.enabled)
        .ok_or_else(|| anyhow::anyhow!("no configured machine profile {machine_id} for session {id} — check `ninox machine list`"))?;
    let supports = follow_size && {
        let target = profile.ssh_target.clone();
        let control = ninox_core::remote::control_socket_path(&profile.id);
        tokio::task::spawn_blocking(move || ninox_core::remote::remote_supports_follow_size(&target, &control))
            .await
            .unwrap_or(false)
    };
    let flags = remote_attach_flags(no_detach, follow_size, supports);
    Ok(ninox_core::remote::attach_exec_argv_with(&profile, id, &flags))
}

pub(crate) async fn run_connect(store: &Store, id: &str) -> anyhow::Result<()> {
    match connect_preflight(store, id).await? {
        ConnectPlan::Attach(argv) => exec_attach(argv),
        ConnectPlan::Dead { id, status: SessionStatus::Interrupted } => {
            anyhow::bail!("session {id} has no live tmux session — marked interrupted (resumable from the app)")
        }
        ConnectPlan::Dead { id, status } => {
            anyhow::bail!("session {id} has no live tmux session — {}", crate::cli::status_slug(&status))
        }
        ConnectPlan::NotFound { suggestions } if suggestions.is_empty() => {
            anyhow::bail!("no session named {id} — run `ninox list`")
        }
        ConnectPlan::NotFound { suggestions } => {
            anyhow::bail!("no session named {id} — did you mean: {}", suggestions.join(", "))
        }
    }
}

pub(crate) fn exec_attach(argv: Vec<String>) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
        Err(anyhow::anyhow!("failed to exec {}: {err}", argv[0]))
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status()?;
        anyhow::ensure!(status.success(), "tmux attach exited with {status}");
        Ok(())
    }
}

#[cfg(test)]
mod connect_tests {
    use super::remote_attach_flags;

    #[test]
    fn remote_attach_flags_forward_follow_size_only_to_a_supporting_remote() {
        assert_eq!(remote_attach_flags(true, true, true), ["--no-detach", "--follow-size"]);
        assert_eq!(remote_attach_flags(true, true, false), ["--no-detach"]);
        assert_eq!(remote_attach_flags(false, true, false), Vec::<&str>::new());
        assert_eq!(remote_attach_flags(true, false, true), ["--no-detach"]);
    }

    use super::{connect_preflight, ConnectPlan};
    use ninox_core::{store::Store, types::SessionStatus};

    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("ninox-connect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(dir.join(format!("t-{}.db", rand_suffix()))).unwrap()
    }
    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[tokio::test]
    async fn unknown_id_suggests_prefix_matches() {
        let store = temp_store();
        store.upsert_session(&crate::cli::test_fixtures::session("fix-login-flow", None, SessionStatus::Working)).unwrap();
        let ConnectPlan::NotFound { suggestions } = connect_preflight(&store, "fix").await.unwrap() else {
            panic!("expected NotFound");
        };
        assert_eq!(suggestions, vec!["fix-login-flow"]);
    }

    #[tokio::test]
    async fn dead_session_is_marked_terminated() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = temp_store();
        // No tmux session exists on the private socket for this id, so
        // has_session() is false and connect must mark it Terminated.
        let s = crate::cli::test_fixtures::session("ghost", None, SessionStatus::Working);
        store.upsert_session(&s).unwrap();
        let ConnectPlan::Dead { id, status } = connect_preflight(&store, "ghost").await.unwrap() else {
            panic!("expected Dead");
        };
        assert_eq!(id, "ghost");
        assert_eq!(status, SessionStatus::Terminated);
        let after = store.get_session("ghost").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Terminated);
        assert!(after.terminal_at.is_some(), "Terminated must be stamped so the sweep gives it a window");
    }

    #[tokio::test]
    async fn remote_session_skips_local_liveness_and_errors_clearly_with_no_configured_profile() {
        let store = temp_store();
        // No local tmux/ptyd pane exists for this id at all — if this
        // routed through local liveness it would report Dead/Terminated
        // instead of reaching the remote-attach branch.
        let mut s = crate::cli::test_fixtures::session("remote-1", None, SessionStatus::Working);
        s.machine_id = Some("prof1".into());
        store.upsert_session(&s).unwrap();
        // No machine profile named "prof1" is configured, so the remote
        // backend resolves unresolved and attach_args is empty — this must
        // surface as a clear error, not a panic or a fabricated Attach.
        let err = match connect_preflight(&store, "remote-1").await {
            Err(e) => e,
            Ok(_) => panic!("expected an error for an unconfigured machine profile"),
        };
        assert!(err.to_string().contains("prof1"), "{err}");
    }

    #[tokio::test]
    async fn dead_resumable_session_is_marked_interrupted_not_terminated() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = temp_store();
        // A claude_session_id under the default (resumable) harness: connect
        // must preserve resumability exactly like the poller's sweep would.
        let mut s = crate::cli::test_fixtures::session("nap", None, SessionStatus::Working);
        s.claude_session_id = Some("abc".into());
        store.upsert_session(&s).unwrap();
        let ConnectPlan::Dead { status, .. } = connect_preflight(&store, "nap").await.unwrap() else {
            panic!("expected Dead");
        };
        assert_eq!(status, SessionStatus::Interrupted);
        assert_eq!(store.get_session("nap").unwrap().unwrap().status, SessionStatus::Interrupted);
    }
}
