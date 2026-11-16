//! `ninox ptyd`, `ninox pane` and `ninox read`: the hot-path CLI over the
//! session runtime. None of these need the tmux config, wrapper hooks or
//! self-shim, so `main` dispatches them before that setup.

use clap::Subcommand;
use ninox_ptyd::{attach, PaneInfo, PtydClient};

const CLIENT_NAME: &str = "ninox-cli";

/// The TUI's prefix (`[tui] prefix`; C-\ on macOS, else C-Space) then `d`, so one
/// chord detaches everywhere. Ctrl-b / Ctrl-a would collide with a user's
/// own tmux/screen prefix.
fn detach_chord() -> (u8, u8) {
    (ninox_core::config::AppConfig::load_or_default().tui.prefix_byte_or_default(), b'd')
}

/// The TUI's viewer panes are plumbing, not sessions.
fn session_panes(panes: Vec<PaneInfo>) -> Vec<PaneInfo> {
    panes.into_iter().filter(|p| !ninox_core::runtime::is_viewer_pane(&p.pane)).collect()
}

#[derive(Subcommand)]
pub(crate) enum PtydAction {
    /// Report whether the host is running.
    Status,
    /// Stop the host and every pane it holds.
    Stop,
}

#[derive(Subcommand)]
pub(crate) enum PaneAction {
    /// Bridge this terminal to a pane. Detach with the TUI prefix (default
    /// C-\ on macOS, C-Space elsewhere) then d.
    Attach {
        pane_id: String,
        /// Disable the detach chord (embedders close the bridge by killing it).
        #[arg(long)]
        no_detach: bool,
        /// Precede each repaint with the pane's size (`CSI 8 ; rows ; cols t`)
        /// for an embedder that emulates the stream (the desktop app).
        #[arg(long, hide = true)]
        follow_size: bool,
    },
    /// List the host's panes.
    List {
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run_ptyd(takeover: bool, action: Option<PtydAction>) -> anyhow::Result<()> {
    let socket = ninox_core::runtime::ptyd_socket_path();
    match action {
        None => {
            let checkpoints = Some(ninox_core::runtime::ptyd_checkpoint_dir());
            if takeover {
                ninox_ptyd::run_host_takeover(socket, checkpoints).await
            } else {
                ninox_ptyd::run_host(socket, checkpoints).await
            }
        }
        Some(PtydAction::Status) => match PtydClient::connect(&socket, CLIENT_NAME).await {
            Ok(mut client) => {
                let (pid, epoch_ms) = client.host_identity();
                let panes = session_panes(client.list().await?);
                let live = panes.iter().filter(|p| p.alive).count();
                println!(
                    "ptyd running: pid {pid}, epoch {epoch_ms}, {live} live / {} panes, socket {}",
                    panes.len(),
                    socket.display()
                );
                Ok(())
            }
            Err(_) => {
                println!("ptyd not running (socket {})", socket.display());
                std::process::exit(1);
            }
        },
        Some(PtydAction::Stop) => match PtydClient::connect(&socket, CLIENT_NAME).await {
            Ok(mut client) => {
                client.shutdown().await?;
                println!("ptyd stopped");
                Ok(())
            }
            Err(_) => {
                println!("ptyd not running");
                Ok(())
            }
        },
    }
}

pub(crate) async fn run_pane(action: PaneAction, db_path: std::path::PathBuf) -> anyhow::Result<()> {
    let socket = ninox_core::runtime::ptyd_socket_path();
    match action {
        PaneAction::Attach { pane_id, no_detach, follow_size } => {
            // A remote-tagged session recurses into its own machine's
            // `ninox pane attach` over SSH (resolved decision #1 of the
            // remote-sessions design spec) instead of bridging to the local
            // ptyd socket below, which holds no pane for it at all.
            if let Some(machine_id) = remote_machine_id(&db_path, &pane_id)? {
                let argv = crate::connect::remote_pane_attach_argv(&pane_id, &machine_id, no_detach, follow_size).await?;
                return crate::connect::exec_attach(argv);
            }
            use std::io::IsTerminal;
            let opts = attach::AttachOptions {
                detach: (!no_detach).then(detach_chord),
                force_no_tty: !std::io::stdin().is_terminal(),
                follow_size,
            };
            match attach::attach(&socket, &pane_id, opts).await? {
                attach::AttachEnd::Detached => {
                    eprintln!("[detached from {pane_id}]");
                    Ok(())
                }
                attach::AttachEnd::PaneExited(code) => std::process::exit(code.unwrap_or(0)),
                attach::AttachEnd::HostGone => anyhow::bail!("ptyd host went away while attached to {pane_id}"),
            }
        }
        PaneAction::List { json } => {
            let panes = match PtydClient::connect(&socket, CLIENT_NAME).await {
                Ok(mut client) => session_panes(client.list().await?),
                Err(_) => Vec::new(),
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&panes)?);
            } else {
                print!("{}", format_panes(&panes));
            }
            Ok(())
        }
    }
}

/// `pane_id`'s `machine_id`, if it names a session the local store knows
/// about. `None` for an unknown id — never an error: it just means "not a
/// remote-tagged session," so the caller falls through to the existing
/// local ptyd attach.
fn remote_machine_id(db_path: &std::path::Path, pane_id: &str) -> anyhow::Result<Option<String>> {
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let store = ninox_core::store::Store::open(db_path)?;
    Ok(store.get_session(pane_id)?.and_then(|s| match s.kind_data() {
        ninox_core::types::SessionKindData::Remote { machine_id } => Some(machine_id),
        ninox_core::types::SessionKindData::Local => None,
    }))
}

fn format_panes(panes: &[PaneInfo]) -> String {
    if panes.is_empty() {
        return "no ptyd panes\n".to_string();
    }
    let mut out = String::new();
    for p in panes {
        let state = match (p.alive, p.exit_code) {
            (true, _) => "live".to_string(),
            (false, Some(code)) => format!("exited {code}"),
            (false, None) => "exited".to_string(),
        };
        out.push_str(&format!(
            "{:<32} pid {:<7} {:<10} {}x{}{}\n",
            p.pane,
            p.pid,
            state,
            p.cols,
            p.rows,
            p.title.as_deref().map(|t| format!("  {t}")).unwrap_or_default(),
        ));
    }
    out
}

pub(crate) async fn run_read(session_id: &str, lines: Option<usize>, ansi: bool) -> anyhow::Result<()> {
    let screen = ninox_core::runtime::read_screen(session_id, lines, ansi).await?;
    println!("{screen}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, alive: bool, exit_code: Option<i32>, title: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane: id.into(),
            pid: 42,
            cols: 140,
            rows: 50,
            alive,
            exit_code,
            created_ms: 0,
            last_output_ms: 0,
            title: title.map(str::to_string),
            cwd: "/tmp".into(),
            seq: 0, history_size: 0,
        }
    }

    #[test]
    fn pane_list_shows_state_size_and_title() {
        let out = format_panes(&[pane("w1", true, None, Some("claude")), pane("w2", false, Some(3), None)]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("w1") && lines[0].contains("live") && lines[0].ends_with("140x50  claude"), "{out}");
        assert!(lines[1].contains("exited 3"), "{out}");
        assert_eq!(format_panes(&[]), "no ptyd panes\n");
    }

    #[tokio::test]
    async fn pane_attach_for_a_remote_session_with_no_configured_profile_errors_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let store = ninox_core::store::Store::open(&db_path).unwrap();
        let mut s = crate::cli::test_fixtures::session("remote-1", None, ninox_core::types::SessionStatus::Working);
        s.machine_id = Some("prof1".into());
        store.upsert_session(&s).unwrap();
        drop(store);

        let err = match run_pane(PaneAction::Attach { pane_id: "remote-1".into(), no_detach: false, follow_size: false }, db_path).await {
            Err(e) => e,
            Ok(_) => panic!("expected an error for an unconfigured machine profile"),
        };
        assert!(err.to_string().contains("prof1"), "{err}");
    }

    #[test]
    fn remote_machine_id_is_none_for_a_stored_local_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let store = ninox_core::store::Store::open(&db_path).unwrap();
        let s = crate::cli::test_fixtures::session("local-1", None, ninox_core::types::SessionStatus::Working);
        store.upsert_session(&s).unwrap();
        drop(store);

        assert_eq!(remote_machine_id(&db_path, "local-1").unwrap(), None);
    }

    #[tokio::test]
    async fn pane_attach_for_an_unknown_id_falls_through_to_the_local_path() {
        let _iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        // "nope" isn't in the store at all, so the remote branch must be
        // skipped entirely — any error that does surface (from the local
        // ptyd socket not existing in this test environment) must not
        // mention a machine profile.
        if let Err(e) = run_pane(PaneAction::Attach { pane_id: "nope".into(), no_detach: false, follow_size: false }, db_path).await {
            assert!(!e.to_string().contains("machine profile"), "{e}");
        }
    }

    #[test]
    fn pane_list_hides_tui_viewer_panes() {
        let viewer = ninox_core::runtime::viewer_pane_id(7, "w1");
        let shown = session_panes(vec![pane("w1", true, None, None), pane(&viewer, true, None, None)]);
        assert_eq!(shown.iter().map(|p| p.pane.as_str()).collect::<Vec<_>>(), ["w1"]);
    }
}
