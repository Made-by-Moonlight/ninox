mod app;
mod attach;
mod app_icon;
mod components;
mod gui_log;
mod input;
mod motion;
mod nav_history;
mod prompt_agent;
mod style;
#[cfg(test)]
mod test_isolation_guard;
mod theme;
mod titlebar;

use std::ops::ControlFlow;

use ninox_ui_shared::cli::{bootstrap, dispatch, exit_wrong_ui, DaemonOutcome, UiArgs, UiRequest};

// iced builds and, when `run_with` returns, drops its own tokio runtime.
// Tokio panics if a runtime is dropped while the current thread is inside
// another runtime's `block_on` — which a plain `#[tokio::main] async fn
// main()` always is, for the whole function body. So `main` stays
// synchronous: the one tokio runtime it builds does the async setup via
// `block_on`, then sits untouched (just kept alive, never entered) while
// `run_with` runs and tears down iced's own runtime on a `block_on`-free
// stack.
fn main() -> anyhow::Result<()> {
    gui_log::init();

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;

    // ninox-gui never installs the `nx` alias — see ninox-tui's own main.rs.
    match rt.block_on(dispatch(false))? {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(UiRequest::Default(a) | UiRequest::Gui(a)) => run_gui(rt, a),
        ControlFlow::Break(UiRequest::Tui(_)) => {
            exit_wrong_ui("ninox-gui", "built-in terminal UI", "ninox-tui", Some("nx"))
        }
    }
}

fn run_gui(rt: tokio::runtime::Runtime, a: UiArgs) -> anyhow::Result<()> {
    if a.headless {
        gui_log::enable_file(&ninox_core::daemon_log::log_path(&a.db_path));
    }
    let ctx = match rt.block_on(bootstrap(a.store, a.port_arg, a.headless, a.db_path))? {
        DaemonOutcome::ActedAsDaemon => return Ok(()),
        DaemonOutcome::Ready(ctx) => ctx,
    };

    #[cfg(target_os = "macos")]
    let window_settings = iced::window::Settings {
        platform_specific: iced::window::settings::PlatformSpecific {
            title_hidden: true,
            titlebar_transparent: true,
            fullsize_content_view: true,
        },
        icon: app_icon::window_icon(),
        ..Default::default()
    };
    #[cfg(not(target_os = "macos"))]
    let window_settings = iced::window::Settings { icon: app_icon::window_icon(), ..Default::default() };
    app_icon::set_dock_icon();

    let ninox_ui_shared::cli::UiBootstrap {
        engine, orchestrator_root, brain, db_path, token, port,
    } = ctx;
    if let Some(dir) = db_path.parent() {
        gui_log::enable_file(&dir.join("gui.log"));
    }

    // Registers this process as a trusted `HumanConsoleIdentity` so the
    // Pending Requests panel's `answer_user_request` calls verify —
    // without this, every answer attempt fails verification. A
    // detached/unusual launch should degrade to "can't answer sync
    // requests" rather than refusing to start the GUI at all.
    let human_console_error = match ninox_core::human_console_auth::register_human_console(&engine.store, "ninox-gui") {
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("failed to register ninox-gui as a human console: {e}");
            Some(e.to_string())
        }
    };

    style::use_v1_generic_families();
    let mut application = iced::application("Ninox", app::App::iced_update, app::App::iced_view)
        .subscription(app::App::subscription)
        .theme(app::App::theme)
        // Native global zoom (Cmd/Ctrl +/-/0) — scales the whole UI rather
        // than resizing individual widgets. Driven by `App::zoom`.
        .scale_factor(|state| state.zoom)
        .window(window_settings);
    for font in style::BUNDLED_FONTS {
        application = application.font(*font);
    }
    application
        .default_font(style::SANS)
        .run_with(move || {
            let (mut app, task) = app::App::new(engine, orchestrator_root, brain, db_path);
            app.human_console_error = human_console_error;
            app.server.port = port;
            (app, task)
        })?;

    token.cancel();
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Regression coverage for the shutdown panic this file fixes: iced
    //! builds and drops its own tokio runtime around `run_with`, and tokio
    //! refuses to drop a runtime while the current thread is inside another
    //! runtime's `block_on`. These tests don't touch iced/winit at all —
    //! they isolate the tokio behavior `main`/`run_gui` above depend on.

    fn build_outer_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap()
    }

    // iced_winit drives its subscriptions/tasks with its own internal
    // current-thread tokio runtime (see the crash backtrace in MLOPS-5475:
    // `CachedParkThread::block_on`, not the multi-thread scheduler) — build
    // the same flavor here so these tests exercise the actual failure mode
    // rather than the unrelated "nested block_on" guard multi-thread
    // runtimes raise immediately.
    fn build_inner_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    #[test]
    fn dropping_a_runtime_outside_any_block_on_does_not_panic() {
        let outer = build_outer_runtime();
        outer.block_on(async {});

        // Mirrors `run_gui`: this happens after the outer `block_on` call
        // has already returned, not from inside it (like calling
        // `run_with` from `main`'s body, not from an async block it awaits).
        let inner = build_inner_runtime();
        inner.block_on(async {});
        drop(inner);
        drop(outer);
    }

    #[test]
    #[should_panic(expected = "Cannot drop a runtime")]
    fn dropping_a_runtime_inside_another_runtimes_block_on_panics() {
        let outer = build_outer_runtime();
        outer.block_on(async {
            // Mirrors the bug: iced never calls `.block_on()` on its own
            // runtime either — it just exists as a field that gets dropped,
            // synchronously, somewhere inside the outer runtime's `block_on`
            // call stack (`run_with` isn't `.await`ed; it's a plain blocking
            // call made from within the polled future). Merely constructing
            // and dropping a runtime value there is already enough to panic.
            let inner = build_inner_runtime();
            drop(inner);
        });
    }
}
