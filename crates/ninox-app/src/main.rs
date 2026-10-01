mod app;
mod components;
mod connect;
mod fleet;
mod input;
mod models;
mod runtime_cli;
mod service;
mod spawn_util;
mod style;
mod theme;
mod tui;

use anyhow::Context as _;
use spawn_util::{
    acquire_worker_checkout_for_incarnation, repo_from_workspace, seed_worker_skills,
};
use ninox_core::{
    capabilities::Audience,
    config::{AppConfig, SendMechanism},
    events::Engine,
    github::resolve_token,
    lifecycle::{poller::Poller, repo_discovery},
    slugify,
    store::Store,
    tmux,
    types::{Session, SessionStatus},
    workers, BrainIndex, QueryFilters,
};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
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
    /// Spawn a worker session (used by orchestrator agents via NINOX_BIN)
    Spawn {
        /// Task description — passed to the agent harness
        #[arg(long, short)]
        prompt: String,
        /// Absolute path to the repository the worker should operate in
        #[arg(long, short)]
        workspace: String,
        /// Delivery contract. Defaults to PR for Git workspaces, direct otherwise.
        #[arg(long, value_enum)]
        delivery: Option<WorkerDelivery>,
        /// Display name for the session (defaults to first four words of prompt)
        #[arg(long, short)]
        name: Option<String>,
        /// Orchestrator session ID (read from NINOX_ORCHESTRATOR_ID if not supplied)
        #[arg(long)]
        orchestrator_id: Option<String>,
    },
    /// Release a finalized clean worker checkout for warm reuse.
    Release {
        /// Retained worker session ID.
        session_id: String,
        /// Owning orchestrator (read from NINOX_ORCHESTRATOR_ID if omitted).
        #[arg(long)]
        orchestrator_id: Option<String>,
    },
    /// Spawn a peer orchestrator session (used by orchestrator agents, and
    /// only ever when the user explicitly asks for another orchestrator —
    /// hence the mandatory `--user-requested`)
    SpawnOrchestrator {
        /// Display name for the orchestrator; slugified into its session ID
        #[arg(long, short)]
        name: String,
        /// Initial brief, delivered once the new orchestrator's harness is
        /// ready for input. Omit to start it empty and follow up with
        /// `ninox send`.
        #[arg(long, short)]
        prompt: Option<String>,
        /// Confirms the user asked for this orchestrator. Required — an
        /// orchestrator must never spin one up on its own initiative.
        #[arg(long)]
        user_requested: bool,
    },
    /// Clean up this orchestrator's workers: kill their sessions and remove
    /// their worktrees and hook artifacts (used by orchestrator agents).
    /// Finished workers only, unless `--force` is given.
    Reap {
        /// Worker session IDs to reap. Omit to reap every finished worker.
        session_ids: Vec<String>,
        /// Select every worker, not just the finished ones. Live workers
        /// still need `--force` to actually be reaped.
        #[arg(long)]
        all: bool,
        /// Reap selected workers even while they are still running.
        #[arg(long)]
        force: bool,
        /// Orchestrator whose workers to reap (read from
        /// NINOX_ORCHESTRATOR_ID if not supplied)
        #[arg(long)]
        orchestrator_id: Option<String>,
    },
    /// Send a text message to a session's terminal (injected as keyboard input)
    Send {
        /// Target session ID
        session_id: String,
        /// Message text to inject (Enter is sent automatically)
        message: String,
    },
    /// Ask the orchestrator to schedule additional work discovered outside
    /// this worker's task (used by workers; Ninox delivers it and the
    /// orchestrator spawns a dedicated worker)
    RequestWork {
        /// Description of the additional work
        description: String,
    },
    /// Complete this exact worker incarnation with its canonical final summary.
    Complete {
        /// Canonical final handoff delivered durably to the owning orchestrator.
        summary: String,
    },
    /// Atomically receive and acknowledge one durable worker completion.
    ReceiveCompletion {
        /// Completion ID from Ninox's delivery nudge.
        completion_id: String,
    },
    /// Knowledge base operations
    Brain {
        #[command(subcommand)]
        action: BrainAction,
    },
    /// Emit a Claude Code statusline and record cost/context usage for the
    /// session at this workspace. Invoked by Claude Code's own `statusLine`
    /// hook (see `.claude/settings.json`), not intended for direct use.
    Statusline,
    /// File-based inbox drain hooks (installed in a worker's worktree
    /// settings only when `[messaging].mechanism = "inbox"` — see
    /// `ninox_core::inbox`). Invoked by Claude Code's own Stop/
    /// UserPromptSubmit hooks, not intended for direct use.
    Inbox {
        #[command(subcommand)]
        action: InboxAction,
    },
    /// Inspect and safely finalize workers owned by this orchestrator.
    Workers {
        #[command(subcommand)]
        action: WorkersAction,
    },
    /// Register a PR for consolidated watching ([pr_watch] must be enabled).
    /// The current session ($NINOX_SESSION) becomes the watch's opener and
    /// receives merge/CI/review notifications for it.
    Open {
        /// GitHub PR URL, e.g. https://github.com/owner/repo/pull/42
        #[arg(long)]
        pr: String,
    },
    /// Remove this session's watch on a PR (other sessions' watches on the
    /// same PR are unaffected).
    Close {
        /// GitHub PR URL previously passed to `ninox open --pr`
        #[arg(long)]
        pr: String,
    },
    /// List sessions (default) or watched resources
    List {
        /// List active PR watches instead of sessions
        #[arg(long)]
        prs: bool,
        /// Emit JSON instead of the session board
        #[arg(long)]
        json: bool,
    },
    /// List the agent-facing capabilities Ninox currently offers, with each
    /// one's live enabled/disabled state (see `ninox_core::capabilities`).
    Capabilities {
        /// Show only worker-facing capabilities
        #[arg(long)]
        worker: bool,
        /// Show only orchestrator-facing capabilities
        #[arg(long)]
        orchestrator: bool,
        /// Emit JSON instead of one line per capability
        #[arg(long)]
        json: bool,
    },
    /// Register (or inspect) this orchestrator's goals/plan markdown doc,
    /// rendered live in the desktop app.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
    /// Attach your terminal to a running session (worker or orchestrator).
    /// Detach with your `[tui] prefix` then d (ptyd sessions; default C-\ on
    /// macOS, C-Space elsewhere) or tmux's detach key
    /// (default: C-b d) to return to your shell.
    Connect {
        /// Session ID (see `ninox list`)
        session_id: String,
    },
    /// Start a new orchestrator and attach your terminal to it.
    Orchestrate {
        /// Display name; slugified into the session ID
        name: String,
        /// Initial brief, delivered before attaching
        #[arg(long, short)]
        prompt: Option<String>,
        /// Print the workspace directory and a connect hint instead of attaching
        #[arg(long)]
        no_attach: bool,
    },
    /// Open the terminal UI — what bare `nx` does from a terminal.
    Tui,
    /// Durable fleets: status, ordered restore after a reboot, recovery
    /// briefings and their acknowledgement.
    Fleet {
        #[command(subcommand)]
        action: fleet::FleetAction,
    },
    /// Start the headless engine at login (launchd / systemd user unit).
    Service {
        #[command(subcommand)]
        action: service::ServiceAction,
    },
    /// Open the desktop app, even when run from a terminal.
    Gui,
    /// Report a session's activity state (working/idle/blocked) and manage
    /// worker→worker dependency edges — both rendered live in the desktop
    /// app's Workers view.
    WorkerStatus {
        #[command(subcommand)]
        action: WorkerStatusAction,
    },
    /// Run the ninox-ptyd PTY host in the foreground. Started on demand by
    /// the engine and CLI; not intended for direct use.
    #[command(hide = true)]
    Ptyd {
        /// Take over a running host's panes instead of refusing to start.
        #[arg(long)]
        takeover: bool,
        #[command(subcommand)]
        action: Option<runtime_cli::PtydAction>,
    },
    /// Inspect or attach to panes held by the ptyd host.
    Pane {
        #[command(subcommand)]
        action: runtime_cli::PaneAction,
    },
    /// Print a session's current screen (plain text unless --ansi).
    Read {
        /// Session ID (see `ninox list`)
        session_id: String,
        /// Print the last N lines, reaching into scrollback, instead of
        /// just the visible screen
        #[arg(long)]
        lines: Option<usize>,
        /// Keep colors and styles as ANSI escape sequences
        #[arg(long)]
        ansi: bool,
    },
}

#[derive(Subcommand)]
enum WorkerStatusAction {
    /// Declare this session's activity state
    Set {
        /// The state to declare
        #[arg(value_enum)]
        state: ActivityStateArg,
        /// Optional context, e.g. what you're blocked on
        #[arg(long)]
        note: Option<String>,
    },
    /// UserPromptSubmit hook entry point: marks the session working.
    /// Installed in a worker's worktree settings; not intended for direct use.
    HookPrompt,
    /// Stop hook entry point: marks the session idle (unless it declared
    /// itself blocked). Installed in a worker's worktree settings; not
    /// intended for direct use.
    HookStop,
    /// Declare that a session depends on another session
    Depend {
        /// The session (id or name) being depended on
        target: String,
        /// Why the dependency exists
        #[arg(long)]
        note: Option<String>,
        /// The depending session (id or name) — defaults to the invoking
        /// session; orchestrators use this to declare edges between workers
        #[arg(long = "for")]
        source: Option<String>,
    },
    /// Remove a previously declared dependency
    Undepend {
        /// The session (id or name) that was depended on
        target: String,
        /// The depending session (id or name) — defaults to the invoking session
        #[arg(long = "for")]
        source: Option<String>,
    },
    /// List live sessions with their activity state and dependency edges
    List {
        /// Emit JSON instead of indented text
        #[arg(long)]
        json: bool,
    },
}

/// CLI-facing subset of `ninox_core::ActivityState` — `unknown` is the
/// absence of a report, never something an agent declares.
#[derive(Clone, Copy, clap::ValueEnum)]
enum ActivityStateArg {
    Working,
    Idle,
    Blocked,
}

impl From<ActivityStateArg> for ninox_core::ActivityState {
    fn from(a: ActivityStateArg) -> Self {
        match a {
            ActivityStateArg::Working => Self::Working,
            ActivityStateArg::Idle    => Self::Idle,
            ActivityStateArg::Blocked => Self::Blocked,
        }
    }
}

#[derive(Subcommand)]
enum PlanAction {
    /// Register (or re-register) a markdown file as this orchestrator's
    /// plan doc. Idempotent — safe to call again after editing the file's
    /// path, or to re-run with the same path.
    Register {
        /// Path to the markdown file to track.
        file: PathBuf,
    },
    /// Stop tracking this orchestrator's plan doc, if any.
    Unregister,
    /// Print this orchestrator's current plan-doc registration as JSON.
    Show,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WorkerDelivery {
    Pr,
    Direct,
}

#[derive(Subcommand)]
enum WorkersAction {
    /// List every worker owned by this orchestrator as JSON.
    List,
    /// Inspect one owned worker as JSON.
    Inspect { session_id: String },
    /// Stop and retain one or more owned workers. Workspaces are never deleted.
    Finalize {
        #[arg(required = true)]
        session_ids: Vec<String>,
    },
}

#[derive(Subcommand)]
enum InboxAction {
    /// Stop hook entry point: drains pending inbox messages into a
    /// `{"decision":"block","reason":...}` response so Claude Code
    /// continues instead of actually stopping. Prints nothing when nothing
    /// is pending, letting Claude Code stop normally.
    DrainStop,
    /// UserPromptSubmit hook entry point: drains pending inbox messages
    /// into `hookSpecificOutput.additionalContext`, never touching the
    /// submitted prompt itself.
    DrainPrompt,
}

#[derive(Subcommand)]
enum BrainAction {
    /// Rebuild the knowledge index
    Index,
    /// Pull and push all changes to the brain's remote (requires a
    /// configured remote — see `ninox brain remote set`)
    Sync,
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
    /// Write a Markdown entry and index it immediately — replaces the
    /// write-then-`ninox brain index` two-step flow so an entry is
    /// queryable as soon as it's added, not whenever someone next
    /// remembers to reindex.
    Add {
        /// Relative path of the entry under the brain root (e.g. repos/ninox.md)
        path: String,
        /// Markdown content, including frontmatter. Reads stdin if omitted.
        #[arg(long)]
        content: Option<String>,
    },
    /// Package the brain's Markdown source into a portable .tar.gz archive
    /// (excludes the derived .index.db)
    Export {
        /// Output path for the archive, e.g. brain.tar.gz
        output: PathBuf,
    },
    /// Extract a `ninox brain export` archive and rebuild the index
    Import {
        /// Path to the archive to import
        input: PathBuf,
        /// Import into this brain path instead of the resolved default
        #[arg(long)]
        into: Option<PathBuf>,
        /// Overwrite entries that already exist in the target brain
        #[arg(long)]
        force: bool,
    },
    /// Scan known repo workspaces and write their location, remote, and
    /// purpose into `repos/`, plus mechanically detectable relationships
    /// (shared worktrees, shared remote owner) into `relationships/`.
    /// Re-running updates existing entries in place rather than duplicating
    /// them.
    DiscoverRepos {
        /// Workspace paths to scan. Defaults to every workspace_path
        /// recorded in the session store (i.e. every repo a worker has ever
        /// been spawned into) when none are given.
        paths: Vec<PathBuf>,
    },
    /// Manage this brain's remote backing store
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// Attach an S3-compatible remote and run an initial sync
    Set {
        /// Remote URL, e.g. s3://team-brains/main
        url: String,
        /// Custom endpoint for S3-compatible stores (R2, MinIO)
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        region: Option<String>,
        /// Freshness-check cache TTL in seconds (0 = check every lookup)
        #[arg(long, default_value_t = 0)]
        ttl: u64,
    },
    /// Show remote, last sync, pending pushes, and live conflicts (offline)
    Status,
    /// Detach from the remote; the local copy stays a normal brain
    Unset,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let mut command = args.command;

    // `spawn` must reject worker callers before the shared CLI startup below:
    // that path writes tmux config and wrappers, creates the DB parent, and
    // opens the store. Role is stamped by Ninox when the runtime launches;
    // `NINOX_CALLER_TYPE` keeps sessions launched by older Ninox versions safe.
    if matches!(command, Some(Command::Spawn { .. })) {
        reject_recursive_worker_spawn(
            std::env::var(spawn_util::EXECUTION_ROLE_ENV).ok().as_deref(),
            std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
        )?;
    }

    // Fires on every assistant turn (event-driven) or every `refreshInterval`
    // seconds for every session Ninox spawns — must stay fast and never
    // trigger the tmux-config/wrapper-hook/self-shim setup below, none of
    // which this subcommand needs.
    if matches!(command, Some(Command::Statusline)) {
        run_statusline(args.db.unwrap_or_else(default_db_path));
        return Ok(());
    }

    // The PTY host, pane bridges and screen reads are runtime plumbing
    // (`ninox read` is agent-invoked mid-session); none of them touch the
    // store or need the setup below.
    if let Some(Command::Ptyd { .. } | Command::Pane { .. } | Command::Read { .. }) = &command {
        return match command.unwrap() {
            Command::Ptyd { takeover, action } => runtime_cli::run_ptyd(takeover, action).await,
            Command::Pane { action } => runtime_cli::run_pane(action).await,
            Command::Read { session_id, lines, ansi } => runtime_cli::run_read(&session_id, lines, ansi).await,
            _ => unreachable!(),
        };
    }

    // Fires on every Stop/UserPromptSubmit turn of every worker with inbox
    // messaging enabled — same "stay fast, skip the heavy setup" reasoning
    // as Statusline above; this subcommand needs none of it either.
    if let Some(Command::Inbox { action }) = command {
        run_inbox(action, args.db.clone().unwrap_or_else(default_db_path));
        return Ok(());
    }

    if matches!(
        command,
        Some(Command::Complete { .. } | Command::ReceiveCompletion { .. })
    ) {
        let db_path = args.db.unwrap_or_else(default_db_path);
        if let Some(parent) = db_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let store = Arc::new(Store::open(db_path)?);
        return match command.expect("completion command matched above") {
            Command::Complete { summary } => run_complete(store, &summary).await,
            Command::ReceiveCompletion { completion_id } => {
                run_receive_completion(store, &completion_id).await
            }
            _ => unreachable!("completion commands were matched above"),
        };
    }

    if let Some(Command::Workers { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::process::exit(run_workers_cli(action, db_path).await);
    }

    // Fires on every `ninox open --pr` / `ninox close --pr` / `ninox list` /
    // `ninox connect` invocation — agents call these directly (via
    // NINOX_BIN) on every PR they open or finish with, and connect needs to
    // land the user in tmux without delay, so this must stay fast and skip
    // the tmux-config/wrapper-hook/self-shim setup below the same way
    // Statusline/Inbox do above; it needs only `Store::open` (+ tmux for
    // Connect). `matches!`
    // (rather than `if let ... = command`) is used as the guard because the
    // variants carry different fields — binding them here would move
    // `command` on a branch that falls through to the `match command` below
    // instead of returning, which the borrow checker rejects. `..` patterns
    // bind nothing, so the guard itself never moves `command`; the actual
    // move happens via the full match below, on a path that unconditionally
    // returns.
    if matches!(
        command,
        Some(Command::Open { .. } | Command::Close { .. } | Command::List { .. } | Command::Connect { .. })
    ) {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::fs::create_dir_all(db_path.parent().unwrap())?;
        let store = Store::open(&db_path)?;
        let cmd = command.unwrap();

        // Handle session board listing (prs: false) separately; doesn't need config/env
        if let Command::List { prs: false, json } = cmd {
            warn_if_daemon_down(args.port).await;
            println!("{}", run_list_sessions(&store, json)?);
            return Ok(());
        }

        // Terminal attach only needs Store + tmux, like List{prs:false} above.
        if let Command::Connect { session_id } = cmd {
            warn_if_daemon_down(args.port).await;
            connect::run_connect(&store, &session_id).await?;
            return Ok(());
        }

        // PR watch operations (Open, Close, List{prs:true}) share config setup
        let enabled = AppConfig::load().unwrap_or_default().pr_watch.enabled;
        let opener = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
        let action = match cmd {
            Command::Open { pr }  => PrWatchCliAction::Open { pr },
            Command::Close { pr } => PrWatchCliAction::Close { pr },
            Command::List { prs: true, .. } => PrWatchCliAction::List,
            _ => unreachable!(),
        };
        println!("{}", run_pr_watch(&store, enabled, action, opener)?);
        return Ok(());
    }

    // Agents run this to discover what ninox can do for them (the bootstrap
    // line in every worker footer and the orchestrator AGENTS.md points
    // here), so it must stay as cheap as Statusline/Inbox/Open above: it
    // reads the config and nothing else — no store, no tmux, no wrappers.
    //
    // Unlike Open/Close/List above, this arm *can* bind its fields directly:
    // all three are `bool` (Copy), so the pattern copies them out instead of
    // moving `command`, and the arm returns unconditionally anyway.
    if let Some(Command::Capabilities { worker, orchestrator, json }) = command {
        let filter = match (worker, orchestrator) {
            (true, false) => Some(Audience::Worker),
            (false, true) => Some(Audience::Orchestrator),
            // Neither flag, or both — no filter, list everything.
            _ => None,
        };
        let config = AppConfig::load().unwrap_or_default();
        println!("{}", run_capabilities(&config, filter, json));
        return Ok(());
    }

    if let Some(Command::Plan { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::process::exit(run_plan_cli(action, db_path).await);
    }

    // The hook verbs fire on every UserPromptSubmit/Stop turn of every
    // worker (same cadence as Inbox above); `set`/`depend` are agent-invoked
    // mid-session. All need only `Store::open` — keep them out of the heavy
    // setup below.
    if let Some(Command::WorkerStatus { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        match action {
            // No fallible prelude before the hook verbs — their never-fail
            // contract covers the whole path (a missing/uncreatable data
            // dir surfaces as a Store::open failure, swallowed inside
            // run_worker_status_hook).
            WorkerStatusAction::HookPrompt => {
                run_worker_status_hook(db_path, WorkerStatusHookKind::Prompt, now);
                return Ok(());
            }
            WorkerStatusAction::HookStop => {
                run_worker_status_hook(db_path, WorkerStatusHookKind::Stop, now);
                return Ok(());
            }
            action => {
                std::fs::create_dir_all(db_path.parent().unwrap())?;
                let store = Store::open(&db_path)?;
                let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
                let cwd = std::env::current_dir().ok();
                let self_session = ninox_core::worker_status::resolve_session_id(
                    &store, env_session.as_deref(), cwd.as_deref(),
                )?;
                let parsed = match action {
                    WorkerStatusAction::Set { state, note } =>
                        WorkerStatusCliAction::Set { state: state.into(), note },
                    WorkerStatusAction::Depend { target, note, source } =>
                        WorkerStatusCliAction::Depend { target, note, source },
                    WorkerStatusAction::Undepend { target, source } =>
                        WorkerStatusCliAction::Undepend { target, source },
                    WorkerStatusAction::List { json } =>
                        WorkerStatusCliAction::List { json },
                    WorkerStatusAction::HookPrompt | WorkerStatusAction::HookStop =>
                        unreachable!("hook verbs return above"),
                };
                println!("{}", run_worker_status(&store, parsed, self_session, now)?);
                return Ok(());
            }
        }
    }

    // `fleet ack`/`status`/`brief` are agent-invoked; only a real restore
    // needs the wrapper/shim setup below (it launches sessions).
    if let Some(Command::Fleet { action }) = command {
        if fleet::is_lightweight(&action) {
            let db_path = args.db.unwrap_or_else(default_db_path);
            std::fs::create_dir_all(db_path.parent().unwrap())?;
            let store = Arc::new(Store::open(&db_path)?);
            return fleet::run_cli(action, store, AppConfig::load().unwrap_or_default()).await;
        }
        command = Some(Command::Fleet { action });
    }
    if let Some(Command::Service { action }) = command {
        return service::run(action);
    }

    // ptyd users needn't have tmux installed; a legacy tmux session still
    // gets the config written lazily by tmux.rs's own server bootstrap.
    if ninox_core::runtime::configured_backend() == ninox_core::runtime::Backend::Tmux {
        if let Err(e) = tmux::write_server_config() {
            eprintln!("failed to write tmux config: {e}");
        }
    }

    if let Err(e) = ninox_core::hooks::install_wrappers() {
        tracing::warn!("failed to install wrapper hooks: {e}");
    }
    if let Ok(exe) = ninox_core::hooks::canonical_exe() {
        if let Err(e) = ninox_core::hooks::install_self_shim(&exe) {
            tracing::warn!("failed to install ninox self-shim: {e}");
        }
        if let Err(e) = ninox_core::hooks::install_nx_alias(&exe, std::env::var_os("PATH").as_deref()) {
            tracing::debug!("nx alias not installed: {e}");
        }
    }

    let db_path = args.db.unwrap_or_else(default_db_path);
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let store = Arc::new(Store::open(&db_path)?);

    match command {
        Some(Command::Spawn { prompt, workspace, delivery, name, orchestrator_id }) => {
            let config = AppConfig::load().unwrap_or_default();
            run_spawn(store, config, prompt, workspace, delivery, name, orchestrator_id).await
        }
        Some(Command::Release { session_id, orchestrator_id }) => {
            let rust_cache = AppConfig::load().unwrap_or_default().rust_cache;
            run_release(store, &session_id, orchestrator_id, rust_cache).await
        }
        Some(Command::SpawnOrchestrator { name, prompt, user_requested }) => {
            let config = AppConfig::load().unwrap_or_default();
            run_spawn_orchestrator(store, config, name, prompt, user_requested).await
        }
        Some(Command::Orchestrate { name, prompt, no_attach }) => {
            let config = AppConfig::load().unwrap_or_default();
            let port = args.port.unwrap_or(config.port);
            run_orchestrate(store, config, port, db_path, name, prompt, no_attach).await
        }
        Some(Command::Tui) => {
            let config = AppConfig::load().unwrap_or_default();
            let port = args.port.unwrap_or(config.port);
            tui::run(store, port, db_path).await
        }
        Some(Command::Reap { session_ids, all, force, orchestrator_id }) => {
            run_reap(store, session_ids, all, force, orchestrator_id).await
        }
        Some(Command::Send { session_id, message }) => {
            let config = AppConfig::load().unwrap_or_default();
            let sessions_dir = std::env::var("NINOX_DATA_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(AppConfig::sessions_dir);
            ninox_core::messaging::deliver_message(
                &store, &sessions_dir, &session_id, &message, config.send_mechanism(),
            )
            .await
        }
        Some(Command::RequestWork { description }) => {
            run_request_work(&store, &description)
        }
        Some(Command::Fleet { action }) => {
            fleet::run_cli(action, store, AppConfig::load().unwrap_or_default()).await
        }
        Some(Command::Service { .. }) => {
            unreachable!("Service short-circuits and returns earlier in main()")
        }
        Some(Command::Complete { .. } | Command::ReceiveCompletion { .. }) => {
            unreachable!("completion commands short-circuit and return earlier in main()")
        }
        Some(Command::Brain { action }) => {
            run_brain(action, store).await
        }
        Some(Command::Statusline) => {
            run_statusline(db_path);
            Ok(())
        }
        Some(Command::Inbox { action }) => {
            run_inbox(action, db_path);
            Ok(())
        }
        // Workers always short-circuit-returns above before reaching this
        // match; unreachable in practice, but the compiler can't see that
        // across the early `return`.
        Some(Command::Workers { .. }) => {
            unreachable!("Workers short-circuits and returns earlier in main()")
        }
        // Open/Close/List always short-circuit-return above before reaching
        // this match; unreachable in practice, but the compiler can't see
        // that across the early `return`.
        Some(Command::Open { .. } | Command::Close { .. } | Command::List { .. }) => {
            unreachable!("Open/Close/List short-circuit and return earlier in main()")
        }
        // Same story as Open/Close/List: handled by the early return above.
        Some(Command::Capabilities { .. }) => {
            unreachable!("Capabilities short-circuits and returns earlier in main()")
        }
        Some(Command::Plan { .. }) => {
            unreachable!("Plan short-circuits and returns earlier in main()")
        }
        // Same story as Open/Close/List: handled by the early return above.
        Some(Command::Connect { .. }) => {
            unreachable!("Connect short-circuits and returns earlier in main()")
        }
        Some(Command::WorkerStatus { .. }) => {
            unreachable!("WorkerStatus short-circuits and returns earlier in main()")
        }
        Some(Command::Ptyd { .. } | Command::Pane { .. } | Command::Read { .. }) => {
            unreachable!("runtime commands short-circuit and return earlier in main()")
        }
        Some(Command::Gui) => run_tui(store, args.port, args.headless, db_path, true).await,
        None => run_tui(store, args.port, args.headless, db_path, false).await,
    }
}

async fn run_complete(store: Arc<Store>, summary: &str) -> anyhow::Result<()> {
    let session_id = std::env::var("NINOX_SESSION").ok();
    let incarnation_id = std::env::var("NINOX_WORKER_INCARNATION").ok();
    let orchestrator_id = std::env::var("NINOX_ORCHESTRATOR_ID").ok();
    let execution_role = std::env::var(spawn_util::EXECUTION_ROLE_ENV).ok();
    let caller_type = std::env::var("NINOX_CALLER_TYPE").ok();
    let runtime = tmux::current_private_pane_identity().await?;
    let legacy_runtime = session_id
        .as_deref()
        .map(|session_id| store.legacy_worker_runtime(session_id))
        .transpose()?
        .flatten();
    let runtime_incarnation = match (runtime.as_ref(), legacy_runtime.as_ref()) {
        (Some(runtime), None) => {
            tmux::private_session_env(&runtime.physical_tmux_name, "NINOX_WORKER_INCARNATION")
                .await?
        }
        _ => None,
    };
    let worker = workers::authorize_worker_completion(
        &store,
        session_id.as_deref(),
        incarnation_id.as_deref(),
        orchestrator_id.as_deref(),
        (execution_role.as_deref(), caller_type.as_deref()),
        runtime.as_ref(),
        runtime_incarnation.as_deref(),
    )?;
    let orchestrator_id = worker
        .orchestrator_id
        .as_deref()
        .context("worker has no owning orchestrator")?;
    let result = store.complete_worker_incarnation(
        &worker.session_id,
        &worker.incarnation_id,
        orchestrator_id,
        summary,
        ninox_core::lifecycle::poller::now_millis(),
    )?;
    let completion = match result {
        ninox_core::types::WorkerCompletionIntent::Completed(completion)
        | ninox_core::types::WorkerCompletionIntent::AlreadyCompleted(completion) => completion,
    };
    println!(
        "completion {} durably queued for {}",
        completion.completion_id, completion.orchestrator_id
    );
    Ok(())
}

async fn run_receive_completion(store: Arc<Store>, completion_id: &str) -> anyhow::Result<()> {
    let runtime = tmux::current_private_pane_identity().await?;
    let orchestrator_id = workers::authorize_orchestrator(
        &store,
        std::env::var("NINOX_ORCHESTRATOR_ID").ok().as_deref(),
        std::env::var(spawn_util::EXECUTION_ROLE_ENV).ok().as_deref(),
        std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
        runtime.as_ref(),
    )?;
    match store.acknowledge_worker_completion(
        &orchestrator_id,
        completion_id,
        ninox_core::lifecycle::poller::now_millis(),
    )? {
        ninox_core::types::WorkerCompletionReceipt::Delivered(completion) => {
            println!("{}", completion.summary);
        }
        ninox_core::types::WorkerCompletionReceipt::AlreadyAcknowledged => {
            println!("completion already acknowledged; canonical summary not repeated");
        }
    }
    Ok(())
}

const WORKERS_CLI_SCHEMA_VERSION: u32 = 1;

fn workers_error(code: &str, message: impl Into<String>, retryable: bool) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "message": message.into(),
        "retryable": retryable,
    })
}

fn classify_worker_error(error: &anyhow::Error) -> serde_json::Value {
    let message = error.to_string();
    if message.contains("not found") {
        workers_error("not_found", message, false)
    } else if message.contains("runtime start") || message.contains("in-progress") {
        workers_error("worker_spawning", message, true)
    } else if message.contains("cleanup was already claimed") {
        workers_error("cleanup_claimed", message, false)
    } else {
        workers_error("operation_failed", message, false)
    }
}

fn emit_workers_envelope(ok: bool, data: serde_json::Value, error: serde_json::Value) {
    println!(
        "{}",
        serde_json::json!({
            "schema_version": WORKERS_CLI_SCHEMA_VERSION,
            "command": "workers",
            "ok": ok,
            "data": data,
            "error": error,
        })
    );
}

const PLAN_CLI_SCHEMA_VERSION: u32 = 1;

fn plan_error(code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "code": code, "message": message.into() })
}

fn emit_plan_envelope(ok: bool, data: serde_json::Value, error: serde_json::Value) {
    println!(
        "{}",
        serde_json::json!({
            "schema_version": PLAN_CLI_SCHEMA_VERSION,
            "command": "plan",
            "ok": ok,
            "data": data,
            "error": error,
        })
    );
}

async fn run_workers_cli(action: WorkersAction, db_path: PathBuf) -> i32 {
    if let Some(parent) = db_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        if let Err(error) = std::fs::create_dir_all(parent) {
            emit_workers_envelope(
                false,
                serde_json::Value::Null,
                workers_error("database_error", error.to_string(), false),
            );
            return 4;
        }
    }
    let store = match Store::open(db_path) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            emit_workers_envelope(
                false,
                serde_json::Value::Null,
                workers_error("database_error", error.to_string(), false),
            );
            return 4;
        }
    };
    let runtime = match tmux::current_private_pane_identity().await {
        Ok(runtime) => runtime,
        Err(error) => {
            emit_workers_envelope(
                false,
                serde_json::Value::Null,
                workers_error("authorization_failed", error.to_string(), false),
            );
            return 2;
        }
    };
    let orchestrator_id = match workers::authorize_orchestrator(
        &store,
        std::env::var("NINOX_ORCHESTRATOR_ID").ok().as_deref(),
        std::env::var(spawn_util::EXECUTION_ROLE_ENV).ok().as_deref(),
        std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
        runtime.as_ref(),
    ) {
        Ok(orchestrator_id) => orchestrator_id,
        Err(error) => {
            emit_workers_envelope(
                false,
                serde_json::Value::Null,
                workers_error("authorization_failed", error.to_string(), false),
            );
            return 2;
        }
    };

    match action {
        WorkersAction::List => match workers::list_owned_workers(&store, &orchestrator_id).await {
            Ok(owned) => {
                emit_workers_envelope(true, serde_json::json!(owned), serde_json::Value::Null);
                0
            }
            Err(error) => {
                emit_workers_envelope(
                    false,
                    serde_json::Value::Null,
                    classify_worker_error(&error),
                );
                1
            }
        },
        WorkersAction::Inspect { session_id } => {
            match workers::inspect_owned_worker(&store, &orchestrator_id, &session_id).await {
                Ok(worker) => {
                    emit_workers_envelope(
                        true,
                        serde_json::json!(worker),
                        serde_json::Value::Null,
                    );
                    0
                }
                Err(error) => {
                    let classified = classify_worker_error(&error);
                    let exit = if classified["code"] == "not_found" {
                        3
                    } else {
                        1
                    };
                    emit_workers_envelope(false, serde_json::Value::Null, classified);
                    exit
                }
            }
        }
        WorkersAction::Finalize { session_ids } => {
            let mut failed = false;
            let mut results = Vec::with_capacity(session_ids.len());
            for session_id in session_ids {
                match workers::finalize_owned_worker(
                    store.clone(),
                    &orchestrator_id,
                    &session_id,
                )
                .await
                {
                    Ok(result) => results.push(serde_json::json!({
                        "session_id": session_id,
                        "ok": true,
                        "result": result,
                        "error": null,
                    })),
                    Err(error) => {
                        failed = true;
                        results.push(serde_json::json!({
                            "session_id": session_id,
                            "ok": false,
                            "result": null,
                            "error": classify_worker_error(&error),
                        }));
                    }
                }
            }
            emit_workers_envelope(
                !failed,
                serde_json::json!({ "results": results }),
                if failed {
                    workers_error(
                        "partial_failure",
                        "one or more workers could not be finalized",
                        false,
                    )
                } else {
                    serde_json::Value::Null
                },
            );
            if failed {
                5
            } else {
                0
            }
        }
    }
}

async fn run_plan_cli(action: PlanAction, db_path: PathBuf) -> i32 {
    if let Some(parent) = db_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        if let Err(error) = std::fs::create_dir_all(parent) {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("database_error", error.to_string()));
            return 4;
        }
    }
    let store = match Store::open(db_path) {
        Ok(store) => store,
        Err(error) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("database_error", error.to_string()));
            return 4;
        }
    };
    let runtime = match ninox_core::runtime::current_private_pane_identity().await {
        Ok(runtime) => runtime,
        Err(error) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("authorization_failed", error.to_string()));
            return 2;
        }
    };
    let orchestrator_id = match ninox_core::orchestrator_auth::authorize_orchestrator(
        &store,
        std::env::var("NINOX_ORCHESTRATOR_ID").ok().as_deref(),
        std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
        runtime.as_ref(),
    ) {
        Ok(orchestrator_id) => orchestrator_id,
        Err(error) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("authorization_failed", error.to_string()));
            return 2;
        }
    };

    match action {
        PlanAction::Register { file } => {
            let resolved = std::fs::canonicalize(&file)
                .unwrap_or(file)
                .to_string_lossy()
                .into_owned();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            match store.register_orchestrator_plan(&orchestrator_id, &resolved, now) {
                Ok(()) => {
                    emit_plan_envelope(true, serde_json::json!({ "file_path": resolved }), serde_json::Value::Null);
                    0
                }
                Err(error) => {
                    emit_plan_envelope(false, serde_json::Value::Null, plan_error("operation_failed", error.to_string()));
                    1
                }
            }
        }
        PlanAction::Unregister => match store.unregister_orchestrator_plan(&orchestrator_id) {
            Ok(removed) => {
                emit_plan_envelope(true, serde_json::json!({ "removed": removed }), serde_json::Value::Null);
                0
            }
            Err(error) => {
                emit_plan_envelope(false, serde_json::Value::Null, plan_error("operation_failed", error.to_string()));
                1
            }
        },
        PlanAction::Show => match store.get_orchestrator_plan(&orchestrator_id) {
            Ok(Some(plan)) => {
                let exists = std::path::Path::new(&plan.file_path).is_file();
                emit_plan_envelope(
                    true,
                    serde_json::json!({
                        "file_path": plan.file_path,
                        "registered_at": plan.registered_at,
                        "updated_at": plan.updated_at,
                        "exists": exists,
                    }),
                    serde_json::Value::Null,
                );
                0
            }
            Ok(None) => {
                emit_plan_envelope(false, serde_json::Value::Null, plan_error("not_found", "no plan doc registered for this orchestrator"));
                3
            }
            Err(error) => {
                emit_plan_envelope(false, serde_json::Value::Null, plan_error("operation_failed", error.to_string()));
                1
            }
        },
    }
}

async fn run_spawn(
    store: Arc<Store>,
    config: AppConfig,
    prompt: String,
    workspace: String,
    requested_delivery: Option<WorkerDelivery>,
    name: Option<String>,
    orchestrator_id: Option<String>,
) -> anyhow::Result<()> {
    reject_recursive_worker_spawn(
        std::env::var(spawn_util::EXECUTION_ROLE_ENV).ok().as_deref(),
        std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
    )?;
    let agent = config.worker.clone();
    // Refuse worker-incapable harnesses BEFORE any side effect (worktree
    // creation, session upsert) — bailing after the upsert would leave a
    // permanent ghost "Working" session with no pid and no tmux session
    // for poll_pids to reap.
    let registry = config.registry();
    if registry.spec(&agent.harness).worker_args.is_none() {
        anyhow::bail!(
            "harness '{}' has no verified worker mode (no worker_args in its spec) — \
             pick a worker-capable harness in Settings or add worker_args under \
             [harnesses.{}] in config.toml",
            agent.harness, agent.harness,
        );
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // Use the supplied name (slugified) as the session ID so orchestrators can
    // address workers directly by a human-readable name (e.g. "ath-123-auth").
    // Falls back to a timestamp-based ID when no name is provided.
    let id = name.as_deref()
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("worker-{ts}"));
    // Refuse to reuse an existing session's id BEFORE any side effect
    // (worktree creation, upsert). The upsert below is a full-row write
    // keyed on `id`: reusing the name of a live worker — most dangerously a
    // merged worker kept alive for validation ([auto_reap] off), which now
    // squats its slug until reaped — would clobber its row (wiping
    // merged_at/pr_number), and the ensuing duplicate-id tmux-create failure
    // would mark the hijacked record Terminated, feeding it to the sweep.
    // Mirrors the TUI spawn modal's collision guard (see `app.rs`).
    if store.get_session(&id)?.is_some() {
        anyhow::bail!(
            "a session named '{id}' already exists — pick another name, or reap the \
             existing worker first (`ninox reap {id}`, add --force if it's still running)"
        );
    }
    let display_name = name.unwrap_or_else(|| first_words(&prompt, 4));
    // The fleet card summary: the first line of the raw task prompt, before
    // the worker-context footer is appended below.
    let summary = first_line(&prompt, 140);
    let orchestrator_id = orchestrator_id
        .or_else(|| std::env::var("NINOX_ORCHESTRATOR_ID").ok());
    let delivery = resolve_worker_delivery(requested_delivery, &workspace);
    anyhow::ensure!(
        store.get_session(&id)?.is_none(),
        "a session named {id} already exists — pick another name"
    );
    let repository =
        ninox_core::worktree::RepositoryIdentity::resolve(std::path::Path::new(&workspace)).ok();
    let checkout_backed = repository.is_some();
    let checkout_cap = repository.as_ref().map_or_else(
        || config.validated_worker_checkout_default_cap(),
        |repository| config.validated_worker_checkout_cap_for_repository(&repository.top_level),
    )?;
    let capacity_workspace = repository
        .as_ref()
        .map(|repository| repository.top_level.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.clone());
    let pending = Session {
        id: id.clone(),
        orchestrator_id: orchestrator_id.clone(),
        name: display_name.clone(),
        repo: repo_from_workspace(&workspace).unwrap_or_default(),
        status: SessionStatus::Spawning,
        agent_type: agent.harness.clone(),
        cost_usd: 0.0,
        started_at: ts,
        pr_number: None,
        pr_id: None,
        workspace_path: Some(workspace.clone()),
        pid: None,
        model: agent.model.clone(),
        context_tokens: None,
        catalogue_path: std::env::var("NINOX_BRAIN").ok().filter(|s| !s.is_empty()),
        context_used_pct: None,
        context_total_tokens: None,
        context_window_size: None,
        claude_session_id: None,
        summary: summary.clone(),
        terminal_at: None,
        gate_status: None,
        merged_at: None,
        activity: ninox_core::types::ActivityState::Unknown,
        activity_note: None,
        activity_since: None,
    };
    anyhow::ensure!(
        store.insert_spawning_session(&pending)?,
        "session {id} already exists"
    );
    let incarnation = match store.prepare_worker_incarnation(
        &id,
        orchestrator_id.as_deref(),
        ts,
        &capacity_workspace,
        checkout_backed,
        checkout_cap,
    ) {
        Ok(incarnation) => incarnation,
        Err(error) if error.to_string().contains("repository checkout pool saturated") => {
            let _ = store.delete_spawning_session_snapshot(&id, ts, None);
            let candidates =
                store.checkout_worker_candidates_for_repository(&capacity_workspace)?;
            anyhow::bail!(
                "{error}. {}",
                release_candidate_guidance(orchestrator_id.as_deref(), &candidates)
            )
        }
        Err(error) => {
            let _ = store.delete_spawning_session_snapshot(&id, ts, None);
            return Err(error);
        }
    };

    let repositories_root = config.resolved_repositories_root();
    let checkout = match acquire_worker_checkout_for_incarnation(
        store.clone(),
        &workspace,
        &id,
        &incarnation.incarnation_id,
        repositories_root.as_deref(),
        &config.resolved_worktree_root(),
        config.send_mechanism() == SendMechanism::Inbox,
    )
    .await
    {
        Ok(checkout) => Some(checkout),
        Err(error) => {
            let rolled_back =
                rollback_worker_incarnation(store.clone(), &id, &incarnation.incarnation_id, None)
                    .await;
            if rolled_back {
                let _ = store.terminalize_spawning_session_snapshot(&id, ts, &workspace);
                // The task brief is what a fleet restore re-briefs this
                // worker with if its conversation can't be resumed — record
                // it even though the worker never reached a live runtime.
                if let Err(e) = store.record_spawn_facts(&id, &prompt, None) {
                    tracing::warn!("record spawn facts for {id}: {e}");
                }
            }
            return Err(error);
        }
    };
    let effective_workspace = checkout
        .as_ref()
        .map_or_else(|| workspace.clone(), |checkout| checkout.workspace.clone());
    let canonical_source_workspace = checkout
        .as_ref()
        .map_or_else(|| workspace.clone(), |checkout| checkout.source_workspace.clone());
    if !store.update_spawning_session_workspace_snapshot(
        &id,
        ts,
        &effective_workspace,
    )? {
        rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        anyhow::bail!("worker session changed before checkout binding");
    }
    if !store.bind_worker_incarnation(
            &id,
            &incarnation.incarnation_id,
            &canonical_source_workspace,
            &effective_workspace,
            checkout
                .as_ref()
                .and_then(|checkout| checkout.pooled_lease.as_ref())
                .map(|lease| lease.lease_id.as_str()),
        )?
    {
        rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        anyhow::bail!("worker incarnation changed before checkout binding");
    }

    // Without a trust entry the headless worker blocks forever on Claude
    // Code's "do you trust this folder?" dialog instead of taking the prompt.
    if let Err(e) = ninox_core::trust::seed_workspace_trust(std::path::Path::new(&effective_workspace)) {
        tracing::warn!("failed to seed claude workspace trust for {effective_workspace}: {e}");
    }
    // Durable, on-disk counterpart to the `worker_context_footer` below: the
    // footer is lost after context compaction, but a seeded SKILL.md
    // survives for the life of the worktree. Which skills land (and whether
    // gated ones like watch-pr are among them) is decided by the capability
    // registry against this config — see `seed_worker_skills`.
    seed_worker_skills(&effective_workspace, &config).await;

    // Derive the GitHub repo slug from the workspace's git remote so that
    // poll_github can call the GitHub API with the correct owner/repo.
    let repo = repo_from_workspace(&workspace).unwrap_or_default();

    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    let ninox_bin = ninox_core::config::AppConfig::ninox_bin_dir();
    let ninox_bin_str = ninox_bin.display().to_string();

    let orch_id_env = orchestrator_id.as_deref().unwrap_or("").to_string();

    // The task brief and branch are what a fleet restore re-briefs this
    // worker with if its conversation can't be resumed.
    let task_brief = prompt.clone();
    // Append worker context so every agent knows its session ID, delivery
    // contract, orchestrator ID, and how to communicate back when done.
    let mut effective_prompt = match worker_prompt_for_canonical_workspace(
        &prompt,
        &canonical_source_workspace,
        &effective_workspace,
        delivery,
    ) {
        Ok(prompt) => prompt,
        Err(error) => {
            let rolled_back = rollback_worker_incarnation(
                store.clone(),
                &id,
                &incarnation.incarnation_id,
                checkout.as_ref(),
            )
            .await;
            if rolled_back {
                let _ = store.terminalize_spawning_session_snapshot(&id, ts, &workspace);
            }
            return Err(error.context("prepare worker prompt"));
        }
    };
    if !orch_id_env.is_empty() {
        effective_prompt.push_str(&worker_context_footer(&id, &orch_id_env, delivery, config.pr_watch.enabled));
    }

    let claude_session_id = ninox_core::harness::new_claude_session_id();

    let session = Session {
        id:              id.clone(),
        orchestrator_id,
        name:            display_name,
        repo,
        status:          SessionStatus::Working,
        agent_type:      agent.harness.clone(),
        cost_usd:        0.0,
        started_at:      ts,
        pr_number:       None,
        pr_id:           None,
        workspace_path:  Some(effective_workspace.clone()),
        pid:             None,
        model:           agent.model.clone(),
        context_tokens:  None,
        // The catalogue this worker thinks with — `NINOX_BRAIN` is
        // forwarded from the orchestrator's own environment (see the env
        // block below), so record the same value for Re-file.
        catalogue_path:  std::env::var("NINOX_BRAIN").ok().filter(|s| !s.is_empty()),
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: Some(claude_session_id.clone()),
        summary,
        terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
    };

    if let Err(error) = store.upsert_session(&session) {
        rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        return Err(error);
    }
    // The task brief and branch are what a fleet restore re-briefs this
    // worker with if its conversation can't be resumed.
    let branch = ninox_core::fleet::probe::current_branch(std::path::Path::new(&effective_workspace));
    if let Err(e) = store.record_spawn_facts(&id, &task_brief, branch.as_deref()) {
        tracing::warn!("record spawn facts for {id}: {e}");
    }

    // Prepend the ninox bin dir inside the shell command rather than via tmux
    // -e PATH=..., because the login shell (-l) sources rc files that may
    // re-prepend Homebrew or nvm directories, pushing our wrapper behind the
    // real `gh`. By exporting PATH here we win the race after rc files run.
    let Some(cmd_base) = registry.worker_cmd(&agent, &effective_prompt, &claude_session_id) else {
        let rolled_back = rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        if rolled_back {
            let _ =
                store.update_session_status_snapshot(&id, ts, SessionStatus::Terminated);
        }
        anyhow::bail!(
            "harness '{}' lost its worker capability during spawn",
            agent.harness
        );
    };
    let cmd = format!(
        "export PATH='{}':\"$PATH\"; {}",
        ninox_bin_str.replace('\'', "'\\''"),
        cmd_base,
    );

    // A fresh tmux session does *not* inherit the caller's ambient
    // environment — only vars explicitly passed via `-e` (see
    // `tmux::create_session`) or already tracked in the server's global
    // environment (seeded once, from whichever process first started the
    // server). Since `run_spawn` is normally invoked (as `ninox spawn`) from
    // *inside* an orchestrator's own tmux session — one that itself was
    // launched with NINOX_BRAIN/NINOX_CONFIG via `-e` by
    // `spawn_util::spawn_interactive_session` — those vars are present in
    // this process's own env and must be forwarded explicitly, or the
    // spawned worker loses brain/config access entirely.
    let ninox_brain_env = std::env::var("NINOX_BRAIN").ok();
    let ninox_config_env = std::env::var("NINOX_CONFIG").ok();

    let rust_cache_env =
        spawn_util::configured_worker_rust_cache_env(&config.rust_cache, &effective_workspace)
            .await;
    let mut env_vec = worker_env_vars(
        &id,
        &incarnation.incarnation_id,
        &sessions_dir_str,
        &orch_id_env,
        ninox_brain_env.as_deref(),
        ninox_config_env.as_deref(),
    );
    env_vec.extend(
        rust_cache_env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    );
    let runtime_claim = store
        .claim_worker_runtime_start(&id, &incarnation.incarnation_id)?
        .context("worker incarnation changed before runtime launch")?;
    // The session was already upserted as Working above; if tmux refuses
    // the spawn (e.g. the workspace dir doesn't exist — create_session
    // rejects that rather than letting the pane silently start in $HOME),
    // mark it Terminated before bailing. A pid-less Working ghost is
    // invisible to poll_pids and would linger until the next app restart's
    // reconciliation.
    if let Err(e) = ninox_core::runtime::create_session(config.runtime.backend, &id, &effective_workspace, &cmd, &env_vec).await {
        let _ = store.abort_worker_runtime_start(
            &id,
            &incarnation.incarnation_id,
            &runtime_claim.claim_id,
        );
        let rolled_back = rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        if rolled_back {
            let _ =
                store.update_session_status_snapshot(&id, ts, SessionStatus::Terminated);
        }
        return Err(e);
    }
    if !store.complete_worker_runtime_start(
        &id,
        &incarnation.incarnation_id,
        &runtime_claim.claim_id,
    )? {
        let _ = ninox_core::runtime::kill_session(&id).await;
        anyhow::bail!("worker incarnation changed before runtime launch completed");
    }
    println!("spawned {}", session.id);

    Ok(())
}

async fn rollback_worker_incarnation(
    store: Arc<Store>,
    session_id: &str,
    incarnation_id: &str,
    checkout: Option<&spawn_util::WorkerCheckout>,
) -> bool {
    spawn_util::rollback_worker_incarnation_checkout(store, session_id, incarnation_id, checkout)
        .await
}

fn reject_recursive_worker_spawn(
    execution_role: Option<&str>,
    legacy_caller_type: Option<&str>,
) -> anyhow::Result<()> {
    // Deliberate worker fan-out stays default-denied until Ninox can issue a
    // parent-authorized, non-inheriting capability and durably record the
    // parent worker lineage. An ambient opt-out would recreate this bug.
    match execution_role.or(legacy_caller_type) {
        Some(spawn_util::WORKER_EXECUTION_ROLE) => anyhow::bail!(
            "worker sessions cannot spawn workers; ask the orchestrator to delegate this task"
        ),
        Some(spawn_util::ORCHESTRATOR_EXECUTION_ROLE) | None => Ok(()),
        Some(role) => anyhow::bail!(
            "unrecognized {}={role:?}; refusing to spawn a worker",
            spawn_util::EXECUTION_ROLE_ENV,
        ),
    }
}

fn release_candidate_guidance(
    orchestrator_id: Option<&str>,
    candidates: &[ninox_core::types::WorkerIncarnation],
) -> String {
    if candidates.is_empty() {
        return "no finalized clean recycle candidate exists; finish or explicitly remove an existing worker"
            .to_string();
    }
    let scope = orchestrator_id
        .map(|id| format!(" --orchestrator-id {id}"))
        .unwrap_or_default();
    let candidates = candidates
        .iter()
        .map(|worker| {
            if worker.orchestrator_id.as_deref() != orchestrator_id {
                return format!(
                    "{} is owned by {} and {:?} at {}",
                    worker.session_id,
                    worker.orchestrator_id.as_deref().unwrap_or("a standalone caller"),
                    worker.state,
                    worker.workspace_path,
                );
            }
            if matches!(
                worker.state,
                ninox_core::types::WorkerIncarnationState::Retained
            ) {
                format!(
                    "`ninox release {}{scope}` ({})",
                    worker.session_id, worker.workspace_path
                )
            } else {
                format!(
                    "{} is {:?} at {} (finish it or `ninox reap {} --force{scope}`)",
                    worker.session_id, worker.state, worker.workspace_path, worker.session_id
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("existing checkout-backed workers (recycle one before retrying): {candidates}")
}

fn resolve_worker_delivery(
    requested: Option<WorkerDelivery>,
    workspace: &str,
) -> WorkerDelivery {
    requested.unwrap_or_else(|| {
        if ninox_core::worktree::RepositoryIdentity::resolve(std::path::Path::new(workspace)).is_ok()
        {
            WorkerDelivery::Pr
        } else {
            WorkerDelivery::Direct
        }
    })
}

/// Preserve the caller's task verbatim and append the allocated workspace as
/// authoritative metadata.
#[cfg(test)]
fn worker_prompt_for_workspace(
    prompt: &str,
    source_workspace: &str,
    worker_workspace: &str,
    delivery: WorkerDelivery,
) -> anyhow::Result<String> {
    let canonical_source = ninox_core::worktree::RepositoryIdentity::resolve(
        std::path::Path::new(source_workspace),
    )
    .map(|identity| identity.top_level)
    .or_else(|_| std::path::Path::new(source_workspace).canonicalize())
    .ok()
    .and_then(|path| path.to_str().map(str::to_string))
    .unwrap_or_else(|| source_workspace.to_string());
    worker_prompt_for_canonical_workspace(
        prompt,
        &canonical_source,
        worker_workspace,
        delivery,
    )
}

fn worker_prompt_for_canonical_workspace(
    prompt: &str,
    canonical_source: &str,
    worker_workspace: &str,
    delivery: WorkerDelivery,
) -> anyhow::Result<String> {
    let source_note = if canonical_source == worker_workspace {
        String::new()
    } else {
        " The original source checkout is repository context only; \
         do not read, write, or run Git commands there."
            .to_string()
    };
    let workspace_contract = match delivery {
        WorkerDelivery::Pr =>
            "Perform every repository read, write, and Git command inside it.",
        WorkerDelivery::Direct =>
            "Perform all task work and write artifacts or direct changes inside it.",
    };
    Ok(format!(
        "{prompt}\n\n---\n\
         **Ninox workspace:** `{worker_workspace}` is the authoritative workspace. \
         {workspace_contract}{source_note}"
    ))
}

fn worker_context_footer(
    id: &str,
    orch_id: &str,
    delivery: WorkerDelivery,
    pr_watch_enabled: bool,
) -> String {
    match delivery {
        WorkerDelivery::Pr if !orch_id.is_empty() => {
            pr_worker_context_footer(id, orch_id, pr_watch_enabled)
        }
        WorkerDelivery::Pr => format!(
            "\n\n---\n\
             Ninox session `{id}`\n\n\
             **Goal:** complete the task and open a pull request.\n\n\
             **Scope:** one worker, one task, one pull request. Do not perform \
             additional out-of-scope work or open another PR; return it in the \
             final handoff.\n\n\
             Return a blocker-or-completion handoff to the caller.",
        ),
        WorkerDelivery::Direct => format!(
            "\n\n---\n\
             Ninox session `{id}`{orchestrator} · direct delivery\n\n\
             **Goal:** complete the task through validated artifacts or direct changes \
             in the authoritative workspace.\n\n\
             **Delivery:** Do not create branches, remotes, or commits; do not push or \
             open pull requests. \
             Validate the delivered artifacts or direct changes before handoff.\n\n\
             {scope}\n\
             {handoff}",
            orchestrator = if orch_id.is_empty() {
                String::new()
            } else {
                format!(" · orchestrator `{orch_id}`")
            },
            scope = if orch_id.is_empty() {
                "**Scope:** one worker, one task. Do not perform additional out-of-scope \
                 work; return it in the final handoff."
                    .to_string()
            } else {
                "**Scope:** one worker, one task. If you discover additional work outside \
                 this task, do not do it — hand it to the orchestrator instead:\n\
                 ```bash\n\
                 ninox request-work \"<description of the additional work>\"\n\
                 ```"
                    .to_string()
            },
            handoff = if orch_id.is_empty() {
                "Return a blocker-or-completion handoff to the caller, then stop after \
                 reporting that you are blocked or the direct delivery is complete."
                    .to_string()
            } else {
                format!(
                    "Report a blocker with:\n\
                     ```bash\n\
                     ninox send {orch_id} \"<blocked and needs a decision>\"\n\
                     ```\n\
                     When complete, durably hand off the canonical final summary with:\n\
                     ```bash\n\
                     ninox complete \"<artifacts/direct changes and validation>\"\n\
                     ```\n\
                     Stop after reporting a blocker or completing the direct delivery."
                )
            },
        ),
    }
}

/// The context footer appended to every worker's task prompt: its own
/// session id, its orchestrator's id, the channels back to the orchestrator,
/// and the one-worker-one-PR scope rule. Always ends with the `ninox
/// capabilities` bootstrap line so a worker can discover the rest of what
/// ninox offers it without that list being restated here. When
/// `pr_watch_enabled` (mirrors `AppConfig.pr_watch.enabled`), also tells the
/// worker to register PR watches instead of polling `gh` directly.
fn pr_worker_context_footer(id: &str, orch_id: &str, pr_watch_enabled: bool) -> String {
    let mut footer = format!(
        "\n\n---\n\
         Ninox session `{id}` · orchestrator `{orch_id}`\n\n\
         **Goal:** complete the task and open a pull request.\n\n\
         **Scope:** one worker, one task, one pull request. If you discover \
         additional work outside this task, do not do it and do not open \
         another PR — hand it to the orchestrator instead:\n\
         ```bash\n\
         ninox request-work \"<description of the additional work>\"\n\
         ```\n\
         To message the orchestrator (e.g. when stuck or when the PR is open):\n\
         ```bash\n\
         ninox send {orch_id} \"<your message>\"\n\
         ```\n\
         When the PR is open and the task is done, durably hand off the canonical \
         final summary with:\n\
         ```bash\n\
         ninox complete \"<root cause/design, commit, PR URL, and validation>\"\n\
         ```\n\
         Use `ninox send` for blockers or progress, not successful completion.",
    );
    if pr_watch_enabled {
        footer.push_str(
            "\n\nInstead of polling gh for PR/CI status, register watches: \
             `ninox open --pr <url>` (notifications are delivered to you), \
             `ninox close --pr <url>` when done.",
        );
    }
    // `--worker`: unfiltered output would also list the orchestrator-only
    // skills, including spawn-worker telling the reader to delegate work
    // instead of doing it — exactly the wrong instruction for a worker.
    footer.push_str(
        "\n\nRun `ninox capabilities --worker` to list what ninox can currently do.",
    );
    footer
}

/// The tmux env for a spawned worker: always the session id + data dir, plus
/// whichever of orchestrator id / brain path / config path are actually
/// present (an empty `orch_id` or `None` env value is omitted rather than
/// forwarded as an empty string).
fn worker_env_vars<'a>(
    id: &'a str,
    incarnation_id: &'a str,
    sessions_dir: &'a str,
    orch_id: &'a str,
    ninox_brain: Option<&'a str>,
    ninox_config: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut env_vec: Vec<(&str, &str)> = vec![
        ("NINOX_SESSION", id),
        ("NINOX_WORKER_INCARNATION", incarnation_id),
        (spawn_util::EXECUTION_ROLE_ENV, spawn_util::WORKER_EXECUTION_ROLE),
        ("NINOX_CALLER_TYPE", "worker"),
        ("NINOX_DATA_DIR", sessions_dir),
        // Same reasoning as the app spawn path — see
        // `ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV`. Workers
        // are the usual target of orchestrator messages, so a worker that
        // came up without a socket is exactly the case that would quietly
        // never use the configured mechanism.
        (ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV, "1"),
    ];
    if !orch_id.is_empty() {
        env_vec.push(("NINOX_ORCHESTRATOR_ID", orch_id));
    }
    if let Some(v) = ninox_brain {
        env_vec.push(("NINOX_BRAIN", v));
    }
    if let Some(v) = ninox_config {
        env_vec.push(("NINOX_CONFIG", v));
    }
    env_vec
}


/// Decide which orchestrator a `ninox reap` acts on, refusing the calls that
/// must never be allowed to reap anything.
///
/// `NINOX_CALLER_TYPE` is `orchestrator` on every path that launches one
/// (fresh spawn, Re-file, Resume — see `app::refile_plan`/`resume_plan`) and
/// on none that launches a worker, which is what separates "an orchestrator
/// is asking" from "a worker is asking". Workers DO carry
/// `NINOX_ORCHESTRATOR_ID`, pointing at their parent, so that variable alone
/// would happily let one reap its own siblings.
///
/// `--orchestrator-id` exists for deliberate out-of-session calls (a human at
/// a terminal, a script), so it skips the ambient-env lookup — but it is NOT
/// an escape hatch from the guard. Inside any session that isn't an
/// orchestrator it is refused too. Otherwise a worker could pass its own
/// parent's id and reap the whole sibling fleet — including itself, whose
/// `kill_session` would take down the very process running the reap, leaving
/// a `Working` row with no worktree that neither `poll_pids` nor the retention
/// sweep will ever clean up.
///
/// `caller_is_orchestrator` is resolved from the STORE (is `NINOX_SESSION` an
/// orchestrator row?), with the env var as a fallback only when there's no
/// session id to look up. Env alone is not trustworthy for a destructive
/// command: `NINOX_CALLER_TYPE` lives in the agent's own shell, so a worker
/// could simply export `NINOX_CALLER_TYPE=orchestrator` and reap its fleet.
/// This does not defeat *determined* evasion — stripping `NINOX_SESSION` and
/// passing `--orchestrator-id` looks identical to a human at a terminal — but
/// it does mean the guard can't be undone by setting one variable.
/// Whether the process calling `ninox reap` is itself an orchestrator.
///
/// Resolved from the STORE whenever there's a session id to look up: an
/// orchestrator's session id is exactly an `orchestrators` row id, and a
/// worker's never is. `NINOX_CALLER_TYPE` is only consulted when there is no
/// `NINOX_SESSION` at all (a plain shell), because for a destructive command
/// that variable is not evidence — it lives in the agent's own environment,
/// so a worker could export `NINOX_CALLER_TYPE=orchestrator` and reap its
/// whole fleet, including itself.
fn caller_is_orchestrator(
    env_session:     Option<&str>,
    orchestrator_ids: &[&str],
    env_caller_type: Option<&str>,
) -> bool {
    match env_session {
        Some(sid) => orchestrator_ids.contains(&sid),
        None      => env_caller_type == Some("orchestrator"),
    }
}

fn resolve_release_orchestrator(
    explicit:               Option<String>,
    env_orch_id:            Option<String>,
    caller_is_orchestrator: bool,
    env_session:            Option<String>,
) -> anyhow::Result<String> {
    let is_orchestrator = caller_is_orchestrator;
    let in_a_session    = env_session.is_some() || env_orch_id.is_some();
    if in_a_session && !is_orchestrator {
        anyhow::bail!(
            "`ninox release` is an orchestrator command — a worker cannot release its \
             siblings' checkouts, with or without --orchestrator-id."
        );
    }
    explicit
        .or(env_orch_id)
        .ok_or_else(|| anyhow::anyhow!(
            "NINOX_ORCHESTRATOR_ID is not set — `ninox release` runs inside an \
             orchestrator session, or pass --orchestrator-id explicitly"
        ))
}

fn resolve_reap_orchestrator(
    explicit:               Option<String>,
    env_orch_id:            Option<String>,
    caller_is_orchestrator: bool,
    env_session:            Option<String>,
) -> anyhow::Result<String> {
    let is_orchestrator = caller_is_orchestrator;
    let in_a_session    = env_session.is_some() || env_orch_id.is_some();
    if in_a_session && !is_orchestrator {
        anyhow::bail!(
            "`ninox reap` is an orchestrator command — a worker cannot reap its \
             siblings, with or without --orchestrator-id. Ask your orchestrator \
             to clean up instead (it will see your session finish on its own)."
        );
    }
    explicit
        .or(env_orch_id)
        .ok_or_else(|| anyhow::anyhow!(
            "NINOX_ORCHESTRATOR_ID is not set — `ninox reap` runs inside an \
             orchestrator session, or pass --orchestrator-id explicitly"
        ))
}

/// `ninox release` — hand a finalized, clean worker checkout back to the
/// pool for warm reuse by a later worker on the same repository.
async fn run_release(
    store: Arc<Store>,
    session_id: &str,
    orchestrator_id: Option<String>,
    rust_cache: ninox_core::config::RustCacheConfig,
) -> anyhow::Result<()> {
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let env_session = env("NINOX_SESSION");
    let caller_session = env_session.clone();
    let orchestrators = store.list_orchestrators()?;
    let orchestrator_ids: Vec<&str> =
        orchestrators.iter().map(|orchestrator| orchestrator.id.as_str()).collect();
    let caller = caller_is_orchestrator(
        env_session.as_deref(),
        &orchestrator_ids,
        env("NINOX_CALLER_TYPE").as_deref(),
    );
    let session = store
        .get_session(session_id)?
        .with_context(|| format!("no worker named {session_id}"))?;
    let ambient_orchestrator = env("NINOX_ORCHESTRATOR_ID");
    let owner = if let Some(owner) = session.orchestrator_id.as_deref() {
        let resolved = resolve_release_orchestrator(
            orchestrator_id,
            ambient_orchestrator,
            caller,
            env_session.clone(),
        )?;
        anyhow::ensure!(
            owner == resolved,
            "worker {session_id} is not owned by orchestrator {resolved}"
        );
        Some(resolved)
    } else {
        validate_standalone_release_scope(
            session_id,
            orchestrator_id.as_deref(),
            ambient_orchestrator.as_deref(),
            env_session.as_deref(),
        )?;
        None
    };
    let worker = store
        .current_worker_incarnation(session_id)?
        .with_context(|| format!("worker {session_id} has no durable checkout capability"))?;
    anyhow::ensure!(
        worker.orchestrator_id.as_deref() == owner.as_deref(),
        "worker checkout ownership does not match its session owner"
    );
    let _release_lock = lock_worker_release(&store, &worker)?;
    let worker = store
        .current_worker_incarnation(session_id)?
        .with_context(|| format!("worker {session_id} release completed concurrently"))?;
    anyhow::ensure!(
        !store.worker_runtime_claimed(session_id)?,
        "worker {session_id} runtime start is already in progress"
    );
    let claim = if matches!(
        worker.state,
        ninox_core::types::WorkerIncarnationState::ReleaseClaimed
    ) {
        worker
    } else {
        anyhow::ensure!(
            matches!(
                worker.state,
                ninox_core::types::WorkerIncarnationState::Retained
            ),
            "worker {session_id} is not finalized and retained"
        );
        store
            .claim_worker_release(session_id, &worker.incarnation_id)?
            .context("worker release lost its exact incarnation/lease claim")?
    };
    if let Err(error) = spawn_util::stop_exact_worker_runtime(
        session_id,
        &claim.incarnation_id,
        caller_session.as_deref(),
    )
    .await
    {
        let _ = store.abort_worker_claim(
            session_id,
            &claim.incarnation_id,
            ninox_core::types::WorkerIncarnationState::ReleaseClaimed,
            ninox_core::types::WorkerIncarnationState::Retained,
        );
        return Err(error);
    }
    let result =
        release_retained_worker_checkout(store.clone(), &claim, rust_cache.prune_on_release).await;
    match settle_worker_release(&store, &claim, result) {
        Ok(()) => {
            println!("released {session_id}; preserved branch/ref");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn settle_worker_release(
    store: &Store,
    claim: &ninox_core::types::WorkerIncarnation,
    result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match result {
        Ok(()) => {
            anyhow::ensure!(
                store.complete_worker_claim(
                    &claim.session_id,
                    &claim.incarnation_id,
                    ninox_core::types::WorkerIncarnationState::ReleaseClaimed,
                )?,
                "worker release completion lost its exact incarnation claim"
            );
            Ok(())
        }
        Err(error) => {
            let restored = store.abort_worker_claim(
                &claim.session_id,
                &claim.incarnation_id,
                ninox_core::types::WorkerIncarnationState::ReleaseClaimed,
                ninox_core::types::WorkerIncarnationState::Retained,
            )
            .with_context(|| {
                format!("restore retained state after worker release failed: {error:#}")
            })?;
            anyhow::ensure!(
                restored,
                "worker release failed ({error:#}) and its exact retained state could not be restored"
            );
            Err(error)
        }
    }
}

fn validate_standalone_release_scope(
    session_id: &str,
    requested_orchestrator: Option<&str>,
    ambient_orchestrator: Option<&str>,
    caller_session: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        requested_orchestrator.is_none() && ambient_orchestrator.is_none(),
        "standalone worker {session_id} has no orchestrator owner"
    );
    anyhow::ensure!(
        caller_session.is_none_or(|caller| caller == session_id),
        "standalone worker {session_id} cannot be released from another session"
    );
    Ok(())
}

fn lock_worker_release(
    store: &Store,
    worker: &ninox_core::types::WorkerIncarnation,
) -> anyhow::Result<Option<std::fs::File>> {
    let record = store
        .pooled_checkout_by_session(&worker.session_id)?
        .or(store.pooled_checkout_by_path(std::path::Path::new(
            &worker.workspace_path,
        ))?);
    let Some(record) = record else {
        return Ok(None);
    };
    if !record.path.exists() {
        return Ok(None);
    }
    Ok(Some(
        ninox_core::worktree::PooledWorktree::from_record(&record)?.lock_identity()?,
    ))
}

async fn release_retained_worker_checkout(
    store: Arc<Store>,
    worker: &ninox_core::types::WorkerIncarnation,
    prune_cargo_on_release: bool,
) -> anyhow::Result<()> {
    let record = match store.pooled_checkout_by_session(&worker.session_id)? {
        Some(record) => record,
        None => match store.pooled_checkout_by_path(std::path::Path::new(
            &worker.workspace_path,
        ))? {
            Some(record) if matches!(record.state, ninox_core::types::PooledCheckoutState::Free) => {
                return Ok(());
            }
            None if matches!(
                worker.state,
                ninox_core::types::WorkerIncarnationState::ReleaseClaimed
            ) && !std::path::Path::new(&worker.workspace_path).exists() =>
            {
                return Ok(());
            }
            _ => anyhow::bail!("retained worker has no active reusable pooled checkout"),
        },
    };
    anyhow::ensure!(
        record.owner_incarnation_id.as_deref() == Some(worker.incarnation_id.as_str())
            && record.lease_id.as_deref() == worker.lease_id.as_deref(),
        "retained checkout capability is stale or ambiguous"
    );
    let lease_id = worker.lease_id.clone().context("retained worker has no lease")?;
    if !record.path.exists() {
        anyhow::ensure!(
            store.remove_missing_pooled_checkout_for_incarnation(
                &record.path,
                &worker.session_id,
                &worker.incarnation_id,
                &lease_id,
            )?,
            "missing retained checkout changed before reconciliation"
        );
        return Ok(());
    }
    let branch = record.branch.clone().context("retained checkout has no branch")?;
    let path = record.path.clone();
    let session_id = worker.session_id.clone();
    let incarnation_id = worker.incarnation_id.clone();
    tokio::task::spawn_blocking(move || {
        let pooled = ninox_core::worktree::PooledWorktree::from_record(&record)?;
        pooled.ensure_recyclable(&branch)?;
        if prune_cargo_on_release {
            let report = ninox_core::rust_cache::prune_cargo_outputs(&pooled)
                .context("prune checkout-local Cargo outputs before release")?;
            tracing::info!(
                path = %path.display(),
                removed_directories = report.removed_directories,
                removed_metadata_files = report.removed_metadata_files,
                "pruned checkout-local Cargo outputs"
            );
        }
        pooled.release_recyclable(&branch)?;
        anyhow::ensure!(
            store.release_pooled_checkout_for_incarnation(
                &path,
                &session_id,
                &incarnation_id,
                &lease_id,
            )?,
            "pooled checkout lease changed after safe detach"
        );
        Ok(())
    })
    .await
    .context("worker release task panicked")?
}

/// `ninox reap` — clean up the calling orchestrator's workers: kill each
/// one's session and reclaim its worktree and hook artifacts. The store
/// record survives in a terminal state so the fleet board keeps the card for
/// the retention window and `sweep_retired_sessions` stays the only thing
/// that deletes a session row (see `Engine::reap_workers`).
async fn run_reap(
    store:           Arc<Store>,
    session_ids:     Vec<String>,
    all:             bool,
    force:           bool,
    orchestrator_id: Option<String>,
) -> anyhow::Result<()> {
    use ninox_core::events::{ReapOutcome, ReapSelection};

    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let env_session = env("NINOX_SESSION");
    let orchestrators = store.list_orchestrators()?;
    let orch_ids: Vec<&str> = orchestrators.iter().map(|o| o.id.as_str()).collect();
    let caller_is_orchestrator = caller_is_orchestrator(
        env_session.as_deref(), &orch_ids, env("NINOX_CALLER_TYPE").as_deref(),
    );
    let orch_id = resolve_reap_orchestrator(
        orchestrator_id,
        env("NINOX_ORCHESTRATOR_ID"),
        caller_is_orchestrator,
        env_session,
    )?;
    // A typo'd id would otherwise "succeed" with `nothing to reap`, since
    // `sessions_by_orchestrator` can't tell an unknown orchestrator from one
    // with no workers.
    if !orchestrators.iter().any(|o| o.id == orch_id) {
        anyhow::bail!("no orchestrator named {orch_id} — check the id");
    }

    let selection = if !session_ids.is_empty() {
        ReapSelection::Ids(&session_ids)
    } else if all {
        ReapSelection::All
    } else {
        ReapSelection::Finished
    };

    let engine = Engine::new(store);
    let outcomes = engine.reap_workers(&orch_id, selection, force).await?;

    if outcomes.is_empty() {
        println!("nothing to reap — {orch_id} has no finished workers");
        println!("(still-running and interrupted-but-resumable workers are never reaped by default)");
        return Ok(());
    }
    for (id, outcome) in &outcomes {
        println!("{}", reap_report_line(id, *outcome));
    }
    if outcomes.iter().any(|(_, o)| matches!(
        o,
        ReapOutcome::SkippedLive | ReapOutcome::SkippedLiveMerged | ReapOutcome::SkippedResumable,
    )) {
        println!("\nWorkers that were still running or still resumable were left alone — re-run with --force to reap them too.");
    }
    // The retry has to name the ids AND `--force`: the row is still whatever
    // non-terminal status it had, so a bare `ninox reap` (finished workers
    // only) would never select it again — and nothing else would either,
    // since a CLI-spawned worker has no `pid` for `poll_pids` to notice and
    // the retention sweep ignores non-terminal rows. Telling the caller to
    // "re-run reap" without that would leave a permanent live ghost for a
    // worker whose session and worktree are already gone.
    let unrecorded: Vec<&str> = outcomes.iter()
        .filter(|(_, o)| matches!(o, ReapOutcome::CleanedButNotRecorded))
        .map(|(id, _)| id.as_str())
        .collect();
    if !unrecorded.is_empty() {
        anyhow::bail!(
            "cleaned up but could NOT update the record for: {ids}. Their sessions and \
             worktrees are gone while the store still shows them live, and nothing will \
             reconcile that on its own. Once the store is writable, run:\n  \
             ninox reap {ids_space} --force",
            ids       = unrecorded.join(", "),
            ids_space = unrecorded.join(" "),
        );
    }
    // Only an explicitly named id can come back NotFound, and that means the
    // caller asked for something that isn't theirs (or no longer exists) —
    // a failure, not a quiet no-op.
    let missing: Vec<&str> = outcomes.iter()
        .filter(|(_, o)| matches!(o, ReapOutcome::NotFound))
        .map(|(id, _)| id.as_str())
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "not workers of {orch_id}: {} — they may already have been purged, or belong to another orchestrator",
            missing.join(", "),
        );
    }
    Ok(())
}

/// One human-readable line per reap outcome.
fn reap_report_line(id: &str, outcome: ninox_core::events::ReapOutcome) -> String {
    use ninox_core::events::ReapOutcome;
    match outcome {
        ReapOutcome::Reaped          => format!("reaped {id}"),
        ReapOutcome::ReapedLive      => format!("reaped {id} (was still running — killed)"),
        ReapOutcome::ReapedLiveMerged => format!("reaped {id} (PR merged, kept alive for validation)"),
        ReapOutcome::ReapedResumable => format!("reaped {id} (was interrupted — no longer resumable)"),
        ReapOutcome::SkippedLive     => format!("skipped {id} — still running (use --force)"),
        ReapOutcome::SkippedLiveMerged => {
            format!("skipped {id} — PR merged, kept alive for validation; reap with --force when done")
        }
        ReapOutcome::SkippedResumable => {
            format!("skipped {id} — interrupted but resumable (use --force to give that up)")
        }
        ReapOutcome::NotFound        => format!("skipped {id} — not one of your workers"),
        ReapOutcome::CleanedButNotRecorded => {
            format!("reaped {id} BUT could not record it — the session and worktree are gone, the record is not; see the log")
        }
    }
}

/// `ninox spawn-orchestrator` — stand up a peer orchestrator session.
///
/// Mirrors the app's own Spawn-modal orchestrator path (`app.rs`,
/// `SpawnKind::Orchestrator`): a workspace under the orchestrator root, an
/// `Orchestrator` row plus its session row, and the caller-type env that
/// makes the new session behave as an orchestrator. Like `run_spawn`, it
/// creates the tmux session directly rather than going through
/// `spawn_interactive_session` — this is a short-lived CLI process with no
/// UI to stream a PTY into, and the app adopts the new session on its next
/// store poll.
async fn run_spawn_orchestrator(
    store:          Arc<Store>,
    config:         AppConfig,
    name:           String,
    prompt:         Option<String>,
    user_requested: bool,
) -> anyhow::Result<()> {
    if !user_requested {
        anyhow::bail!(
            "refusing to spawn an orchestrator without --user-requested. \
             Orchestrators are spawned only when the user explicitly asks for \
             one — for work you decided to do yourself, spawn a worker \
             (`ninox spawn`) instead."
        );
    }

    let want_brief = prompt.is_some();
    let spawned = spawn_orchestrator_common(&store, &config, &name, prompt).await?;
    println!("spawned orchestrator {}", spawned.id);
    if !want_brief {
        println!("send it a brief with: ninox send {} \"<your message>\"", spawned.id);
    }
    Ok(())
}

pub(crate) struct SpawnedOrchestrator {
    pub id:        String,
    pub workspace: String,
}

async fn run_orchestrate(
    store:     Arc<Store>,
    config:    AppConfig,
    port:      u16,
    db_path:   PathBuf,
    name:      String,
    prompt:    Option<String>,
    no_attach: bool,
) -> anyhow::Result<()> {
    // Sessions outlive this command; make sure the poller/services do too.
    if let Ok(exe) = std::env::current_exe() {
        if let ninox_core::daemon::DaemonStatus::Failed(e) =
            ninox_core::daemon::ensure_daemon(port, &exe, &db_path).await
        {
            eprintln!("warning: background services not running ({e}) — statuses may go stale");
        }
    }
    let spawned = spawn_orchestrator_common(&store, &config, &name, prompt).await?;
    println!("spawned orchestrator {}", spawned.id);
    if no_attach {
        println!("dir: {}", spawned.workspace);
        println!("connect with: ninox connect {}", spawned.id);
        return Ok(());
    }
    connect::exec_attach(ninox_core::runtime::attach_args(&spawned.id).await)
}

pub(crate) async fn spawn_orchestrator_common(
    store:  &Store,
    config: &AppConfig,
    name:   &str,
    prompt: Option<String>,
) -> anyhow::Result<SpawnedOrchestrator> {
    let id = slugify(name);
    if id.is_empty() {
        anyhow::bail!("--name must contain at least one alphanumeric character");
    }
    // Same hazard the app's modal guards: a duplicate id would upsert over an
    // existing record, then fail the tmux create and mark the hijacked
    // session Terminated.
    if store.get_session(&id)?.is_some()
        || store.list_orchestrators()?.iter().any(|o| o.id == id)
    {
        anyhow::bail!("a session named {id} already exists — pick another name");
    }

    let agent = config.orchestrator.clone();
    let ninox_bin = ninox_core::hooks::canonical_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ninox".to_string());
    let config_path = AppConfig::config_path().to_string_lossy().to_string();

    // The root carries AGENTS.md/CLAUDE.md, the skills, and the subagent
    // blocker, which every orchestrator session inherits. Normally seeded at
    // app startup, but a spawn must not depend on the app having run first.
    let root = config.resolved_orchestrator_root();
    if let Err(e) =
        ninox_core::orchestrator_root::setup_orchestrator_root(&root, config, &ninox_bin, &config_path).await
    {
        tracing::warn!("orchestrator root setup failed: {e}");
    }
    let ws = root.join(&id);
    tokio::fs::create_dir_all(&ws).await?;
    // Without a trust entry the headless session blocks forever on Claude
    // Code's "do you trust this folder?" dialog instead of taking the brief.
    if let Err(e) = ninox_core::trust::seed_workspace_trust(&ws) {
        tracing::warn!("failed to seed claude workspace trust for {}: {e}", ws.display());
    }
    let ws_str = ws.to_string_lossy().to_string();

    // The new orchestrator thinks with the same brain as its spawner when
    // there is one (NINOX_BRAIN is set inside a session), falling back to the
    // configured default for an out-of-session call.
    let catalogue_path = std::env::var("NINOX_BRAIN").ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| config.resolved_brain_path().to_string_lossy().to_string());

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let claude_session_id = ninox_core::harness::new_claude_session_id();

    store.upsert_orchestrator(&ninox_core::types::Orchestrator {
        id:         id.clone(),
        name:       name.to_string(),
        created_at: ts,
    })?;
    let session = Session {
        id:              id.clone(),
        orchestrator_id: None,
        name:            name.to_string(),
        repo:            String::new(),
        status:          SessionStatus::Working,
        agent_type:      agent.harness.clone(),
        cost_usd:        0.0,
        started_at:      ts,
        pr_number:       None,
        pr_id:           None,
        workspace_path:  Some(ws_str.clone()),
        pid:             None,
        model:           agent.model.clone(),
        context_tokens:  None,
        catalogue_path:  Some(catalogue_path.clone()),
        context_used_pct: None, context_total_tokens: None, context_window_size: None,
        claude_session_id: Some(claude_session_id.clone()),
        summary:         None,
        terminal_at:     None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
    };
    store.upsert_session(&session)?;

    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    // Same PATH-prepend reasoning as `run_spawn`: rc files re-order PATH, so
    // exporting inside the launch command is what puts our shims first.
    let bin_dir = ninox_core::config::AppConfig::ninox_bin_dir().display().to_string();
    let cmd = format!(
        "export PATH='{}':\"$PATH\"; {}",
        bin_dir.replace('\'', "'\\''"),
        config.registry().interactive_cmd(&agent, &claude_session_id),
    );
    let env = orchestrator_env_vars(
        &ninox_bin, &config_path, &catalogue_path, &id, &sessions_dir_str,
    );

    if let Err(e) = ninox_core::runtime::create_session(config.runtime.backend, &id, &ws_str, &cmd, &env).await {
        // Roll BOTH rows back rather than marking the session Terminated the
        // way `run_spawn` does for a worker. A worker's Terminated row is a
        // useful record that the retention sweep eventually purges; an
        // orchestrator's never is — `sweep_retired_sessions` skips every
        // session id that belongs to an orchestrator — so a ghost row here
        // would sit in the store forever AND permanently burn the name, since
        // the duplicate-name guard above would keep finding it. Nothing ran,
        // so there is nothing worth recording.
        let _ = store.delete_session(&id);
        let _ = store.delete_orchestrator(&id);
        return Err(e);
    }

    if let Some(brief) = prompt {
        // Typing at a harness that hasn't drawn its input box yet is swallowed
        // outright, so wait for the prompt before delivering the brief.
        let spawner = std::env::var("NINOX_ORCHESTRATOR_ID").ok().filter(|s| !s.is_empty());
        let message = format!("{brief}{}", orchestrator_context_footer(&id, spawner.as_deref()));
        if !ninox_core::runtime::wait_for_input_prompt(&id, std::time::Duration::from_secs(90)).await {
            eprintln!("warning: {id} is still starting up — sending the brief anyway");
        }
        if let Err(e) = ninox_core::messaging::deliver_message(
            store, &sessions_dir, &id, &message, config.send_mechanism(),
        ).await {
            eprintln!(
                "warning: could not deliver the initial brief to {id}: {e}\n\
                 retry with: ninox send {id} \"<the brief>\""
            );
        }
    }

    Ok(SpawnedOrchestrator { id, workspace: ws_str })
}

/// The tmux env for a CLI-spawned orchestrator. Mirrors
/// `spawn_util::interactive_env_vars` plus the two vars that make a session
/// an *orchestrator*: its own id (so workers it spawns report back to it)
/// and the caller type (which gates the subagent blocker and `ninox reap`).
fn orchestrator_env_vars<'a>(
    ninox_bin:      &'a str,
    ninox_config:   &'a str,
    catalogue_path: &'a str,
    session_id:     &'a str,
    sessions_dir:   &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("NINOX_BIN",             ninox_bin),
        ("NINOX_CONFIG",          ninox_config),
        ("NINOX_BRAIN",           catalogue_path),
        ("NINOX_SESSION",         session_id),
        ("NINOX_DATA_DIR",        sessions_dir),
        ("NINOX_ORCHESTRATOR_ID", session_id),
        ("NINOX_CALLER_TYPE",     "orchestrator"),
        // A CLI-spawned orchestrator is messaged the same way any other
        // session is — its initial brief goes out through `deliver_message`
        // a few lines after it starts — so it needs the messaging gate on
        // for the same reason the other two spawn paths do. See
        // `ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV`.
        (ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV, "1"),
    ]
}

/// The context footer appended to a spawned orchestrator's initial brief:
/// who it is, that it coordinates rather than implements, and (when spawned
/// by another orchestrator) the channel back to whoever asked for it.
fn orchestrator_context_footer(id: &str, spawner: Option<&str>) -> String {
    let mut footer = format!(
        "\n\n---\n\
         Ninox orchestrator `{id}`\n\n\
         **Role:** you are an orchestrator, not a worker. Spawn workers \
         (`ninox spawn`) for the brief above — never implement it yourself.\n",
    );
    if let Some(spawner) = spawner {
        footer.push_str(&format!(
            "\nSpawned by orchestrator `{spawner}`. Report back when the work is \
             done or you need a decision:\n\
             ```bash\n\
             ninox send {spawner} \"<your message>\"\n\
             ```\n",
        ));
    }
    footer
}

/// `ninox request-work` — record a work request in this worker's session
/// metadata. The engine's poller notices it within one tick, notifies the
/// UI, and forwards it to the orchestrator's terminal.
fn run_request_work(store: &Store, description: &str) -> anyhow::Result<()> {
    let description = description.trim();
    if description.is_empty() {
        anyhow::bail!("request-work needs a non-empty description of the work");
    }
    let session_id = std::env::var("NINOX_SESSION")
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!(
            "NINOX_SESSION is not set — `ninox request-work` only works inside \
             a Ninox worker session"
        ))?;
    let sessions_dir = std::env::var("NINOX_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(AppConfig::sessions_dir);
    let request = ninox_core::hooks::append_work_request(&sessions_dir, &session_id, description)?;
    // The file above is the delivery queue; this row is the fleet's
    // queryable record of what the orchestrator still owes.
    let orchestrator_id = store.get_session(&session_id).ok().flatten()
        .and_then(|s| s.orchestrator_id)
        .or_else(|| std::env::var("NINOX_ORCHESTRATOR_ID").ok().filter(|s| !s.is_empty()));
    if let Err(e) = store.insert_work_request(&ninox_core::store::WorkRequestRow {
        id:              request.id.clone(),
        from_session:    session_id.clone(),
        orchestrator_id,
        body:            request.description.clone(),
        created_at:      request.requested_at,
        delivered_at:    None,
        resolved_at:     None,
    }) {
        tracing::warn!("record work request {} in the store: {e}", request.id);
    }
    println!(
        "work request {} recorded — the orchestrator will be asked to spawn a worker for it",
        request.id,
    );
    Ok(())
}

/// Handler for `ninox inbox drain-stop`/`drain-prompt` — the Stop/
/// UserPromptSubmit hooks installed in a worker's worktree settings when
/// inbox messaging is enabled (`ensure_statusline_settings`). Never returns
/// an error and never panics: a broken inbox must never wedge the human's
/// Claude Code session shut, so any failure here degrades to printing
/// nothing (Stop proceeds normally / the prompt passes through untouched),
/// with the actual error logged to stderr for debugging.
fn run_inbox(action: InboxAction, db_path: PathBuf) {
    use std::io::Read;
    // Required by the Claude Code hook contract even though neither drain
    // action needs any of its fields: pending-message dedup via mark-
    // delivered already makes repeat Stop invocations (`stop_hook_active`)
    // self-limiting — a second call only ever sees messages that arrived
    // after the first block, never a stale batch re-blocking forever.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let Some(session_id) = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty()) else {
        eprintln!("ninox inbox: NINOX_SESSION not set — nothing to drain");
        return;
    };
    let sessions_dir = inbox_sessions_dir();

    let response = match action {
        InboxAction::DrainStop   => ninox_core::inbox::drain_for_stop(&sessions_dir, &session_id),
        InboxAction::DrainPrompt => ninox_core::inbox::drain_for_prompt_submit(&sessions_dir, &session_id),
    };
    let emitted_block = matches!((&action, &response), (InboxAction::DrainStop, Ok(Some(_))));
    match response {
        Ok(Some(json)) => println!("{json}"),
        Ok(None) => {}
        Err(e) => eprintln!("ninox inbox: {e}"),
    }
    // A blocked Stop means the agent continues working on the injected
    // messages — record that. Best-effort backstop for the parallel-hook
    // race with `worker-status hook-stop` (see run_worker_status_hook); any
    // failure is swallowed, same never-fail contract as the drain itself.
    if emitted_block {
        if let Ok(store) = Store::open(&db_path) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let _ = ninox_core::worker_status::apply_activity(
                &store, &session_id,
                ninox_core::worker_status::ActivityEvent::HookStop { turn_continues: true },
                now,
            );
        }
    }
}

/// Which hook invoked `run_worker_status_hook` — the `ActivityEvent` is
/// built inside the handler because Stop's meaning depends on per-session
/// state (pending inbox messages).
enum WorkerStatusHookKind {
    Prompt,
    Stop,
}

/// Handler for `ninox worker-status hook-prompt`/`hook-stop` — the
/// UserPromptSubmit/Stop hooks installed in a worker's worktree settings
/// (`ensure_statusline_settings`). Same contract as `run_inbox`: never
/// returns an error and never panics, because a broken status write must
/// never wedge the human's Claude Code session — any failure degrades to
/// doing nothing, with the error on stderr. Prints nothing: these hooks
/// have no output contract.
fn run_worker_status_hook(
    db_path: PathBuf,
    kind: WorkerStatusHookKind,
    now: i64,
) {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("ninox worker-status: cannot open store: {e}");
            return;
        }
    };
    let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
    // The hook payload's cwd is the worker's worktree even when the hook
    // process itself runs elsewhere; fall back to our own cwd without it.
    let cwd = ninox_core::worker_status::hook_payload_cwd(&input)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let session_id = match ninox_core::worker_status::resolve_session_id(
        &store, env_session.as_deref(), cwd.as_deref(),
    ) {
        Ok(Some(session_id)) => session_id,
        Ok(None) => return, // unidentifiable session — silently no-op, by contract
        Err(e) => {
            eprintln!("ninox worker-status: {e}");
            return;
        }
    };
    let event = match kind {
        WorkerStatusHookKind::Prompt => ninox_core::worker_status::ActivityEvent::HookPrompt,
        // Pending inbox messages mean the sibling `inbox drain-stop` hook
        // will block this Stop and the agent continues working on the
        // injected instructions — recording Idle for that whole
        // continuation would mislead the Workers view. Hooks run in
        // parallel, so this pending check races the drain's mark-delivered;
        // `run_inbox`'s Working write on the block path backstops the case
        // where the drain wins.
        WorkerStatusHookKind::Stop => {
            let turn_continues = inbox_messaging_enabled()
                && ninox_core::inbox::read_pending_messages(&inbox_sessions_dir(), &session_id)
                    .map(|msgs| !msgs.is_empty())
                    .unwrap_or(false);
            ninox_core::worker_status::ActivityEvent::HookStop { turn_continues }
        }
    };
    if let Err(e) = ninox_core::worker_status::apply_activity(&store, &session_id, event, now) {
        eprintln!("ninox worker-status: {e}");
    }
}

fn inbox_messaging_enabled() -> bool {
    AppConfig::load().unwrap_or_default().send_mechanism()
        == ninox_core::config::SendMechanism::Inbox
}

fn inbox_sessions_dir() -> PathBuf {
    std::env::var("NINOX_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(AppConfig::sessions_dir)
}

/// The parsed form of the non-hook `worker-status` verbs, consumed by
/// [`run_worker_status`] — same testability convention as
/// [`PrWatchCliAction`]. The hook verbs live in
/// [`run_worker_status_hook`] instead because their I/O contract differs
/// (stdin JSON, never fail).
enum WorkerStatusCliAction {
    Set { state: ninox_core::ActivityState, note: Option<String> },
    Depend { target: String, note: Option<String>, source: Option<String> },
    Undepend { target: String, source: Option<String> },
    List { json: bool },
}

/// Core logic for `ninox worker-status set|depend|undepend|list`.
/// `self_session` is the invoking session as resolved by
/// `worker_status::resolve_session_id` (env var, then cwd→worktree match).
/// Returns the message to print.
fn run_worker_status(
    store: &ninox_core::store::Store,
    action: WorkerStatusCliAction,
    self_session: Option<String>,
    now: i64,
) -> anyhow::Result<String> {
    use ninox_core::types::{DepKind, SessionDep};
    use ninox_core::worker_status::{apply_activity, ActivityEvent};

    // Depend/Undepend take an explicit `--for <session>` source (the
    // orchestrator path); everything else acts on the invoking session.
    let require_source = |explicit: &Option<String>| -> anyhow::Result<String> {
        if let Some(reference) = explicit {
            return resolve_session_ref(store, reference);
        }
        self_session.clone().ok_or_else(|| anyhow::anyhow!(
            "cannot identify the invoking session — NINOX_SESSION is unset and the \
             working directory is not inside a known session workspace; pass --for <session>",
        ))
    };

    // A reference can resolve (exact id) to a session that is terminal or
    // already purged — writes against it would silently vanish, so the
    // mutating verbs validate liveness first.
    let ensure_live = |session_id: &str| -> anyhow::Result<()> {
        match store.get_session(session_id)? {
            Some(s) if !s.status.is_terminal() => Ok(()),
            _ => anyhow::bail!("session '{session_id}' is no longer live"),
        }
    };

    match action {
        WorkerStatusCliAction::Set { state, note } => {
            let session_id = self_session.ok_or_else(|| anyhow::anyhow!(
                "cannot identify the invoking session — NINOX_SESSION is unset and the \
                 working directory is not inside a known session workspace",
            ))?;
            ensure_live(&session_id)?;
            let state_str = serde_json::to_string(&state)?.replace('"', "");
            match apply_activity(store, &session_id, ActivityEvent::Explicit { state, note }, now)? {
                Some(_) => Ok(format!("activity set to {state_str}")),
                None    => Ok(format!("activity already {state_str} — nothing to update")),
            }
        }
        WorkerStatusCliAction::Depend { target, note, source } => {
            let source_id = require_source(&source)?;
            let target_id = resolve_session_ref(store, &target)?;
            anyhow::ensure!(
                source_id != target_id,
                "a session cannot depend on itself ({source_id})",
            );
            ensure_live(&source_id)?;
            ensure_live(&target_id)?;
            store.add_session_dep(&SessionDep {
                session_id: source_id.clone(),
                depends_on: target_id.clone(),
                kind: DepKind::Declared,
                note,
                created_at: now,
            })?;
            Ok(format!("registered dependency: {source_id} → {target_id}"))
        }
        WorkerStatusCliAction::Undepend { target, source } => {
            let source_id = require_source(&source)?;
            let target_id = resolve_session_ref(store, &target)?;
            if store.remove_session_dep(&source_id, &target_id, DepKind::Declared)? {
                Ok(format!("removed dependency: {source_id} → {target_id}"))
            } else {
                Ok(format!("no declared dependency {source_id} → {target_id}"))
            }
        }
        WorkerStatusCliAction::List { json } => {
            let all_sessions = store.list_sessions()?;
            let sessions: Vec<_> = all_sessions.iter()
                .filter(|s| !s.status.is_terminal())
                .collect();
            let deps = store.list_session_deps()?;
            // Name lookup over the UNFILTERED list: a dependency target that
            // just finished (Done, lingering in the DB) must keep printing
            // by name, not decay to a raw session id.
            let name_of = |id: &str| all_sessions.iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| id.to_string());
            if json {
                let items: Vec<_> = sessions.iter().map(|s| {
                    let depends_on: Vec<_> = deps.iter()
                        .filter(|d| d.session_id == s.id)
                        .map(|d| serde_json::json!({
                            "session_id": d.depends_on,
                            "name": name_of(&d.depends_on),
                            "kind": d.kind.as_str(),
                            "note": d.note,
                        }))
                        .collect();
                    serde_json::json!({
                        "session_id": s.id,
                        "name": s.name,
                        "status": s.status,
                        "activity": s.activity,
                        "activity_note": s.activity_note,
                        "activity_since": s.activity_since,
                        "depends_on": depends_on,
                    })
                }).collect();
                return Ok(serde_json::to_string_pretty(&items)?);
            }
            let mut lines = Vec::new();
            for s in &sessions {
                let activity = serde_json::to_string(&s.activity)?.replace('"', "");
                let note = s.activity_note.as_deref()
                    .map(|n| format!(" — {n}"))
                    .unwrap_or_default();
                lines.push(format!("{}  [{activity}]{note}", s.name));
                for d in deps.iter().filter(|d| d.session_id == s.id) {
                    lines.push(format!("  ⭢ depends on {} ({})", name_of(&d.depends_on), d.kind.as_str()));
                }
            }
            if lines.is_empty() {
                return Ok("no live sessions".to_string());
            }
            Ok(lines.join("\n"))
        }
    }
}

/// Resolve a user-supplied session reference (exact id, else exact name
/// among non-terminal sessions) to a session id.
fn resolve_session_ref(
    store: &ninox_core::store::Store,
    reference: &str,
) -> anyhow::Result<String> {
    let sessions = store.list_sessions()?;
    if sessions.iter().any(|s| s.id == reference) {
        return Ok(reference.to_string());
    }
    let by_name: Vec<_> = sessions.iter()
        .filter(|s| !s.status.is_terminal() && s.name == reference)
        .collect();
    match by_name.as_slice() {
        [only] => Ok(only.id.clone()),
        []     => anyhow::bail!("no session with id or name '{reference}'"),
        many   => anyhow::bail!(
            "session name '{reference}' is ambiguous — use an id: {}",
            many.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", "),
        ),
    }
}

/// The parsed form of `Command::Open`/`Command::Close`/`Command::List{prs}`
/// consumed by [`run_pr_watch`] — kept separate from `Command` so the core
/// logic is testable without going through clap's arg parsing.
enum PrWatchCliAction {
    Open { pr: String },
    Close { pr: String },
    List,
}

/// Core logic for `ninox open --pr` / `ninox close --pr` / `ninox list
/// --prs`, pulled into a free function (same convention as `run_brain_add`/
/// `run_discover_repos`) so tests can drive it directly against a scratch
/// `Store` without spawning the binary. Returns the message to print.
fn run_pr_watch(
    store: &ninox_core::store::Store,
    config_enabled: bool,
    action: PrWatchCliAction,
    opener: Option<String>,
) -> anyhow::Result<String> {
    use ninox_core::{github::parse_pr_url, types::PrWatch};
    match action {
        PrWatchCliAction::Open { pr } => {
            let (repo, number) = parse_pr_url(&pr)
                .ok_or_else(|| anyhow::anyhow!("not a GitHub PR URL: {pr}"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            store.upsert_pr_watch(&PrWatch {
                repo: repo.clone(),
                pr_number: number,
                pr_url: pr,
                opener_session_id: opener,
                created_at: now,
            })?;
            let mut msg = format!("watching {repo}#{number}");
            if !config_enabled {
                msg.push_str(
                    " — note: pr_watch is disabled ([pr_watch].enabled = false), \
                     the watch is recorded but inactive until it is enabled",
                );
            }
            Ok(msg)
        }
        PrWatchCliAction::Close { pr } => {
            let (repo, number) = parse_pr_url(&pr)
                .ok_or_else(|| anyhow::anyhow!("not a GitHub PR URL: {pr}"))?;
            let removed = store.delete_pr_watch(&repo, number, opener.as_deref())?;
            Ok(if removed > 0 {
                format!("closed watch on {repo}#{number}")
            } else {
                format!("no watch on {repo}#{number} for this session")
            })
        }
        PrWatchCliAction::List => {
            let watches = store.list_pr_watches()?;
            if watches.is_empty() {
                return Ok("no active PR watches".to_string());
            }
            Ok(watches
                .iter()
                .map(|w| format!(
                    "{}#{}  opener={}  {}",
                    w.repo, w.pr_number,
                    w.opener_session_id.as_deref().unwrap_or("(unowned)"),
                    w.pr_url,
                ))
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
}

pub(crate) fn group_sessions(
    sessions: Vec<Session>,
    orchestrators: Vec<ninox_core::types::Orchestrator>,
) -> Vec<(Option<ninox_core::types::Orchestrator>, Vec<Session>)> {
    let mut groups: Vec<(Option<ninox_core::types::Orchestrator>, Vec<Session>)> = orchestrators
        .into_iter()
        .map(|o| (Some(o), Vec::new()))
        .collect();
    let mut ungrouped: Vec<Session> = Vec::new();
    for s in sessions {
        // An orchestrator's own session row shares its id; a worker points
        // at its orchestrator via orchestrator_id.
        let owner = s.orchestrator_id.as_deref().unwrap_or(&s.id).to_string();
        match groups.iter_mut().find(|(o, _)| o.as_ref().is_some_and(|o| o.id == owner)) {
            Some((_, members)) => members.push(s),
            None => ungrouped.push(s),
        }
    }
    // Orchestrator's own row first within its group.
    for (o, members) in &mut groups {
        let oid = o.as_ref().map(|o| o.id.clone()).unwrap_or_default();
        members.sort_by_key(|s| (s.id != oid, s.started_at));
    }
    if !ungrouped.is_empty() {
        groups.push((None, ungrouped));
    }
    groups
}

pub(crate) fn render_session_board(groups: &[(Option<ninox_core::types::Orchestrator>, Vec<Session>)]) -> String {
    if groups.iter().all(|(_, m)| m.is_empty()) {
        return "no sessions — start one with `ninox orchestrate <name>`".to_string();
    }
    let mut out = String::new();
    for (orch, members) in groups {
        match orch {
            Some(o) => out.push_str(&format!("{} ({})\n", o.name, o.id)),
            None    => out.push_str("(no orchestrator)\n"),
        }
        for s in members {
            let pr = s.pr_number.map(|n| format!("PR #{n}")).unwrap_or_default();
            out.push_str(&format!(
                "  {:<24} {:<10} {:<20} {:<8} ${:.2}\n",
                s.id, status_slug(&s.status), s.repo, pr, s.cost_usd,
            ));
        }
    }
    out
}

pub(crate) fn status_slug(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Spawning      => "spawning",
        SessionStatus::Working       => "working",
        SessionStatus::PrOpen        => "pr_open",
        SessionStatus::CiFailed      => "ci_failed",
        SessionStatus::ReviewPending => "review_pending",
        SessionStatus::Mergeable     => "mergeable",
        SessionStatus::Done          => "done",
        SessionStatus::Terminated    => "terminated",
        SessionStatus::Interrupted   => "interrupted",
    }
}

pub(crate) fn run_list_sessions(store: &ninox_core::store::Store, json: bool) -> anyhow::Result<String> {
    let sessions = store.list_sessions()?;
    let orchestrators = store.list_orchestrators()?;
    if json {
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "orchestrators": orchestrators,
            "sessions": sessions,
        }))?);
    }
    Ok(render_session_board(&group_sessions(sessions, orchestrators)))
}

/// Core of `ninox capabilities`, split out from arg parsing so it can be
/// tested against a constructed [`AppConfig`] rather than the machine's
/// real one.
///
/// Walks `ninox_core::capabilities::REGISTRY` (filtered to `audience_filter`,
/// or unfiltered when `None`) and reports each entry's name, whether its
/// config gate is currently satisfied, and its frontmatter description.
/// Disabled capabilities are listed rather than hidden — an agent asking
/// what ninox can do is better served by "watch-pr is off" than by silence.
fn run_capabilities(
    config: &AppConfig,
    audience_filter: Option<Audience>,
    json: bool,
) -> String {
    use ninox_core::capabilities;

    // `Audience::Both` matches every entry, so it doubles as "no filter".
    let filter = audience_filter.unwrap_or(Audience::Both);
    let caps: Vec<_> = capabilities::for_audience(filter).collect();

    if json {
        let items: Vec<_> = caps
            .iter()
            .map(|cap| {
                serde_json::json!({
                    "name":        cap.name,
                    "audience":    cap.audience.as_str(),
                    "enabled":     (cap.enabled)(config),
                    "description": cap.md_for(filter).and_then(capabilities::description).unwrap_or(""),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&items)
            .unwrap_or_else(|_| "[]".to_string());
    }

    let width = caps.iter().map(|c| c.name.len()).max().unwrap_or(0);
    caps.iter()
        .map(|cap| {
            let status = if (cap.enabled)(config) { "[enabled]" } else { "[disabled]" };
            let desc = cap.md_for(filter).and_then(capabilities::description).unwrap_or("");
            format!("{:<width$}  {:<10}  {}", cap.name, status, desc, width = width)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Handler for `ninox statusline`. Never returns an error and never
/// panics: any failure (bad JSON, no store, no matching session) degrades
/// to printing the minimal fallback line so Claude Code's statusline row
/// never goes blank. See `ninox_core::lifecycle::statusline` for the
/// actual parsing/update/render logic — this is a thin I/O wrapper.
fn run_statusline(db_path: PathBuf) {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let payload = ninox_core::lifecycle::statusline::parse_payload(&input);

    if let Ok(store) = Store::open(&db_path) {
        let _ = ninox_core::lifecycle::statusline::apply_update(&store, &payload);
    }

    println!("{}", ninox_core::lifecycle::statusline::render_line(&payload));
}

async fn run_brain(action: BrainAction, store: Arc<Store>) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let brain_path = config.resolved_brain_path();
    // First open of a config-declared remote catalogue materializes its
    // .sync.toml (a local-fs write only; the sync itself is lazy).
    if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, &brain_path) {
        tracing::warn!("brain: failed to materialize .sync.toml: {e}");
    }

    match action {
        BrainAction::Index => {
            run_remote_sync_if_configured(&brain_path).await;
            let brain = BrainIndex::open(&brain_path)?;
            let embedder = try_build_embedder();
            let stats = brain.rebuild(embedder.as_deref())?;
            println!(
                "indexed {} entries ({} embedded, {} cached)",
                stats.indexed, stats.embedded, stats.cached
            );
        }
        BrainAction::Sync => {
            match ninox_core::brain_sync::BrainSync::for_brain(&brain_path).await {
                Ok(None) => {
                    eprintln!("this brain has no remote — configure one with `ninox brain remote set s3://bucket/prefix`");
                    std::process::exit(1);
                }
                Ok(Some(sync)) => {
                    let report = sync.sync().await?;
                    print_sync_report(&report);
                    if report.changed_local() {
                        let brain = BrainIndex::open(&brain_path)?;
                        let embedder = try_build_embedder();
                        brain.rebuild(embedder.as_deref())?;
                    }
                }
                Err(e) => anyhow::bail!("brain remote unavailable: {e}"),
            }
        }
        BrainAction::Query { text, entry_type, tag } => {
            let embedder = if text.trim().is_empty() { None } else { try_build_embedder() };
            let brain = ninox_core::brain_sync::open_synced(&brain_path, embedder.as_deref()).await?;
            let filters = QueryFilters { entry_type, tag };
            let entries = brain.query(&text, embedder.as_deref(), filters)?;
            for entry in &entries {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Show { path } => {
            let brain = ninox_core::brain_sync::open_synced(&brain_path, None).await?;
            match brain.get(&path)? {
                Some(entry) => println!("{}", serde_json::to_string_pretty(&entry)?),
                None => {
                    eprintln!("entry not found: {path}");
                    std::process::exit(1);
                }
            }
        }
        BrainAction::Add { path, content } => {
            let content = match content {
                Some(c) => c,
                None => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
            let stats = run_brain_add(&brain_path, &path, &content)?;
            println!(
                "wrote {path} and indexed {} entries ({} embedded, {} cached)",
                stats.indexed, stats.embedded, stats.cached
            );
        }
        BrainAction::Export { output } => {
            let stats = ninox_core::brain_archive::export(&brain_path, &output)?;
            println!("exported {} entries to {}", stats.files, output.display());
        }
        BrainAction::Import { input, into, force } => {
            let target = into.unwrap_or(brain_path);
            let stats = ninox_core::brain_archive::import(&input, &target, force)?;
            println!("imported {} entries into {}", stats.imported, target.display());
            if !stats.skipped.is_empty() {
                eprintln!(
                    "skipped {} conflicting entr{} already present in the target brain (use --force to overwrite):",
                    stats.skipped.len(),
                    if stats.skipped.len() == 1 { "y" } else { "ies" }
                );
                for path in &stats.skipped {
                    eprintln!("  {}", path.display());
                }
            }
            if !stats.failed.is_empty() {
                eprintln!("failed to extract {} entr{}:", stats.failed.len(), if stats.failed.len() == 1 { "y" } else { "ies" });
                for (path, err) in &stats.failed {
                    eprintln!("  {}: {err}", path.display());
                }
            }

            let brain = BrainIndex::open(&target)?;
            let embedder = try_build_embedder();
            let rebuild_stats = brain.rebuild(embedder.as_deref())?;
            println!(
                "indexed {} entries ({} embedded, {} cached)",
                rebuild_stats.indexed, rebuild_stats.embedded, rebuild_stats.cached
            );

            if !stats.skipped.is_empty() || !stats.failed.is_empty() {
                std::process::exit(1);
            }
        }
        BrainAction::DiscoverRepos { paths } => {
            // Each catalogue group discovered below opens its own
            // BrainIndex (see run_discover_repos) rather than reusing
            // `brain` above, since candidates can span multiple catalogues.
            run_discover_repos(&brain_path, &store, paths)?;
        }
        BrainAction::Remote { action } => match action {
            RemoteAction::Set { url, endpoint, region, ttl } => {
                let cfg = ninox_core::brain_sync::SyncToml {
                    remote: url,
                    endpoint,
                    region,
                    cache_ttl_secs: ttl,
                };
                cfg.save(&brain_path)?;
                println!("remote set to {} for {}", cfg.remote, brain_path.display());
                match ninox_core::brain_sync::BrainSync::for_brain(&brain_path).await? {
                    Some(sync) => {
                        let report = sync.sync().await?;
                        print_sync_report(&report);
                        let brain = BrainIndex::open(&brain_path)?;
                        let embedder = try_build_embedder();
                        brain.rebuild(embedder.as_deref())?;
                    }
                    None => unreachable!(".sync.toml was just written"),
                }
            }
            RemoteAction::Status => match ninox_core::brain_sync::remote_status(&brain_path)? {
                None => {
                    println!("no remote configured for {}", brain_path.display());
                }
                Some(s) => {
                    println!("remote:          {}", s.remote);
                    println!("cache ttl:       {}s", s.cache_ttl_secs);
                    println!("last generation: {}", s.generation);
                    println!(
                        "last check:      {}",
                        if s.last_check_unix == 0 { "never".to_string() } else { ninox_core::brain_sync::rfc3339(s.last_check_unix) }
                    );
                    println!("pending pushes:  {}", s.pending_pushes.len());
                    for rel in &s.pending_pushes {
                        println!("  {rel}");
                    }
                    println!("live conflicts:  {}", s.conflict_files.len());
                    for rel in &s.conflict_files {
                        println!("  {rel}");
                    }
                }
            },
            RemoteAction::Unset => {
                let removed_cfg = std::fs::remove_file(brain_path.join(ninox_core::brain_sync::SYNC_TOML)).is_ok();
                let _ = std::fs::remove_file(brain_path.join(ninox_core::brain_sync::SYNC_STATE));
                if removed_cfg {
                    println!("remote detached; {} is a plain local brain again", brain_path.display());
                } else {
                    println!("no remote was configured for {}", brain_path.display());
                }
            }
        },
    }

    Ok(())
}

/// `ninox brain index` on a remote-backed brain: full sync BEFORE the
/// rebuild so pulled entries land in the index (spec: pull → resolve →
/// push → rebuild). Failures degrade to local-only indexing — the index
/// step must keep working offline.
async fn run_remote_sync_if_configured(brain_path: &std::path::Path) {
    match ninox_core::brain_sync::BrainSync::for_brain(brain_path).await {
        Ok(None) => {}
        Ok(Some(sync)) => match sync.sync().await {
            Ok(report) => print_sync_report(&report),
            Err(e) => eprintln!("brain sync failed (continuing with local index): {e}"),
        },
        Err(e) => eprintln!("brain remote unavailable (continuing local-only): {e}"),
    }
}

fn print_sync_report(report: &ninox_core::brain_sync::SyncReport) {
    println!(
        "synced with remote: pulled {}, pushed {}, deleted {} local / {} remote, {} conflict{}",
        report.pulled,
        report.pushed,
        report.deleted_local,
        report.deleted_remote,
        report.conflicts.len(),
        if report.conflicts.len() == 1 { "" } else { "s" },
    );
    for rel in &report.conflicts {
        eprintln!("  conflict copy kept: {rel}");
    }
}

/// `ninox brain discover-repos` — scan `paths` (or, if empty, every
/// workspace_path the session store has ever recorded) and write what's
/// mechanically derivable about each repo into `repos/`, plus any
/// mechanically detectable relationships into `relationships/`.
///
/// Queries the brain for each entry's id before writing (mirroring the
/// "query first" convention `docs/BRAIN.md` and the harvest prompt in
/// `lifecycle::brain_harvest` teach) purely to report new-vs-updated counts —
/// the write itself is idempotent regardless, since each repo's entry id is
/// deterministic (see `repo_discovery::repo_entry_ids`), so re-running
/// overwrites the same file rather than creating a duplicate under a
/// different name.
///
/// Candidate workspaces are grouped by the brain catalogue that should
/// receive their discovery output, and each group is discovered and written
/// independently. When `paths` are given explicitly on the CLI, they all go
/// to `default_brain_path` (this invocation's own resolved brain — the
/// caller picked it on purpose, same as `ninox brain index`/`query`/`show`).
/// When defaulting to every known session's `workspace_path`, each session's
/// own recorded `catalogue_path` (its `NINOX_BRAIN` at spawn time — see
/// `Session::catalogue_path`) takes precedence over `default_brain_path` —
/// the same rule `Poller::trigger_brain_harvest` follows, and for the same
/// reason: a worker spawned against a non-default catalogue must have its
/// facts land in that catalogue, not silently in whichever brain happens to
/// be default for this CLI invocation.
fn run_discover_repos(
    default_brain_path: &std::path::Path,
    store: &Store,
    paths: Vec<PathBuf>,
) -> anyhow::Result<()> {
    let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    if paths.is_empty() {
        for session in store.list_sessions()? {
            let Some(workspace) = session.workspace_path else { continue };
            let catalogue = session
                .catalogue_path
                .map(PathBuf::from)
                .unwrap_or_else(|| default_brain_path.to_path_buf());
            groups.entry(catalogue).or_default().push(PathBuf::from(workspace));
        }
    } else {
        groups.insert(default_brain_path.to_path_buf(), paths);
    }

    if groups.is_empty() {
        println!(
            "no candidate workspaces — pass one or more paths, or spawn a worker first \
             so the session store has a workspace_path to scan"
        );
        return Ok(());
    }

    for (catalogue_path, candidates) in groups {
        discover_repos_into_catalogue(&catalogue_path, &candidates)?;
    }
    Ok(())
}

/// Discover repos among `candidates` and write the results into the single
/// brain catalogue at `catalogue_path`. See [`run_discover_repos`] for how
/// candidates are grouped by catalogue before reaching here.
fn discover_repos_into_catalogue(catalogue_path: &std::path::Path, candidates: &[PathBuf]) -> anyhow::Result<()> {
    let brain = BrainIndex::open(catalogue_path)?;
    let discovery = repo_discovery::discover(candidates);
    let ids = repo_discovery::repo_entry_ids(&discovery.repos);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let mut new_count = 0usize;
    let mut updated_count = 0usize;
    for (repo, id) in discovery.repos.iter().zip(&ids) {
        if brain.get(id)?.is_some() { updated_count += 1 } else { new_count += 1 }
        write_brain_entry(catalogue_path, id, &repo_discovery::repo_entry_markdown(repo, &today))?;
    }

    for (repo_index, worktrees) in &discovery.extra_worktrees {
        let repo = &discovery.repos[*repo_index];
        let repo_id = &ids[*repo_index];
        let id = repo_discovery::worktree_relationship_id(repo_id);
        let markdown = repo_discovery::worktree_relationship_markdown(repo, repo_id, worktrees, &today);
        write_brain_entry(catalogue_path, &id, &markdown)?;
    }

    let org_groups = repo_discovery::group_by_owner(&discovery.repos, &ids);
    for (owner, members) in &org_groups {
        let id = repo_discovery::shared_org_relationship_id(owner);
        let markdown = repo_discovery::shared_org_relationship_markdown(owner, members, &today);
        write_brain_entry(catalogue_path, &id, &markdown)?;
    }

    let embedder = try_build_embedder();
    let stats = brain.rebuild(embedder.as_deref())?;
    println!(
        "[{}] discovered {} repo(s) ({} new, {} updated), {} worktree relationship(s), \
         {} shared-org relationship(s) — indexed {} entries",
        catalogue_path.display(),
        discovery.repos.len(),
        new_count,
        updated_count,
        discovery.extra_worktrees.len(),
        org_groups.len(),
        stats.indexed,
    );
    Ok(())
}

/// Write a brain entry's Markdown content to `brain_path/id`, creating its
/// parent section directory (e.g. `repos/`) if needed.
fn write_brain_entry(brain_path: &std::path::Path, id: &str, content: &str) -> anyhow::Result<()> {
    let path = brain_path.join(id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Writes `content` to `path` under the brain root and immediately rebuilds
/// the index — the `BrainAction::Add` handler's logic, pulled into a free
/// function (same convention as `run_discover_repos`) so it's callable
/// directly from tests without going through CLI arg parsing or resolving
/// `AppConfig`'s default brain path.
fn run_brain_add(brain_path: &std::path::Path, path: &str, content: &str) -> anyhow::Result<ninox_core::brain::RebuildStats> {
    write_brain_entry(brain_path, path, content)?;
    let brain = BrainIndex::open(brain_path)?;
    let embedder = try_build_embedder();
    brain.rebuild(embedder.as_deref())
}

#[cfg(test)]
mod discover_repos_tests {
    use super::run_discover_repos;
    use ninox_core::{store::Store, types::{Session, SessionStatus}};
    use std::path::Path;

    fn init_repo(dir: &Path, remote: &str) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["remote", "add", "origin", remote]);
        run(&["commit", "-q", "--allow-empty", "-m", "init"]);
    }

    fn session(id: &str, workspace: &Path, catalogue_path: Option<&Path>) -> Session {
        Session {
            id: id.to_string(),
            orchestrator_id: None,
            name: id.to_string(),
            repo: String::new(),
            status: SessionStatus::Working,
            agent_type: "claude-code".to_string(),
            cost_usd: 0.0,
            started_at: 0,
            pr_number: None,
            pr_id: None,
            workspace_path: Some(workspace.to_string_lossy().to_string()),
            pid: None,
            model: None,
            context_tokens: None,
            catalogue_path: catalogue_path.map(|p| p.to_string_lossy().to_string()),
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
        }
    }

    /// Mirrors `Poller::trigger_brain_harvest`'s existing rule (see
    /// `poller.rs`'s `metadata_sync_brain_harvest_prefers_session_catalogue_path_over_default`
    /// test): a session spawned against a non-default catalogue must have
    /// its discovered repo facts land in that catalogue, not silently in
    /// whichever brain happens to be default for this CLI invocation.
    #[test]
    fn discover_repos_routes_each_session_to_its_own_catalogue() {
        let tmp = tempfile::tempdir().unwrap();
        let default_brain = tmp.path().join("default-brain");
        let other_brain = tmp.path().join("other-brain");

        let repo_default = tmp.path().join("repo-default");
        let repo_other = tmp.path().join("repo-other");
        std::fs::create_dir_all(&repo_default).unwrap();
        std::fs::create_dir_all(&repo_other).unwrap();
        init_repo(&repo_default, "git@github.com:acme/repo-default.git");
        init_repo(&repo_other, "git@github.com:acme/repo-other.git");

        let store = Store::open(tmp.path().join("store.db")).unwrap();
        // No catalogue_path recorded -- must fall back to the default brain.
        store.upsert_session(&session("s-default", &repo_default, None)).unwrap();
        // Spawned against a non-default catalogue -- must land there instead.
        store.upsert_session(&session("s-other", &repo_other, Some(&other_brain))).unwrap();

        run_discover_repos(&default_brain, &store, Vec::new()).unwrap();

        assert!(
            default_brain.join("repos/repo-default.md").exists(),
            "session with no catalogue_path must land in the default brain"
        );
        assert!(
            !default_brain.join("repos/repo-other.md").exists(),
            "must not leak the other session's repo into the default brain"
        );
        assert!(
            other_brain.join("repos/repo-other.md").exists(),
            "session with a catalogue_path must land in its own brain"
        );
        assert!(
            !other_brain.join("repos/repo-default.md").exists(),
            "must not leak the default session's repo into the other catalogue"
        );
    }
}

/// Attempt to construct the local embedding model, falling back to `None`
/// (keyword-only search) on any failure — offline first run, corrupted
/// model cache, unsupported platform, etc. Embedding is an enhancement
/// layer; it must never be a hard dependency of `brain index`/`brain query`.
fn try_build_embedder() -> Option<Arc<dyn ninox_core::embeddings::Embedder>> {
    build_embedder(true)
}

/// `try_build_embedder`, optionally without the download progress bar
/// (the TUI owns the terminal).
fn build_embedder(show_download_progress: bool) -> Option<Arc<dyn ninox_core::embeddings::Embedder>> {
    match ninox_core::embeddings::FastEmbedEmbedder::try_new_with_progress(show_download_progress) {
        Ok(embedder) => Some(Arc::new(embedder)),
        Err(err) => {
            tracing::warn!("brain: embedding model unavailable, falling back to keyword-only search: {err}");
            None
        }
    }
}

/// True when this process was started through the `nx` alias.
fn invoked_as_nx() -> bool {
    std::env::args_os().next().is_some_and(|a| is_nx_argv0(std::path::Path::new(&a)))
}

fn is_nx_argv0(argv0: &std::path::Path) -> bool {
    argv0.file_stem().is_some_and(|s| s == ninox_core::hooks::NX_ALIAS)
}

async fn run_tui(
    store: Arc<Store>,
    port_arg: Option<u16>,
    headless: bool,
    db_path: PathBuf,
    force_gui: bool,
) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let port = port_arg.unwrap_or(config.port);
    let orchestrator_root = config.resolved_orchestrator_root();
    let orchestrator_agent = config.orchestrator.clone();
    let config_path = AppConfig::config_path().to_string_lossy().to_string();
    let brain_path = config.resolved_brain_path();

    let ninox_bin = ninox_core::hooks::canonical_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ninox".to_string());

    use std::io::IsTerminal;
    // Bare `nx` from a terminal, or bare `ninox` with no display to open,
    // runs the TUI as a *client* (tui::run ensures the daemon out-of-process,
    // so quitting it leaves agents and the poller running). Bare `ninox`
    // with a display keeps opening the desktop app, as does `ninox gui`;
    // --headless still means "be the daemon" and never opens either.
    if !headless && !force_gui && std::io::stdout().is_terminal() && (invoked_as_nx() || !has_display()) {
        return crate::tui::run(store, port, db_path).await;
    }

    let act_as_daemon = headless || !has_display();
    if daemon_should_yield(act_as_daemon, port).await {
        // Exit 0: launchd's KeepAlive{SuccessfulExit=false} and systemd's
        // Restart=on-failure only restart failures, so a login-item daemon
        // finding the GUI (or a TUI-spawned daemon) on the port stops here
        // instead of crash-looping, each loop re-seeding and polling.
        tracing::info!("another ninox already serves :{port} — exiting");
        return Ok(());
    }

    if let Err(e) = ninox_core::orchestrator_root::setup_orchestrator_root(
        &orchestrator_root, &config, &ninox_bin, &config_path,
    ).await
    {
        tracing::warn!("orchestrator root setup failed: {e}");
    }

    let brain = Arc::new(BrainIndex::open(&brain_path)?);
    let embedder: Option<Arc<dyn ninox_core::embeddings::Embedder>> =
        match tokio::task::spawn_blocking(ninox_core::embeddings::FastEmbedEmbedder::try_new).await {
            Ok(Ok(embedder)) => Some(Arc::new(embedder) as Arc<dyn ninox_core::embeddings::Embedder>),
            Ok(Err(err)) => {
                tracing::warn!("brain: embedding model unavailable, semantic search disabled: {err}");
                None
            }
            Err(join_err) => {
                tracing::warn!("brain: embedder init task panicked: {join_err}");
                None
            }
        };
    let engine = match resolve_token(config.github_token.clone()) {
        Some(token) => Engine::new_with_github(Arc::clone(&store), token),
        None        => Engine::new(Arc::clone(&store)),
    };
    let token = CancellationToken::new();

    // A deliberate --headless (and the no-display daemon fallback below,
    // which behaves the same way) must bind the port or die — otherwise a
    // bind failure just gets traced and the process parks on ctrl_c forever,
    // leaving a permanent duplicate poller. The GUI path instead skips
    // hosting entirely when something is already listening, since the GUI
    // reads the store directly and doesn't need its own poller/server.
    let already_running = !act_as_daemon && ninox_core::daemon::port_in_use(port).await;

    // The GUI always hosts its own poller: its live PR/cost/context updates
    // arrive as in-process Engine events, and `PollSessions` only adopts
    // new rows and terminal statuses from the store — an out-of-process
    // daemon's writes would leave the fleet stale. Two pollers against the
    // same store double GitHub polling while both run (the pre-existing
    // cost of GUI + `--headless` together); only the server bind is skipped
    // when something already holds the port.
    let poller = Poller::new(engine.clone())
        .with_fleet_restorer(fleet::auto_restorer(Arc::clone(&store)));
    tokio::spawn({
        let t = token.clone();
        async move { poller.start(t).await }
    });

    let server_task = if already_running {
        tracing::info!("port :{port} already in use — not binding the server (poller runs in-process)");
        None
    } else {
        let task = tokio::spawn({
            let e = engine.clone();
            let b = brain.clone();
            let emb = embedder.clone();
            async move {
                // Traced here, inside the task, rather than only at the
                // headless select-arm below — the GUI path never awaits
                // this handle, so that's the only place a bind/start
                // failure would otherwise be visible at all.
                let res = ninox_server::start(e, b, emb, port).await;
                if let Err(err) = &res {
                    tracing::error!("server: {err}");
                }
                res
            }
        });
        tracing::info!("ninox ready on :{port}");
        Some(task)
    };

    if act_as_daemon {
        // already_running is forced false above when act_as_daemon, so the
        // server was always spawned.
        let task = server_task.expect("act_as_daemon implies the server was spawned");
        let failure = tokio::select! {
            _ = tokio::signal::ctrl_c() => None,
            res = task => match res {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(join_err) => Some(anyhow::anyhow!("server task panicked: {join_err}")),
            },
        };
        token.cancel();
        match failure {
            // Lost a bind race to another ninox since the probe above.
            Some(_) if ninox_core::daemon::port_in_use(port).await => {
                tracing::info!("another ninox already serves :{port} — exiting");
            }
            // A daemon that can't bind/run its port is a duplicate poller
            // waiting to happen — die loudly instead of parking on ctrl_c
            // forever (daemon.log is where `ensure_daemon`'s startup wait
            // points users). `process::exit` skips destructors, so an
            // in-flight auto-restore's lease is released by hand first.
            Some(e) => {
                tracing::error!("server: {e}");
                fleet::release_own_restore_lease(&store);
                std::process::exit(1);
            }
            None => {}
        }
        return Ok(());
    }

    if config.runtime.backend == ninox_core::runtime::Backend::Tmux {
        if let Err(e) = tmux::require_version().await {
            eprintln!("{e}");
            std::process::exit(1);
        }
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

    const SYMBOLS_NERD_FONT_MONO: &[u8] =
        include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");
    const FONT_NEWSREADER: &[u8] =
        include_bytes!("../assets/fonts/Newsreader[opsz,wght].ttf");
    const FONT_NEWSREADER_ITALIC: &[u8] =
        include_bytes!("../assets/fonts/Newsreader-Italic[opsz,wght].ttf");
    const FONT_ARCHIVO: &[u8] =
        include_bytes!("../assets/fonts/Archivo[wdth,wght].ttf");
    const FONT_SPLINE_SANS_MONO: &[u8] =
        include_bytes!("../assets/fonts/SplineSansMono[wght].ttf");

    iced::application("Ninox", app::App::iced_update, app::App::iced_view)
        .subscription(app::App::subscription)
        .theme(app::App::theme)
        // Native global zoom (Cmd/Ctrl +/-/0) — scales the whole UI rather
        // than resizing individual widgets. Driven by `App::zoom`.
        .scale_factor(|state| state.zoom)
        .window(window_settings)
        .font(SYMBOLS_NERD_FONT_MONO)
        .font(FONT_NEWSREADER)
        .font(FONT_NEWSREADER_ITALIC)
        .font(FONT_ARCHIVO)
        .font(FONT_SPLINE_SANS_MONO)
        .font(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf").as_slice())
        .font(include_bytes!("../assets/fonts/JetBrainsMono-BoldItalic.ttf").as_slice())
        .default_font(iced::Font::with_name("Archivo"))
        .run_with(move || app::App::new(engine, orchestrator_root, orchestrator_agent, brain))?;

    token.cancel();
    Ok(())
}

/// Prints a stderr-only hint (stdout stays clean for `--json` consumers)
/// when nothing is listening on the effective port, so `ninox list`/`ninox
/// connect` can warn readers that statuses may be stale without depending
/// on a daemon they didn't start.
async fn warn_if_daemon_down(port_arg: Option<u16>) {
    let port = port_arg.unwrap_or_else(|| AppConfig::load().unwrap_or_default().port);
    if !ninox_core::daemon::port_in_use(port).await {
        eprintln!(
            "note: background services not running — statuses may be stale \
             (they start automatically with ninox tui or ninox orchestrate)"
        );
    }
}

fn default_db_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ninox")
        .join("ninox.db")
}

fn first_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join("-")
}

/// First non-empty line of `s`, trimmed and clipped to `max_chars` (with a
/// trailing "…" if truncated) — derives the fleet card summary from a
/// spawn prompt. `None` if `s` has no non-empty line.
fn first_line(s: &str, max_chars: usize) -> Option<String> {
    let line = s.lines().find(|l| !l.trim().is_empty())?.trim();
    if line.chars().count() <= max_chars {
        Some(line.to_string())
    } else {
        let clipped: String = line.chars().take(max_chars).collect();
        Some(format!("{clipped}…"))
    }
}

#[cfg(test)]
mod nx_alias_tests {
    use std::path::Path;

    #[test]
    fn only_the_nx_alias_counts_as_nx() {
        assert!(super::is_nx_argv0(Path::new("nx")));
        assert!(super::is_nx_argv0(Path::new("/Users/me/.cargo/bin/nx")));
        assert!(!super::is_nx_argv0(Path::new("/Users/me/.cargo/bin/ninox")));
        assert!(!super::is_nx_argv0(Path::new("nxx")));
    }
}

#[cfg(test)]
mod brain_add_tests {
    use super::run_brain_add;
    use ninox_core::BrainIndex;

    #[test]
    fn add_writes_and_indexes_in_one_call() {
        let brain_dir = tempfile::tempdir().unwrap();
        let stats = run_brain_add(
            brain_dir.path(),
            "repos/ninox.md",
            "---\nname: ninox\nmetadata:\n  type: repo\n---\n\nNinox repo notes.",
        )
        .unwrap();
        assert_eq!(stats.indexed, 1);

        // No separate `ninox brain index` step required — the entry is
        // already queryable against the freshly written index file.
        let brain = BrainIndex::open(brain_dir.path()).unwrap();
        let found = brain.get("repos/ninox.md").unwrap();
        assert!(found.is_some(), "entry should be queryable immediately after add");
    }

    #[test]
    fn add_creates_missing_parent_directories() {
        let brain_dir = tempfile::tempdir().unwrap();
        run_brain_add(brain_dir.path(), "concepts/new-thing.md", "# new thing").unwrap();
        assert!(brain_dir.path().join("concepts/new-thing.md").exists());
    }
}

/// The daemon path must not start a second poller beside a ninox that
/// already holds the port.
async fn daemon_should_yield(act_as_daemon: bool, port: u16) -> bool {
    act_as_daemon && ninox_core::daemon::port_in_use(port).await
}

fn has_display() -> bool {
    #[cfg(target_os = "macos")]
    { true }
    #[cfg(not(target_os = "macos"))]
    { std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() }
}

#[cfg(test)]
mod daemon_yield_tests {
    #[tokio::test]
    async fn daemon_yields_only_when_the_port_is_held() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(super::daemon_should_yield(true, port).await);
        assert!(!super::daemon_should_yield(false, port).await, "the GUI path shares the port");
        drop(listener);
        assert!(!super::daemon_should_yield(true, port).await);
    }
}

#[cfg(test)]
mod worker_env_tests {
    use super::{
        first_line, pr_worker_context_footer, reject_recursive_worker_spawn,
        resolve_worker_delivery, run_spawn,
        worker_context_footer, worker_env_vars, worker_prompt_for_canonical_workspace,
        worker_prompt_for_workspace, Args, Command, WorkerDelivery,
    };
    use clap::Parser;

    fn init_git_repo() -> std::path::PathBuf {
        let repo = tempfile::tempdir().unwrap().keep();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "init",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        repo
    }

    fn parsed_delivery(value: Option<&str>) -> Option<WorkerDelivery> {
        let mut args = vec![
            "ninox",
            "spawn",
            "--prompt",
            "research the incident",
            "--workspace",
            "/tmp/workspace",
        ];
        if let Some(value) = value {
            args.extend(["--delivery", value]);
        }
        let parsed = Args::try_parse_from(args).unwrap();
        let Some(Command::Spawn { delivery, .. }) = parsed.command else {
            panic!("expected spawn command");
        };
        delivery
    }

    #[test]
    fn spawn_cli_parses_explicit_delivery_modes_and_keeps_omission_distinct() {
        assert_eq!(parsed_delivery(Some("pr")), Some(WorkerDelivery::Pr));
        assert_eq!(parsed_delivery(Some("direct")), Some(WorkerDelivery::Direct));
        assert_eq!(parsed_delivery(None), None);
    }

    #[test]
    fn completion_cli_parses_worker_and_orchestrator_protocol_commands() {
        let complete = Args::try_parse_from(["ninox", "complete", "canonical summary"]).unwrap();
        assert!(matches!(
            complete.command,
            Some(Command::Complete { summary }) if summary == "canonical summary"
        ));
        let receive =
            Args::try_parse_from(["ninox", "receive-completion", "completion-id"]).unwrap();
        assert!(matches!(
            receive.command,
            Some(Command::ReceiveCompletion { completion_id })
                if completion_id == "completion-id"
        ));
    }

    #[test]
    fn omitted_delivery_defaults_by_git_workspace_and_explicit_choice_wins() {
        let repo = init_git_repo();
        let plain = tempfile::tempdir().unwrap();

        assert_eq!(
            resolve_worker_delivery(None, repo.to_str().unwrap()),
            WorkerDelivery::Pr,
        );
        assert_eq!(
            resolve_worker_delivery(None, plain.path().to_str().unwrap()),
            WorkerDelivery::Direct,
        );
        assert_eq!(
            resolve_worker_delivery(Some(WorkerDelivery::Direct), repo.to_str().unwrap()),
            WorkerDelivery::Direct,
        );
        assert_eq!(
            resolve_worker_delivery(Some(WorkerDelivery::Pr), plain.path().to_str().unwrap()),
            WorkerDelivery::Pr,
        );
    }

    #[tokio::test]
    async fn run_spawn_marks_the_session_terminated_when_tmux_create_fails() {
        use std::sync::Arc;
        // A nonexistent workspace makes worktree creation fail (falls back
        // to the shared workspace, also missing), and tmux::create_session
        // now rejects the missing dir. run_spawn upserts the session as
        // Working BEFORE that point — bailing without a status fix-up
        // leaves a permanent Working ghost with no pid for poll_pids to
        // reap (only startup reconciliation would ever catch it).
        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "do the task".into(),
            "/definitely/not/a/real/dir".into(),
            None,
            Some("ghost-spawn-test".into()),
            None,
        )
        .await;
        assert!(result.is_err(), "spawn into a missing workspace must fail");

        let session = store.get_session("ghost-spawn-test").unwrap().unwrap();
        assert!(
            matches!(session.status, ninox_core::SessionStatus::Terminated),
            "failed spawn must not leave a Working ghost, got {:?}", session.status,
        );
        // The raw task brief (no worker-context footer) is what a fleet
        // restore re-briefs a fresh-restarted worker with.
        let facts = store.fleet_record("ghost-spawn-test").unwrap().unwrap();
        assert_eq!(facts.task_brief.as_deref(), Some("do the task"));
    }

    #[tokio::test]
    async fn run_spawn_refuses_to_clobber_an_existing_session_id() {
        use std::sync::Arc;
        use ninox_core::types::{Session, SessionStatus};

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        // A merged worker kept alive for validation ([auto_reap] off): live
        // status, merged_at + pr_number set, squatting the slug "ath-123".
        let kept_alive = Session {
            id: "ath-123".into(), orchestrator_id: Some("orch1".into()), name: "ath-123".into(),
            repo: "o/r".into(), status: SessionStatus::Mergeable, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: Some(9), pr_id: Some(9),
            workspace_path: Some("/ws".into()), pid: None, model: None, context_tokens: None,
            catalogue_path: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None,
            merged_at: Some(1_000),
            activity: Default::default(), activity_note: None, activity_since: None,
        };
        store.upsert_session(&kept_alive).unwrap();

        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "follow-up task".into(),
            "/some/other/dir".into(),
            None,
            Some("ath-123".into()),
            None,
        )
        .await;
        assert!(result.is_err(), "reusing a live worker's name must be refused");

        let after = store.get_session("ath-123").unwrap().unwrap();
        assert_eq!(after.merged_at, Some(1_000), "the existing row's merged_at must be untouched");
        assert_eq!(after.pr_number, Some(9), "the existing row's pr_number must be untouched");
        assert!(
            matches!(after.status, SessionStatus::Mergeable),
            "the existing row must not be hijacked to Terminated, got {:?}", after.status,
        );
    }

    #[test]
    fn worker_prompt_preserves_path_like_prose_without_parsing_it() {
        let prompt = "Explain why file:///Users/mu/dev/ninox/bad%GG is malformed.";
        let prepared = worker_prompt_for_canonical_workspace(
            prompt,
            "/Users/mu/dev/ninox",
            "/Users/mu/dev/ninox-w2",
            WorkerDelivery::Pr,
        )
        .unwrap();

        assert_eq!(prepared.split_once("\n\n---\n").unwrap().0, prompt);
    }

    #[test]
    fn first_line_takes_first_non_empty_line_trimmed() {
        assert_eq!(first_line("  Fix the flaky test  \n\nDetails follow.", 140).as_deref(), Some("Fix the flaky test"));
    }

    #[test]
    fn first_line_skips_leading_blank_lines() {
        assert_eq!(first_line("\n\n  Ship the thing\nmore text", 140).as_deref(), Some("Ship the thing"));
    }

    #[test]
    fn first_line_clips_long_text_with_ellipsis() {
        let long = "a".repeat(200);
        let clipped = first_line(&long, 140).unwrap();
        assert_eq!(clipped.chars().count(), 141); // 140 chars + "…"
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn first_line_is_none_for_blank_prompt() {
        assert_eq!(first_line("   \n\n  ", 140), None);
    }

    #[test]
    fn worker_footer_scopes_to_one_pr_and_routes_extra_work_to_request_work() {
        let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Pr, false);
        assert!(footer.contains("`w1`"), "must name the worker's own session");
        assert!(footer.contains("ninox send orch1"), "must keep the message-back channel");
        assert!(
            footer.contains("ninox complete"),
            "must use the durable completion handshake"
        );
        assert!(footer.contains("ninox request-work"), "must offer the work-request channel");
        assert!(
            footer.to_lowercase().contains("do not"),
            "must forbid doing out-of-scope work / opening extra PRs",
        );
        assert!(
            footer.contains("one pull request") || footer.contains("one PR"),
            "must state the one-worker-one-PR contract",
        );
    }

    #[test]
    fn pr_delivery_wrapper_is_byte_identical_to_upstream_footer() {
        assert_eq!(
            worker_context_footer("w1", "orch1", WorkerDelivery::Pr, false),
            pr_worker_context_footer("w1", "orch1", false),
        );
    }

    #[test]
    fn direct_worker_contract_has_no_pr_delivery_workflow() {
        let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Direct, false);
        assert!(footer.contains("validated artifacts or direct changes"));
        assert!(footer.contains("blocked") && footer.contains("complete"));
        assert!(footer.contains("ninox complete"));
        assert!(!footer.contains("complete the task and open a pull request"));
        assert!(!footer.contains("one worker, one task, one pull request"));
    }

    #[test]
    fn worker_footer_omits_pr_watch_instruction_when_disabled() {
        let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Pr, false);
        assert!(!footer.contains("ninox open --pr"));
        assert!(!footer.contains("ninox close --pr"));
    }

    #[test]
    fn direct_worker_without_orchestrator_still_gets_no_git_delivery_contract() {
        let footer = worker_context_footer("w1", "", WorkerDelivery::Direct, false);

        assert!(footer.contains("Do not create branches"));
        assert!(footer.contains("do not push"));
        assert!(footer.contains("open pull requests"));
        assert!(!footer.contains("ninox send "));
        assert!(!footer.contains("ninox request-work"));
    }

    #[test]
    fn worker_prompt_preserves_forbidden_source_and_authorized_worker_distinction() {
        let prompt = "Never modify the primary checkout /Users/matan.uberstein/dev/ninox. \
                      Work only in the assigned sibling /Users/matan.uberstein/dev/ninox-w2.";
        let prepared = worker_prompt_for_workspace(
            prompt,
            "/Users/matan.uberstein/dev/ninox",
            "/Users/matan.uberstein/dev/ninox-w2",
            WorkerDelivery::Pr,
        )
        .unwrap();

        assert_eq!(prepared.split_once("\n\n---\n").unwrap().0, prompt);
    }

    #[test]
    fn worker_prompt_preserves_source_paths_embedded_in_longer_sibling_names() {
        let prompt = "Keep /Users/mu/dev/ninox-w2 and /Users/mu/dev/ninox-archive distinct \
                      from /Users/mu/dev/ninox.";
        let prepared = worker_prompt_for_canonical_workspace(
            prompt,
            "/Users/mu/dev/ninox",
            "/Users/mu/dev/ninox-w2",
            WorkerDelivery::Pr,
        )
        .unwrap();

        assert_eq!(prepared.split_once("\n\n---\n").unwrap().0, prompt);
    }

    #[test]
    fn worker_prompt_preserves_workspace_comparison_prose() {
        let prompt = "Compare /Users/mu/dev/ninox/config.toml with \
                      /Users/mu/dev/ninox-w2/config.toml; explain differences without editing either.";
        let prepared = worker_prompt_for_canonical_workspace(
            prompt,
            "/Users/mu/dev/ninox",
            "/Users/mu/dev/ninox-w2",
            WorkerDelivery::Pr,
        )
        .unwrap();

        assert_eq!(prepared.split_once("\n\n---\n").unwrap().0, prompt);
    }

    #[test]
    fn worker_prompt_appends_authoritative_assigned_workspace_context() {
        let prepared = worker_prompt_for_canonical_workspace(
            "Inspect the repository without changing this sentence.",
            "/Users/mu/dev/ninox",
            "/Users/mu/dev/ninox-w2",
            WorkerDelivery::Pr,
        )
        .unwrap();

        assert!(prepared.contains(
            "**Ninox workspace:** `/Users/mu/dev/ninox-w2` is the authoritative workspace."
        ));
        assert!(prepared.contains(
            "The original source checkout is repository context only; \
             do not read, write, or run Git commands there."
        ));
    }

    #[test]
    fn direct_worker_workspace_prompt_avoids_git_workflow_guidance() {
        let prompt = worker_prompt_for_workspace(
            "Write the incident report.",
            "/tmp/research",
            "/tmp/research",
            WorkerDelivery::Direct,
        )
        .unwrap();

        assert!(prompt.contains("write artifacts or direct changes"));
        assert!(!prompt.contains("Git command"));
        assert!(!prompt.contains("repository read"));
    }

    #[test]
    fn worker_footer_appends_pr_watch_instruction_when_enabled() {
        let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Pr, true);
        assert!(
            footer.contains("ninox open --pr") && footer.contains("ninox close --pr"),
            "must instruct the worker to register/close PR watches instead of polling gh"
        );
    }

    /// The capability-discovery bootstrap line is ungated — a worker must
    /// always be told how to find out what ninox can do for it, regardless
    /// of which individual capabilities happen to be enabled. It must be
    /// audience-scoped: an unfiltered listing would show the worker
    /// orchestrator-only skills like spawn-worker.
    #[test]
    fn worker_footer_always_points_at_the_worker_scoped_capabilities_command() {
        for pr_watch in [false, true] {
            let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Pr, pr_watch);
            assert!(
                footer.contains("ninox capabilities --worker"),
                "worker-scoped capabilities bootstrap line must be present with pr_watch={pr_watch}"
            );
        }
    }

    #[test]
    fn forwards_brain_and_config_when_present() {
        let env = worker_env_vars(
            "w1",
            "incarnation",
            "/data",
            "orch1",
            Some("/brain.db"),
            Some("/cfg.toml"),
        );
        assert!(env.contains(&("NINOX_ORCHESTRATOR_ID", "orch1")));
        assert!(env.contains(&("NINOX_BRAIN", "/brain.db")));
        assert!(env.contains(&("NINOX_CONFIG", "/cfg.toml")));
        assert!(env.contains(&("NINOX_SESSION", "w1")));
        assert!(env.contains(&("NINOX_WORKER_INCARNATION", "incarnation")));
        assert!(env.contains(&(
            crate::spawn_util::EXECUTION_ROLE_ENV,
            crate::spawn_util::WORKER_EXECUTION_ROLE,
        )));
        assert!(env.contains(&("NINOX_CALLER_TYPE", "worker")));
        assert!(env.contains(&("NINOX_DATA_DIR", "/data")));
        // Workers are the usual target of orchestrator messages; without
        // this they come up with no messaging socket and the configured
        // send mechanism silently degrades. See
        // `ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV`.
        assert!(env.contains(&(ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV, "1")));
        // The legacy ATHENE_* transition names are gone.
        assert!(!env.iter().any(|(k, _)| k.starts_with("ATHENE_")));
    }

    #[test]
    fn omits_brain_config_and_orchestrator_id_when_absent() {
        let env = worker_env_vars("w1", "incarnation", "/data", "", None, None);
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_ORCHESTRATOR_ID"));
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_BRAIN"));
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_CONFIG"));
    }

    #[test]
    fn recursive_worker_spawn_is_rejected_before_allocation() {
        assert!(reject_recursive_worker_spawn(Some("worker"), Some("orchestrator")).is_err());
        assert!(reject_recursive_worker_spawn(None, Some("worker")).is_err());
        assert!(reject_recursive_worker_spawn(Some("orchestrator"), Some("worker")).is_ok());
        assert!(reject_recursive_worker_spawn(None, Some("orchestrator")).is_ok());
        assert!(reject_recursive_worker_spawn(None, None).is_ok());
        assert!(reject_recursive_worker_spawn(Some("invalid"), None).is_err());
    }
}

#[cfg(test)]
mod release_cli_tests {
    use super::{
        caller_is_orchestrator, resolve_release_orchestrator, settle_worker_release,
        validate_standalone_release_scope,
    };
    use crate::spawn_util;
    use ninox_core::types::SessionStatus;

    #[test]
    fn standalone_release_allows_local_admin_or_exact_session_only() {
        assert!(validate_standalone_release_scope("solo", None, None, None).is_ok());
        assert!(
            validate_standalone_release_scope("solo", None, None, Some("solo")).is_ok()
        );
        assert!(
            validate_standalone_release_scope("solo", None, None, Some("other")).is_err()
        );
        assert!(
            validate_standalone_release_scope("solo", Some("orch"), None, None).is_err()
        );
        assert!(
            validate_standalone_release_scope("solo", None, Some("orch"), None).is_err()
        );
    }

    #[test]
    fn release_refuses_active_workers_and_cleanup_failure_restores_retained_state() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = ninox_core::Store::open(root.path().join("release.db")).unwrap();
        store
            .upsert_orchestrator(&ninox_core::types::Orchestrator {
                id: "orch".into(),
                name: "orch".into(),
                created_at: 0,
            })
            .unwrap();
        store
            .upsert_session(&ninox_core::types::Session {
                id: "worker".into(),
                orchestrator_id: Some("orch".into()),
                name: "worker".into(),
                repo: String::new(),
                status: SessionStatus::Working,
                agent_type: "cursor-agent".into(),
                cost_usd: 0.0,
                started_at: 1,
                pr_number: None,
                pr_id: None,
                workspace_path: Some(workspace.to_string_lossy().into_owned()),
                pid: None,
                model: None,
                context_tokens: None,
                catalogue_path: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None,
                gate_status: None,
                merged_at: None,
                activity: ninox_core::types::ActivityState::Unknown,
                activity_note: None,
                activity_since: None,
            })
            .unwrap();
        let worker = store
            .prepare_worker_incarnation(
                "worker",
                Some("orch"),
                1,
                workspace.to_str().unwrap(),
                true,
                3,
            )
            .unwrap();
        assert!(store
            .bind_worker_incarnation(
                "worker",
                &worker.incarnation_id,
                workspace.to_str().unwrap(),
                workspace.to_str().unwrap(),
                None,
            )
            .unwrap());
        assert!(store
            .claim_worker_release("worker", &worker.incarnation_id)
            .unwrap()
            .is_none());

        let intent = store.begin_worker_finalization("orch", "worker").unwrap();
        let ninox_core::types::WorkerFinalizationIntent::Apply(worker) = intent else {
            panic!("first finalization must apply");
        };
        assert!(store
            .complete_worker_finalization("worker", &worker.incarnation_id)
            .unwrap());
        let claim = store
            .claim_worker_release("worker", &worker.incarnation_id)
            .unwrap()
            .unwrap();

        let error = settle_worker_release(
            &store,
            &claim,
            Err(anyhow::anyhow!("simulated Cargo cleanup failure")),
        )
        .unwrap_err();

        assert!(error.to_string().contains("simulated Cargo cleanup failure"));
        assert!(store.is_worker_retained("worker").unwrap());
        assert_eq!(
            store
                .current_worker_incarnation("worker")
                .unwrap()
                .unwrap()
                .state,
            ninox_core::types::WorkerIncarnationState::Retained
        );
    }

    #[tokio::test]
    async fn release_stops_and_confirms_only_the_exact_worker_runtime() {
        let id = format!(
            "release-runtime-{}",
            ninox_core::harness::new_claude_session_id()
        );
        ninox_core::tmux::create_session(
            &id,
            "/tmp",
            "sleep 30",
            &[("NINOX_WORKER_INCARNATION", "incarnation")],
        )
        .await
        .unwrap();

        let self_release =
            spawn_util::stop_exact_worker_runtime(&id, "incarnation", Some(&id)).await;
        assert!(self_release.is_err());
        assert!(ninox_core::tmux::has_session(&id).await);

        spawn_util::stop_exact_worker_runtime(&id, "incarnation", None)
            .await
            .unwrap();
        assert!(!ninox_core::tmux::has_session(&id).await);
    }

    #[tokio::test]
    async fn release_never_stops_a_successor_runtime() {
        let id = format!(
            "release-successor-{}",
            ninox_core::harness::new_claude_session_id()
        );
        ninox_core::tmux::create_session(
            &id,
            "/tmp",
            "sleep 30",
            &[("NINOX_WORKER_INCARNATION", "successor")],
        )
        .await
        .unwrap();

        let result = spawn_util::stop_exact_worker_runtime(&id, "old", None).await;
        assert!(result.is_err());
        assert!(ninox_core::tmux::has_session(&id).await);
        ninox_core::tmux::kill_session(&id).await.unwrap();
    }

    fn orch(s: &str) -> Option<String> { Some(s.to_string()) }

    #[test]
    fn release_guard_accepts_an_orchestrator_session() {
        let id = resolve_release_orchestrator(None, orch("orch-1"), true, orch("orch-1")).unwrap();
        assert_eq!(id, "orch-1");
    }

    #[test]
    fn release_guard_refuses_a_worker_session() {
        // A worker carries NINOX_ORCHESTRATOR_ID (its parent's) but no
        // caller type — that ambient id must not make its siblings' checkouts releasable.
        let err = resolve_release_orchestrator(None, orch("orch-1"), false, orch("w1"))
            .expect_err("a worker must not release")
            .to_string();
        assert!(err.contains("cannot release its siblings"), "{err}");
    }

    /// `--orchestrator-id` is for out-of-session use, not an escape hatch. A
    /// worker passing its own parent's id would otherwise release its
    /// sibling fleet's checkouts.
    #[test]
    fn release_guard_refuses_a_worker_even_with_an_explicit_orchestrator_id() {
        let err = resolve_release_orchestrator(orch("orch-1"), orch("orch-1"), false, orch("w1"))
            .expect_err("--orchestrator-id must not bypass the guard")
            .to_string();
        assert!(err.contains("with or without --orchestrator-id"), "{err}");
    }

    #[test]
    fn release_guard_allows_an_explicit_id_outside_any_session() {
        // A human at a terminal / a script: no session env at all.
        let id = resolve_release_orchestrator(orch("orch-7"), None, false, None).unwrap();
        assert_eq!(id, "orch-7");
    }

    #[test]
    fn release_guard_needs_an_id_from_somewhere() {
        let err = resolve_release_orchestrator(None, None, false, None)
            .expect_err("no id anywhere must fail")
            .to_string();
        assert!(err.contains("NINOX_ORCHESTRATOR_ID"), "{err}");
    }

    #[test]
    fn release_guard_prefers_the_explicit_id_over_the_ambient_one() {
        let id = resolve_release_orchestrator(
            orch("orch-explicit"), orch("orch-ambient"), true, orch("orch-ambient"),
        ).unwrap();
        assert_eq!(id, "orch-explicit");
    }

    /// The store, not the environment, decides who is an orchestrator.
    /// `NINOX_CALLER_TYPE` lives in the agent's own shell, so if it were
    /// trusted a worker could `export NINOX_CALLER_TYPE=orchestrator` and
    /// release its whole fleet's checkouts.
    #[test]
    fn caller_type_env_cannot_promote_a_worker_to_an_orchestrator() {
        assert!(
            !caller_is_orchestrator(Some("w1"), &["orch-1"], Some("orchestrator")),
            "a spoofed caller type must not beat the store",
        );
    }

    #[test]
    fn a_session_id_that_is_an_orchestrator_row_is_an_orchestrator() {
        assert!(caller_is_orchestrator(Some("orch-1"), &["orch-1", "orch-2"], None));
    }

    #[test]
    fn caller_type_is_only_consulted_outside_a_session() {
        // A plain shell has no NINOX_SESSION to look up.
        assert!(caller_is_orchestrator(None, &[], Some("orchestrator")));
        assert!(!caller_is_orchestrator(None, &[], None));
    }
}

#[cfg(test)]
mod pr_watch_cli_tests {
    use super::*;
    use ninox_core::store::Store;

    fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn open_registers_watch_with_opener() {
        let (_d, store) = tmp_store();
        let msg = run_pr_watch(
            &store, true,
            PrWatchCliAction::Open { pr: "https://github.com/o/r/pull/7".into() },
            Some("sess-a".into()),
        ).unwrap();
        let watches = store.list_pr_watches().unwrap();
        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].repo, "o/r");
        assert_eq!(watches[0].pr_number, 7);
        assert_eq!(watches[0].opener_session_id.as_deref(), Some("sess-a"));
        assert!(msg.contains("watching o/r#7"));
    }

    #[test]
    fn open_with_toggle_off_still_records_but_warns() {
        let (_d, store) = tmp_store();
        let msg = run_pr_watch(
            &store, false,
            PrWatchCliAction::Open { pr: "https://github.com/o/r/pull/7".into() },
            None,
        ).unwrap();
        assert_eq!(store.list_pr_watches().unwrap().len(), 1);
        assert!(msg.contains("pr_watch is disabled"));
    }

    #[test]
    fn open_rejects_non_pr_url() {
        let (_d, store) = tmp_store();
        let err = run_pr_watch(
            &store, true,
            PrWatchCliAction::Open { pr: "https://github.com/o/r/issues/7".into() },
            None,
        ).unwrap_err();
        assert!(err.to_string().contains("not a GitHub PR URL"));
        assert!(store.list_pr_watches().unwrap().is_empty());
    }

    #[test]
    fn close_removes_only_callers_watch() {
        let (_d, store) = tmp_store();
        for opener in [Some("sess-a".to_string()), Some("sess-b".to_string())] {
            run_pr_watch(&store, true,
                PrWatchCliAction::Open { pr: "https://github.com/o/r/pull/7".into() },
                opener).unwrap();
        }
        let msg = run_pr_watch(&store, true,
            PrWatchCliAction::Close { pr: "https://github.com/o/r/pull/7".into() },
            Some("sess-a".into())).unwrap();
        assert!(msg.contains("closed"));
        let left = store.list_pr_watches().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].opener_session_id.as_deref(), Some("sess-b"));
    }

    #[test]
    fn close_without_matching_watch_reports_nothing_to_close() {
        let (_d, store) = tmp_store();
        let msg = run_pr_watch(&store, true,
            PrWatchCliAction::Close { pr: "https://github.com/o/r/pull/7".into() },
            Some("sess-a".into())).unwrap();
        assert!(msg.contains("no watch"));
    }

    #[test]
    fn list_prints_watches() {
        let (_d, store) = tmp_store();
        run_pr_watch(&store, true,
            PrWatchCliAction::Open { pr: "https://github.com/o/r/pull/7".into() },
            Some("sess-a".into())).unwrap();
        let msg = run_pr_watch(&store, true, PrWatchCliAction::List, None).unwrap();
        assert!(msg.contains("o/r#7"));
        assert!(msg.contains("sess-a"));
    }
}

#[cfg(test)]
mod orchestrator_cli_tests {
    use super::{
        caller_is_orchestrator, orchestrator_context_footer, orchestrator_env_vars,
        reap_report_line, resolve_reap_orchestrator, run_spawn_orchestrator,
    };
    use ninox_core::events::ReapOutcome;
    use std::sync::Arc;

    fn store() -> Arc<ninox_core::store::Store> {
        Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        )
    }

    #[tokio::test]
    async fn spawn_orchestrator_refuses_without_the_user_requested_flag() {
        let store = store();
        let result = run_spawn_orchestrator(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "unrequested".into(),
            None,
            false,
        )
        .await;

        let err = result.expect_err("must refuse an unrequested orchestrator").to_string();
        assert!(err.contains("--user-requested"), "error must name the missing flag: {err}");
        assert!(
            store.list_orchestrators().unwrap().is_empty(),
            "a refused spawn must not leave an orchestrator record behind",
        );
        assert!(store.get_session("unrequested").unwrap().is_none());
    }

    #[tokio::test]
    async fn spawn_orchestrator_refuses_a_name_that_slugifies_to_nothing() {
        let result = run_spawn_orchestrator(
            store(),
            ninox_core::config::AppConfig::default(),
            "!!!".into(),
            None,
            true,
        )
        .await;
        assert!(result.is_err(), "a nameless orchestrator has no addressable session id");
    }

    #[tokio::test]
    async fn spawn_orchestrator_refuses_a_duplicate_name() {
        // A duplicate id would upsert over the existing record and then get
        // marked Terminated by the tmux-create failure — hijacking a live
        // session. Same guard the app's spawn modal applies.
        let store = store();
        store.upsert_session(&ninox_core::types::Session {
            id: "taken".into(), orchestrator_id: None, name: "taken".into(),
            repo: String::new(), status: ninox_core::SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
        }).unwrap();

        let result = run_spawn_orchestrator(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "Taken".into(),
            None,
            true,
        )
        .await;

        assert!(result.is_err(), "a colliding name must be refused");
        let survivor = store.get_session("taken").unwrap().unwrap();
        assert!(
            matches!(survivor.status, ninox_core::SessionStatus::Working),
            "the existing session must be left untouched, got {:?}", survivor.status,
        );
    }

    /// A failed spawn must leave NOTHING behind. Unlike a worker's
    /// Terminated row (which `sweep_retired_sessions` eventually purges), an
    /// orchestrator's row is skipped by the sweep forever — so a ghost would
    /// both linger in the store and permanently burn the name against the
    /// duplicate-name guard, with no CLI way to clear it.
    #[tokio::test]
    async fn failed_spawn_orchestrator_rolls_back_both_rows_so_the_name_is_reusable() {
        let store = store();
        let scratch = tempfile::tempdir().unwrap();
        let config = ninox_core::config::AppConfig {
            orchestrator_root: Some(scratch.path().join("root")),
            ..Default::default()
        };
        // A live tmux session under the same id forces `create_session` to
        // fail deterministically on any tmux build ("duplicate session"),
        // which is what `spawn_util`'s own failure test relies on too.
        let ws = scratch.path().join("occupied");
        std::fs::create_dir_all(&ws).unwrap();
        ninox_core::tmux::create_session(
            "ghost-orch", ws.to_str().unwrap(), "sleep 30", &[],
        ).await.unwrap();

        let result = run_spawn_orchestrator(
            store.clone(), config, "ghost orch".into(), None, true,
        ).await;

        ninox_core::tmux::kill_session("ghost-orch").await.ok();
        assert!(result.is_err(), "spawn must fail for a duplicate tmux session id");
        assert!(
            store.get_session("ghost-orch").unwrap().is_none(),
            "no session row may survive a failed spawn",
        );
        assert!(
            !store.list_orchestrators().unwrap().iter().any(|o| o.id == "ghost-orch"),
            "no orchestrator row may survive a failed spawn — the sweep never purges one",
        );
    }

    #[test]
    fn orchestrator_env_marks_the_session_as_an_orchestrator() {
        let env = orchestrator_env_vars("/bin/ninox", "/cfg.toml", "/brain", "orch-2", "/data");
        // Its own id, so workers it spawns report back to it.
        assert!(env.contains(&("NINOX_ORCHESTRATOR_ID", "orch-2")));
        // The caller type gates the subagent blocker and `ninox reap`.
        assert!(env.contains(&("NINOX_CALLER_TYPE", "orchestrator")));
        assert!(env.contains(&("NINOX_SESSION", "orch-2")));
        assert!(env.contains(&("NINOX_DATA_DIR", "/data")));
        assert!(env.contains(&("NINOX_BRAIN", "/brain")));
        assert!(env.contains(&("NINOX_CONFIG", "/cfg.toml")));
        assert!(env.contains(&("NINOX_BIN", "/bin/ninox")));
        // Its initial brief is delivered through `deliver_message`, so it
        // needs a messaging socket like every other spawned session — see
        // `ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV`.
        assert!(env.contains(&(ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV, "1")));
    }

    #[test]
    fn orchestrator_footer_names_the_spawner_as_the_report_back_channel() {
        let footer = orchestrator_context_footer("child", Some("parent"));
        assert!(footer.contains("`child`"), "must name the new orchestrator");
        assert!(footer.contains("ninox send parent"), "must route replies to the spawner");
        assert!(footer.contains("ninox spawn"), "must keep it coordinating, not implementing");
    }

    #[test]
    fn orchestrator_footer_omits_the_report_back_channel_with_no_spawner() {
        // Spawned from a plain terminal: there is no orchestrator to report
        // back to, so the footer must not invent one.
        let footer = orchestrator_context_footer("solo", None);
        assert!(footer.contains("`solo`"));
        assert!(!footer.contains("ninox send"), "no spawner means no report-back line: {footer}");
    }

    // ── reap guard ──────────────────────────────────────────────────────────

    fn orch(s: &str) -> Option<String> { Some(s.to_string()) }

    #[test]
    fn reap_guard_accepts_an_orchestrator_session() {
        let id = resolve_reap_orchestrator(None, orch("orch-1"), true, orch("orch-1")).unwrap();
        assert_eq!(id, "orch-1");
    }

    #[test]
    fn reap_guard_refuses_a_worker_session() {
        // A worker carries NINOX_ORCHESTRATOR_ID (its parent's) but no
        // caller type — that ambient id must not make its siblings reapable.
        let err = resolve_reap_orchestrator(None, orch("orch-1"), false, orch("w1"))
            .expect_err("a worker must not reap")
            .to_string();
        assert!(err.contains("cannot reap its siblings"), "{err}");
    }

    /// The bypass the reviewer found: `--orchestrator-id` is for out-of-session
    /// use, not an escape hatch. A worker passing its own parent's id would
    /// otherwise reap the whole sibling fleet — and itself, killing the pane
    /// running the reap and stranding a `Working` row with no worktree that
    /// nothing ever cleans up.
    #[test]
    fn reap_guard_refuses_a_worker_even_with_an_explicit_orchestrator_id() {
        let err = resolve_reap_orchestrator(orch("orch-1"), orch("orch-1"), false, orch("w1"))
            .expect_err("--orchestrator-id must not bypass the guard")
            .to_string();
        assert!(err.contains("with or without --orchestrator-id"), "{err}");
    }

    #[test]
    fn reap_guard_allows_an_explicit_id_outside_any_session() {
        // A human at a terminal / a script: no session env at all.
        let id = resolve_reap_orchestrator(orch("orch-7"), None, false, None).unwrap();
        assert_eq!(id, "orch-7");
    }

    #[test]
    fn reap_guard_needs_an_id_from_somewhere() {
        let err = resolve_reap_orchestrator(None, None, false, None)
            .expect_err("no id anywhere must fail")
            .to_string();
        assert!(err.contains("NINOX_ORCHESTRATOR_ID"), "{err}");
    }

    #[test]
    fn reap_guard_prefers_the_explicit_id_over_the_ambient_one() {
        let id = resolve_reap_orchestrator(
            orch("orch-explicit"), orch("orch-ambient"), true, orch("orch-ambient"),
        ).unwrap();
        assert_eq!(id, "orch-explicit");
    }

    /// The store, not the environment, decides who is an orchestrator.
    /// `NINOX_CALLER_TYPE` lives in the agent's own shell, so if it were
    /// trusted a worker could `export NINOX_CALLER_TYPE=orchestrator` and reap
    /// its whole fleet — including itself, where the kill takes down the pane
    /// running the reap and strands a row nothing ever cleans up.
    #[test]
    fn caller_type_env_cannot_promote_a_worker_to_an_orchestrator() {
        assert!(
            !caller_is_orchestrator(Some("w1"), &["orch-1"], Some("orchestrator")),
            "a spoofed caller type must not beat the store",
        );
    }

    #[test]
    fn a_session_id_that_is_an_orchestrator_row_is_an_orchestrator() {
        assert!(caller_is_orchestrator(Some("orch-1"), &["orch-1", "orch-2"], None));
    }

    #[test]
    fn caller_type_is_only_consulted_outside_a_session() {
        // A plain shell has no NINOX_SESSION to look up.
        assert!(caller_is_orchestrator(None, &[], Some("orchestrator")));
        assert!(!caller_is_orchestrator(None, &[], None));
    }

    #[test]
    fn reap_report_distinguishes_every_outcome() {
        assert_eq!(reap_report_line("w1", ReapOutcome::Reaped), "reaped w1");
        let killed = reap_report_line("w1", ReapOutcome::ReapedLive);
        assert!(killed.contains("still running"), "a killed worker must be called out: {killed}");
        let resumable = reap_report_line("w1", ReapOutcome::ReapedResumable);
        assert!(
            resumable.contains("no longer resumable"),
            "giving up resumability must be stated, not silent: {resumable}",
        );
        assert!(reap_report_line("w1", ReapOutcome::SkippedLive).contains("--force"));
        let skipped = reap_report_line("w1", ReapOutcome::SkippedResumable);
        assert!(skipped.contains("resumable") && skipped.contains("--force"), "{skipped}");
        assert!(reap_report_line("w1", ReapOutcome::NotFound).contains("not one of your workers"));

        // The merged-but-kept-alive cases must read as the intended cleanup,
        // naming the merge — not as "you'd interrupt work in progress".
        let skipped_merged = reap_report_line("w1", ReapOutcome::SkippedLiveMerged);
        assert!(
            skipped_merged.contains("merged") && skipped_merged.contains("--force"),
            "a skipped merged worker must say it merged and how to reap it: {skipped_merged}",
        );
        let reaped_merged = reap_report_line("w1", ReapOutcome::ReapedLiveMerged);
        assert!(
            reaped_merged.contains("merged") && !reaped_merged.contains("still running"),
            "a reaped merged worker must name the merge, not read as an interruption: {reaped_merged}",
        );
    }
}

#[cfg(test)]
mod capabilities_cli_tests {
    use super::*;
    use ninox_core::capabilities::{Audience, REGISTRY};

    #[test]
    fn lists_every_registry_entry_by_default() {
        let out = run_capabilities(&AppConfig::default(), None, false);
        assert_eq!(
            out.lines().count(),
            REGISTRY.len(),
            "one line per registry entry, no filter applied"
        );
        for cap in REGISTRY {
            assert!(out.contains(cap.name), "{} missing from output", cap.name);
        }
    }

    #[test]
    fn each_line_carries_a_status_and_a_description() {
        let out = run_capabilities(&AppConfig::default(), None, false);
        for line in out.lines() {
            assert!(
                line.contains("[enabled]") || line.contains("[disabled]"),
                "line must carry a status: {line}"
            );
            let after_status = line.split(']').nth(1).unwrap_or("").trim();
            assert!(!after_status.is_empty(), "line must carry a description: {line}");
        }
    }

    #[test]
    fn audience_filters_narrow_the_listing() {
        let cfg = AppConfig::default();
        let worker = run_capabilities(&cfg, Some(Audience::Worker), false);
        assert!(worker.contains("brain"));
        assert!(worker.contains("watch-pr"));
        assert!(!worker.contains("spawn-worker"), "worker listing must not show orchestrator-only skills");

        let orch = run_capabilities(&cfg, Some(Audience::Orchestrator), false);
        assert!(orch.contains("spawn-worker"));
        assert!(orch.contains("set-agent-config"));
    }

    /// The one gated capability: flipping `[pr_watch]` off must show the
    /// worker `watch-pr` entry as disabled rather than hiding it.
    #[test]
    fn disabled_toggle_marks_worker_watch_pr_disabled() {
        let mut cfg = AppConfig::default();
        cfg.pr_watch.enabled = false;
        let out = run_capabilities(&cfg, Some(Audience::Worker), false);
        let line = out.lines().find(|l| l.starts_with("watch-pr")).expect("watch-pr line");
        assert!(line.contains("[disabled]"), "expected disabled, got: {line}");

        cfg.pr_watch.enabled = true;
        let out = run_capabilities(&cfg, Some(Audience::Worker), false);
        let line = out.lines().find(|l| l.starts_with("watch-pr")).expect("watch-pr line");
        assert!(line.contains("[enabled]"), "expected enabled, got: {line}");
    }

    /// The orchestrator copy of `watch-pr` is ungated — it stays enabled
    /// even with `[pr_watch]` off (matching what gets seeded on disk).
    #[test]
    fn orchestrator_watch_pr_stays_enabled_with_pr_watch_off() {
        let mut cfg = AppConfig::default();
        cfg.pr_watch.enabled = false;
        let out = run_capabilities(&cfg, Some(Audience::Orchestrator), false);
        let line = out.lines().find(|l| l.starts_with("watch-pr")).expect("watch-pr line");
        assert!(line.contains("[enabled]"), "expected enabled, got: {line}");
    }

    #[test]
    fn json_output_parses_with_the_documented_shape() {
        let mut cfg = AppConfig::default();
        cfg.pr_watch.enabled = false;
        let out = run_capabilities(&cfg, None, true);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let arr = parsed.as_array().expect("top level array");
        assert_eq!(arr.len(), REGISTRY.len());
        for item in arr {
            assert!(item["name"].is_string());
            assert!(item["audience"].is_string());
            assert!(item["enabled"].is_boolean());
            assert!(item["description"].is_string());
        }
        let worker_watch = arr
            .iter()
            .find(|i| i["name"] == "watch-pr" && i["audience"] == "worker")
            .expect("worker watch-pr entry");
        assert_eq!(worker_watch["enabled"], serde_json::Value::Bool(false));
    }

    #[test]
    fn json_honors_the_audience_filter() {
        let out = run_capabilities(&AppConfig::default(), Some(Audience::Orchestrator), true);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = parsed.as_array().unwrap();
        assert!(arr.iter().all(|i| i["audience"] != "worker"));
        assert!(arr.iter().any(|i| i["name"] == "spawn-worker"));
    }
}

#[cfg(test)]
mod worker_status_cli_tests {
    use super::{resolve_session_ref, run_worker_status, WorkerStatusCliAction};
    use ninox_core::store::Store;
    use ninox_core::types::{ActivityState, DepKind, Session, SessionStatus};

    fn test_store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        std::mem::forget(dir);
        Store::open(path).unwrap()
    }

    fn seed(store: &Store, id: &str, name: &str, status: SessionStatus) {
        store.upsert_session(&Session {
            id: id.into(), orchestrator_id: None, name: name.into(),
            repo: "o/r".into(), status,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
        }).unwrap();
    }

    #[test]
    fn set_requires_an_identifiable_session() {
        let store = test_store();
        let err = run_worker_status(
            &store,
            WorkerStatusCliAction::Set { state: ActivityState::Blocked, note: None },
            None, 100,
        ).unwrap_err();
        assert!(err.to_string().contains("session"), "error must explain the identity failure: {err}");
    }

    #[test]
    fn set_writes_activity_through_apply_activity() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        let msg = run_worker_status(
            &store,
            WorkerStatusCliAction::Set {
                state: ActivityState::Blocked,
                note: Some("waiting on #12".into()),
            },
            Some("w1".into()), 100,
        ).unwrap();
        assert!(msg.contains("blocked"), "{msg}");
        let s = store.get_session("w1").unwrap().unwrap();
        assert_eq!(s.activity, ActivityState::Blocked);
        assert_eq!(s.activity_note.as_deref(), Some("waiting on #12"));
    }

    #[test]
    fn depend_creates_a_declared_edge_resolving_target_by_name() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "worker-two", SessionStatus::Working);
        run_worker_status(
            &store,
            WorkerStatusCliAction::Depend {
                target: "worker-two".into(), note: Some("needs its schema".into()), source: None,
            },
            Some("w1".into()), 100,
        ).unwrap();
        let deps = store.deps_for_session("w1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on, "w2");
        assert_eq!(deps[0].kind, DepKind::Declared);
    }

    #[test]
    fn depend_rejects_self_and_unknown_targets() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        assert!(run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "w1".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).is_err(), "self-dependency must be rejected");
        assert!(run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "nope".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).is_err(), "unknown target must be rejected");
    }

    #[test]
    fn depend_with_for_lets_an_orchestrator_declare_edges_between_workers() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "worker-two", SessionStatus::Working);
        run_worker_status(
            &store,
            WorkerStatusCliAction::Depend {
                target: "w2".into(), note: None, source: Some("worker-one".into()),
            },
            None, 100, // orchestrator context: no self worker session needed
        ).unwrap();
        let deps = store.deps_for_session("w1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on, "w2");
    }

    #[test]
    fn undepend_removes_the_edge_and_reports_when_absent() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "worker-two", SessionStatus::Working);
        run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "w2".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).unwrap();
        let removed = run_worker_status(
            &store,
            WorkerStatusCliAction::Undepend { target: "w2".into(), source: None },
            Some("w1".into()), 200,
        ).unwrap();
        assert!(removed.contains("removed"), "{removed}");
        assert!(store.deps_for_session("w1").unwrap().is_empty());

        let absent = run_worker_status(
            &store,
            WorkerStatusCliAction::Undepend { target: "w2".into(), source: None },
            Some("w1".into()), 300,
        ).unwrap();
        assert!(absent.contains("no declared dependency"), "{absent}");
    }

    #[test]
    fn list_reports_activity_and_edges_for_live_sessions_only() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "worker-two", SessionStatus::Working);
        seed(&store, "dead", "worker-dead", SessionStatus::Terminated);
        run_worker_status(
            &store,
            WorkerStatusCliAction::Set { state: ActivityState::Blocked, note: Some("stuck".into()) },
            Some("w1".into()), 100,
        ).unwrap();
        run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "w2".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).unwrap();

        let text = run_worker_status(&store, WorkerStatusCliAction::List { json: false }, None, 200).unwrap();
        assert!(text.contains("worker-one") && text.contains("blocked") && text.contains("worker-two"), "{text}");
        assert!(!text.contains("worker-dead"), "terminal sessions must not be listed: {text}");

        let json = run_worker_status(&store, WorkerStatusCliAction::List { json: true }, None, 200).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).expect("list --json must emit valid JSON");
        let arr = v.as_array().expect("top level must be an array");
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn depend_rejects_terminal_targets_even_by_exact_id() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "dead", "worker-dead", SessionStatus::Terminated);
        let err = run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "dead".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).unwrap_err();
        assert!(err.to_string().contains("no longer live"), "{err}");
        assert!(store.deps_for_session("w1").unwrap().is_empty());
    }

    #[test]
    fn undepend_still_works_against_a_terminal_target() {
        // The whole point of undepend is cleaning up edges whose target is
        // gone — it must not apply depend's liveness validation.
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "worker-two", SessionStatus::Working);
        run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "w2".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).unwrap();
        seed(&store, "w2", "worker-two", SessionStatus::Terminated);
        let msg = run_worker_status(
            &store,
            WorkerStatusCliAction::Undepend { target: "w2".into(), source: None },
            Some("w1".into()), 200,
        ).unwrap();
        assert!(msg.contains("removed"), "{msg}");
    }

    #[test]
    fn set_errors_rather_than_claiming_success_on_a_dead_session() {
        // A session can go terminal between identity resolution and the
        // write (or a raced NINOX_SESSION can name a dead row) — the agent
        // must not be told its note was recorded when it wasn't.
        let store = test_store();
        seed(&store, "dead", "worker-dead", SessionStatus::Terminated);
        let err = run_worker_status(
            &store,
            WorkerStatusCliAction::Set { state: ActivityState::Blocked, note: Some("n".into()) },
            Some("dead".into()), 100,
        ).unwrap_err();
        assert!(err.to_string().contains("no longer live"), "{err}");
    }

    #[test]
    fn list_names_dependency_targets_even_after_they_finish() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "worker-two", SessionStatus::Working);
        run_worker_status(
            &store,
            WorkerStatusCliAction::Depend { target: "w2".into(), note: None, source: None },
            Some("w1".into()), 100,
        ).unwrap();
        seed(&store, "w2", "worker-two", SessionStatus::Done);

        let text = run_worker_status(&store, WorkerStatusCliAction::List { json: false }, None, 200).unwrap();
        assert!(
            text.contains("depends on worker-two"),
            "a finished dependency must keep its human-readable name, not decay to a raw id: {text}",
        );
    }

    #[test]
    fn resolve_session_ref_prefers_exact_id_then_unique_name() {
        let store = test_store();
        seed(&store, "w1", "worker-one", SessionStatus::Working);
        seed(&store, "w2", "w1", SessionStatus::Working); // a session *named* like another's id
        assert_eq!(resolve_session_ref(&store, "w1").unwrap(), "w1", "exact id wins over name");
        assert_eq!(resolve_session_ref(&store, "worker-one").unwrap(), "w1");
        assert!(resolve_session_ref(&store, "ghost").is_err());
    }

    #[test]
    fn resolve_session_ref_rejects_ambiguous_names() {
        let store = test_store();
        seed(&store, "a", "twin", SessionStatus::Working);
        seed(&store, "b", "twin", SessionStatus::Working);
        let err = resolve_session_ref(&store, "twin").unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    use ninox_core::types::{Session, SessionStatus};

    /// Serializes tests that mutate process-global env vars (`NINOX_CONFIG`)
    /// against each other — `cargo test` runs test fns on parallel threads,
    /// so without this guard one test's env mutation could leak into
    /// another's read.
    pub(crate) static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `key=value` for the duration of `f`, restoring the prior value
    /// (or unsetting it) afterward. Serialized via `ENV_TEST_GUARD` since
    /// env vars are process-global state shared across parallel test
    /// threads. Mirrors `ninox_core::config::tests::with_env_override`.
    pub(crate) fn with_env_override<T>(
        key: &str,
        value: impl AsRef<std::ffi::OsStr>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        match prior {
            Some(v) => std::env::set_var(key, v),
            None    => std::env::remove_var(key),
        }
        result.unwrap()
    }

    pub(crate) fn session(id: &str, orch: Option<&str>, status: SessionStatus) -> Session {
        Session {
            id: id.into(),
            orchestrator_id: orch.map(String::from),
            name: id.into(),
            repo: "owner/repo".into(),
            status,
            agent_type: "claude-code".into(),
            cost_usd: 1.5,
            started_at: 0,
            pr_number: Some(42),
            pr_id: None,
            workspace_path: None,
            pid: None,
            model: None,
            context_tokens: None,
            catalogue_path: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None,
            gate_status: None,
            merged_at: None,
            activity: ninox_core::ActivityState::Unknown,
            activity_note: None,
            activity_since: None,
        }
    }
}

#[cfg(test)]
mod list_sessions_tests {
    use super::{group_sessions, render_session_board, test_fixtures};
    use ninox_core::types::{Orchestrator, SessionStatus};

    #[test]
    fn workers_group_under_their_orchestrator() {
        let orch = Orchestrator { id: "boss".into(), name: "Boss".into(), created_at: 0 };
        // The orchestrator's own session row shares its id.
        let rows = group_sessions(
            vec![
                test_fixtures::session("boss", None, SessionStatus::Working),
                test_fixtures::session("w1", Some("boss"), SessionStatus::Working),
                test_fixtures::session("stray", Some("gone-orch"), SessionStatus::Terminated),
            ],
            vec![orch],
        );
        assert_eq!(rows.len(), 2); // boss group + ungrouped
        assert_eq!(rows[0].0.as_ref().unwrap().id, "boss");
        assert_eq!(rows[0].1.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["boss", "w1"]);
        assert!(rows[1].0.is_none());
        assert_eq!(rows[1].1[0].id, "stray");
    }

    #[test]
    fn board_renders_status_repo_pr_and_cost() {
        let orch = Orchestrator { id: "boss".into(), name: "Boss".into(), created_at: 0 };
        let out = render_session_board(&group_sessions(
            vec![
                test_fixtures::session("boss", None, SessionStatus::Working),
                test_fixtures::session("w1", Some("boss"), SessionStatus::PrOpen),
            ],
            vec![orch],
        ));
        assert!(out.contains("boss"), "{out}");
        assert!(out.contains("w1"), "{out}");
        assert!(out.contains("PR #42"), "{out}");
        assert!(out.contains("$1.50"), "{out}");
        assert!(out.contains("pr_open"), "{out}");
    }

    #[test]
    fn empty_store_prints_hint() {
        let out = render_session_board(&group_sessions(vec![], vec![]));
        assert!(out.contains("no sessions"), "{out}");
        assert!(out.contains("ninox orchestrate"), "{out}");
    }
}
