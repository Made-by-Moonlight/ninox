mod app;
mod components;
mod theme;

use athene_core::{config::AppConfig, events::Engine, lifecycle::poller::Poller, store::Store};
use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    headless: bool,
    #[arg(long)]
    db: Option<PathBuf>,
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = AppConfig::load().unwrap_or_default();

    let db_path = args.db.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("athene")
            .join("athene.db")
    });
    std::fs::create_dir_all(db_path.parent().unwrap())?;

    let store = Arc::new(Store::open(&db_path)?);
    let engine = Engine::new(store);
    let port = args.port.unwrap_or(config.port);
    let token = CancellationToken::new();

    let poller = Poller::new(engine.clone());
    tokio::spawn({
        let t = token.clone();
        async move { poller.start(t).await }
    });

    tokio::spawn({
        let e = engine.clone();
        async move {
            if let Err(err) = athene_server::start(e, port).await {
                tracing::error!("server: {err}");
            }
        }
    });

    tracing::info!("athene ready on :{port}");

    if args.headless || !has_display() {
        tokio::signal::ctrl_c().await?;
        token.cancel();
        return Ok(());
    }

    iced::application("Athene", app::App::iced_update, app::App::iced_view)
        .subscription(app::App::subscription)
        .theme(app::App::theme)
        .run_with(move || app::App::new(engine))?;

    token.cancel();
    Ok(())
}

fn has_display() -> bool {
    #[cfg(target_os = "macos")]
    { return true; }
    #[cfg(not(target_os = "macos"))]
    { std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() }
}
