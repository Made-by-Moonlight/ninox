use ninox_core::{config::AppConfig, lifecycle::poller::reconcile_dead_session, runtime, store::Store, types::SessionStatus};

pub(crate) enum ConnectPlan {
    Attach(Vec<String>),
    /// The tmux session is gone. `status` is what the row now holds:
    /// Interrupted (resumable from the app) or Terminated.
    Dead { id: String, status: SessionStatus },
    NotFound { suggestions: Vec<String> },
}

pub(crate) async fn connect_preflight(store: &Store, id: &str) -> anyhow::Result<ConnectPlan> {
    let Some(_) = store.get_session(id)? else {
        let suggestions = store
            .list_sessions()?
            .into_iter()
            .filter(|s| s.id.starts_with(id))
            .map(|s| s.id)
            .collect();
        return Ok(ConnectPlan::NotFound { suggestions });
    };
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
        let registry = AppConfig::load().unwrap_or_default().registry();
        let status = match reconcile_dead_session(store, &registry, id)? {
            Some(live) => live.status,
            // Already terminal before we looked; report what's there.
            None => store.get_session(id)?.map(|s| s.status).unwrap_or(SessionStatus::Terminated),
        };
        return Ok(ConnectPlan::Dead { id: id.to_string(), status });
        }
    }
    Ok(ConnectPlan::Attach(runtime::attach_args(id).await))
}

pub(crate) async fn run_connect(store: &Store, id: &str) -> anyhow::Result<()> {
    match connect_preflight(store, id).await? {
        ConnectPlan::Attach(argv) => exec_attach(argv),
        ConnectPlan::Dead { id, status: SessionStatus::Interrupted } => {
            anyhow::bail!("session {id} has no live tmux session — marked interrupted (resumable from the app)")
        }
        ConnectPlan::Dead { id, status } => {
            anyhow::bail!("session {id} has no live tmux session — {}", crate::status_slug(&status))
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
        store.upsert_session(&crate::test_fixtures::session("fix-login-flow", None, SessionStatus::Working)).unwrap();
        let ConnectPlan::NotFound { suggestions } = connect_preflight(&store, "fix").await.unwrap() else {
            panic!("expected NotFound");
        };
        assert_eq!(suggestions, vec!["fix-login-flow"]);
    }

    #[tokio::test]
    async fn dead_session_is_marked_terminated() {
        let store = temp_store();
        // No tmux session exists on the private socket for this id, so
        // has_session() is false and connect must mark it Terminated.
        let s = crate::test_fixtures::session("ghost", None, SessionStatus::Working);
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
    async fn dead_resumable_session_is_marked_interrupted_not_terminated() {
        let store = temp_store();
        // A claude_session_id under the default (resumable) harness: connect
        // must preserve resumability exactly like the poller's sweep would.
        let mut s = crate::test_fixtures::session("nap", None, SessionStatus::Working);
        s.claude_session_id = Some("abc".into());
        store.upsert_session(&s).unwrap();
        let ConnectPlan::Dead { status, .. } = connect_preflight(&store, "nap").await.unwrap() else {
            panic!("expected Dead");
        };
        assert_eq!(status, SessionStatus::Interrupted);
        assert_eq!(store.get_session("nap").unwrap().unwrap().status, SessionStatus::Interrupted);
    }
}
