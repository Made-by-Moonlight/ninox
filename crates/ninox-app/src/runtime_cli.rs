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
    (ninox_core::config::AppConfig::load().unwrap_or_default().tui.prefix_byte_or_default(), b'd')
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
    },
    /// List the host's panes.
    List {
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run_ptyd(takeover: bool, action: Option<PtydAction>) -> anyhow::Result<()> {
    let socket = ninox_ptyd::socket_path();
    match action {
        None => {
            let checkpoints = Some(ninox_ptyd::checkpoint::default_dir());
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

pub(crate) async fn run_pane(action: PaneAction) -> anyhow::Result<()> {
    let socket = ninox_ptyd::socket_path();
    match action {
        PaneAction::Attach { pane_id, no_detach } => {
            use std::io::IsTerminal;
            let opts = attach::AttachOptions {
                detach: (!no_detach).then(detach_chord),
                force_no_tty: !std::io::stdin().is_terminal(),
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

    #[test]
    fn pane_list_hides_tui_viewer_panes() {
        let viewer = ninox_core::runtime::viewer_pane_id(7, "w1");
        let shown = session_panes(vec![pane("w1", true, None, None), pane(&viewer, true, None, None)]);
        assert_eq!(shown.iter().map(|p| p.pane.as_str()).collect::<Vec<_>>(), ["w1"]);
    }
}
