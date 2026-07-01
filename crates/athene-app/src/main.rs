mod app;
mod components;
mod theme;

use athene_core::{
    config::{AgentConfig, AppConfig},
    events::Engine,
    lifecycle::poller::Poller,
    store::Store,
    tmux,
    types::{Session, SessionStatus},
    BrainIndex, QueryFilters,
};
use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc, time::{SystemTime, UNIX_EPOCH}};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    headless: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Spawn a worker session (used by orchestrator agents via ATHENE_BIN)
    Spawn {
        /// Task description — passed to the agent harness
        #[arg(long, short)]
        prompt: String,
        /// Absolute path to the repository the worker should operate in
        #[arg(long, short)]
        workspace: String,
        /// Display name for the session (defaults to first four words of prompt)
        #[arg(long, short)]
        name: Option<String>,
        /// Orchestrator session ID (read from ATHENE_ORCHESTRATOR_ID if not supplied)
        #[arg(long)]
        orchestrator_id: Option<String>,
    },
    /// Knowledge base operations
    Brain {
        #[command(subcommand)]
        action: BrainAction,
    },
}

#[derive(Subcommand)]
enum BrainAction {
    /// Rebuild the knowledge index
    Index,
    /// Search entries by full-text
    Query {
        /// Search text
        text: String,
        /// Filter by entry type
        #[arg(long)]
        entry_type: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Print a single entry
    Show {
        /// Relative path of the entry (e.g. people/alice.md)
        path: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let db_path = args.db.unwrap_or_else(default_db_path);
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let store = Arc::new(Store::open(&db_path)?);

    match args.command {
        Some(Command::Spawn { prompt, workspace, name, orchestrator_id }) => {
            let config = AppConfig::load().unwrap_or_default();
            run_spawn(store, config.worker, prompt, workspace, name, orchestrator_id).await
        }
        Some(Command::Brain { action }) => {
            run_brain(action).await
        }
        None => run_tui(store, args.port, args.headless).await,
    }
}

async fn run_spawn(
    store: Arc<Store>,
    agent: AgentConfig,
    prompt: String,
    workspace: String,
    name: Option<String>,
    orchestrator_id: Option<String>,
) -> anyhow::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let id = format!("worker-{ts}");
    let name = name.unwrap_or_else(|| first_words(&prompt, 4));
    let orchestrator_id = orchestrator_id
        .or_else(|| std::env::var("ATHENE_ORCHESTRATOR_ID").ok());

    let session = Session {
        id:              id.clone(),
        orchestrator_id,
        name,
        repo:            workspace.clone(),
        status:          SessionStatus::Working,
        agent_type:      agent.harness.clone(),
        cost_usd:        0.0,
        started_at:      ts,
        pr_number:       None,
        pr_id:           None,
        workspace_path:  Some(workspace.clone()),
        pid:             None,
    };

    store.upsert_session(&session)?;
    println!("spawned {}", session.id);

    let cmd = agent.worker_cmd(&prompt);
    tmux::create_session(&id, &workspace, &cmd, &[]).await?;

    Ok(())
}

async fn run_brain(action: BrainAction) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let brain_path = config.resolved_brain_path();
    let brain = BrainIndex::open(&brain_path)?;

    match action {
        BrainAction::Index => {
            let count = brain.rebuild()?;
            println!("indexed {count} entries");
        }
        BrainAction::Query { text, entry_type, tag } => {
            let filters = QueryFilters { entry_type, tag };
            let entries = brain.query(&text, filters)?;
            for entry in &entries {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Show { path } => {
            match brain.get(&path)? {
                Some(entry) => println!("{}", serde_json::to_string_pretty(&entry)?),
                None => {
                    eprintln!("entry not found: {path}");
                    std::process::exit(1);
                }
            }
        }
    }

    Ok(())
}

async fn run_tui(store: Arc<Store>, port_arg: Option<u16>, headless: bool) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let port = port_arg.unwrap_or(config.port);
    let orchestrator_root = config.resolved_orchestrator_root();
    let orchestrator_agent = config.orchestrator.clone();
    let config_path = AppConfig::config_path().to_string_lossy().to_string();
    let brain_path = config.resolved_brain_path();

    let athene_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "athene".to_string());

    if let Err(e) = app::setup_orchestrator_root(&orchestrator_root, &athene_bin, &config_path).await {
        tracing::warn!("orchestrator root setup failed: {e}");
    }

    let brain = Arc::new(BrainIndex::open(&brain_path)?);
    let engine = Engine::new(store);
    let token = CancellationToken::new();

    let poller = Poller::new(engine.clone());
    tokio::spawn({
        let t = token.clone();
        async move { poller.start(t).await }
    });

    tokio::spawn({
        let e = engine.clone();
        let b = brain.clone();
        async move {
            if let Err(err) = athene_server::start(e, b, port).await {
                tracing::error!("server: {err}");
            }
        }
    });

    tracing::info!("athene ready on :{port}");

    if headless || !has_display() {
        tokio::signal::ctrl_c().await?;
        token.cancel();
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    let window_settings = iced::window::Settings {
        platform_specific: iced::window::settings::PlatformSpecific {
            title_hidden: true,
            titlebar_transparent: true,
            fullsize_content_view: true,
        },
        ..Default::default()
    };
    #[cfg(not(target_os = "macos"))]
    let window_settings = iced::window::Settings::default();

    iced::application("Athene", app::App::iced_update, app::App::iced_view)
        .subscription(app::App::subscription)
        .theme(app::App::theme)
        .window(window_settings)
        .run_with(move || app::App::new(engine, orchestrator_root, orchestrator_agent))?;

    token.cancel();
    Ok(())
}

fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("athene")
        .join("athene.db")
}

fn first_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join("-")
}

fn has_display() -> bool {
    #[cfg(target_os = "macos")]
    { return true; }
    #[cfg(not(target_os = "macos"))]
    { std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() }
}
