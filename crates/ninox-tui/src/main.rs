mod tui;
mod tui_log;

use std::io::IsTerminal;
use std::ops::ControlFlow;

use ninox_core::config::AppConfig;
use ninox_ui_shared::cli::{bootstrap, dispatch, exit_wrong_ui, UiArgs, UiRequest};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tui_log::init();

    // `dispatch` installs the `nx` alias itself (bare `nx` has always meant
    // "open the terminal UI" — reserved for this binary alone, ninox-gui
    // never installs it) once every hot-path subcommand has short-circuited
    // — never before, so hot-path invocations never pay for it.
    match dispatch(true).await? {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(UiRequest::Default(a) | UiRequest::Tui(a)) => run_tui(a).await,
        ControlFlow::Break(UiRequest::Gui(_)) => {
            exit_wrong_ui("ninox-tui", "desktop GUI", "ninox-gui", None)
        }
    }
}

async fn run_tui(a: UiArgs) -> anyhow::Result<()> {
    if a.headless {
        // Headless daemon mode needs no terminal UI at all — shares the
        // exact engine/poller/server bootstrap ninox-gui uses for the same
        // purpose.
        tui_log::to_daemon_file(&ninox_core::daemon_log::log_path(&a.db_path));
        if let Err(e) = bootstrap(a.store, a.port_arg, true, a.db_path).await {
            tracing::error!("daemon failed to start: {e:#}");
            ninox_core::daemon_log::flush();
            return Err(e);
        }
        return Ok(());
    }
    if !std::io::stdout().is_terminal() {
        eprintln!(
            "ninox-tui: stdout is not a terminal; pass --headless to run without one, \
or run this from an interactive terminal."
        );
        std::process::exit(1);
    }
    let log = AppConfig::config_path().with_file_name("tui.log");
    if let Err(e) = tui_log::to_file(&log) {
        eprintln!("ninox-tui: can't log to {}: {e}; log lines may draw over the screen", log.display());
    }
    let config = AppConfig::load_or_default();
    let port = a.port_arg.unwrap_or(config.port);
    tui::run(a.store, port, a.db_path).await
}
