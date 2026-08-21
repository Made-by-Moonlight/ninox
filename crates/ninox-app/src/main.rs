mod app;
mod components;
mod input;
mod models;
mod spawn_util;
mod style;
mod theme;

use anyhow::Context as _;
use spawn_util::{
    acquire_worker_checkout_for_incarnation, repo_from_workspace, rollback_worker_checkout,
    seed_worker_brain_skill,
};
use ninox_core::{
    config::AppConfig,
    events::Engine,
    github::resolve_token,
    lifecycle::{poller::Poller, repo_discovery},
    slugify,
    store::Store,
    tmux,
    types::{Session, SessionStatus},
    BrainIndex, QueryFilters,
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
    /// settings only when `[inbox_messaging].enabled = true` — see
    /// `ninox_core::inbox`). Invoked by Claude Code's own Stop/
    /// UserPromptSubmit hooks, not intended for direct use.
    Inbox {
        #[command(subcommand)]
        action: InboxAction,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WorkerDelivery {
    Pr,
    Direct,
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
    let command = args.command;

    // Fires on every assistant turn (event-driven) or every `refreshInterval`
    // seconds for every session Ninox spawns — must stay fast and never
    // trigger the tmux-config/wrapper-hook/self-shim setup below, none of
    // which this subcommand needs.
    if matches!(command, Some(Command::Statusline)) {
        run_statusline(args.db.unwrap_or_else(default_db_path));
        return Ok(());
    }

    // Fires on every Stop/UserPromptSubmit turn of every worker with inbox
    // messaging enabled — same "stay fast, skip the heavy setup" reasoning
    // as Statusline above; this subcommand needs none of it either.
    if let Some(Command::Inbox { action }) = command {
        run_inbox(action);
        return Ok(());
    }

    if let Err(e) = tmux::write_server_config() {
        eprintln!("failed to write tmux config: {e}");
    }

    if let Err(e) = ninox_core::hooks::install_wrappers() {
        tracing::warn!("failed to install wrapper hooks: {e}");
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Err(e) = ninox_core::hooks::install_self_shim(&exe) {
            tracing::warn!("failed to install ninox self-shim: {e}");
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
            run_release(store, &session_id, orchestrator_id).await
        }
        Some(Command::Send { session_id, message }) => {
            let config = AppConfig::load().unwrap_or_default();
            let sessions_dir = std::env::var("NINOX_DATA_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(AppConfig::sessions_dir);
            ninox_core::messaging::deliver_message(
                &store, &sessions_dir, &session_id, &message, config.inbox_messaging.enabled,
            )
            .await
        }
        Some(Command::RequestWork { description }) => {
            run_request_work(&description)
        }
        Some(Command::Brain { action }) => {
            run_brain(action, store).await
        }
        Some(Command::Statusline) => {
            run_statusline(db_path);
            Ok(())
        }
        Some(Command::Inbox { action }) => {
            run_inbox(action);
            Ok(())
        }
        None => run_tui(store, args.port, args.headless).await,
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
    reject_recursive_worker_spawn(std::env::var("NINOX_CALLER_TYPE").ok().as_deref())?;
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
    let checkout_backed =
        ninox_core::worktree::RepositoryIdentity::resolve(std::path::Path::new(&workspace)).is_ok();
    let checkout_cap = config.validated_worker_checkout_cap()?;
    let incarnation = match store.prepare_worker_incarnation(
        &id,
        orchestrator_id.as_deref(),
        ts,
        &workspace,
        checkout_backed,
        checkout_cap,
    ) {
        Ok(incarnation) => incarnation,
        Err(error) if error.to_string().contains("worker cap reached") => {
            let candidates = store.checkout_worker_candidates(orchestrator_id.as_deref())?;
            anyhow::bail!(
                "{error}. {}",
                release_candidate_guidance(orchestrator_id.as_deref(), &candidates)
            )
        }
        Err(error) => return Err(error),
    };
    let mut pending = Session {
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
    };
    if let Err(error) = store.upsert_session(&pending) {
        rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            None,
        )
        .await;
        return Err(error);
    }

    let repositories_root = config.resolved_repositories_root();
    let checkout = match acquire_worker_checkout_for_incarnation(
        store.clone(),
        &workspace,
        &id,
        &incarnation.incarnation_id,
        repositories_root.as_deref(),
        &config.resolved_worktree_root(),
        config.inbox_messaging.enabled,
    )
    .await
    {
        Ok(checkout) => Some(checkout),
        Err(error) => {
            pending.status = SessionStatus::Terminated;
            pending.workspace_path = Some(workspace.clone());
            let _ = store.upsert_session(&pending);
            rollback_worker_incarnation(
                store.clone(),
                &id,
                &incarnation.incarnation_id,
                None,
            )
            .await;
            return Err(error);
        }
    };
    let effective_workspace = checkout
        .as_ref()
        .map_or_else(|| workspace.clone(), |checkout| checkout.workspace.clone());
    let canonical_source_workspace = checkout
        .as_ref()
        .map_or_else(|| workspace.clone(), |checkout| checkout.source_workspace.clone());
    pending.workspace_path = Some(effective_workspace.clone());
    if let Err(error) = store.upsert_session(&pending) {
        rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        return Err(error);
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

    if let Err(e) = seed_worker_brain_skill(&effective_workspace).await {
        tracing::warn!("failed to seed brain skill for {id}: {e}");
    }

    // Derive the GitHub repo slug from the workspace's git remote so that
    // poll_github can call the GitHub API with the correct owner/repo.
    let repo = repo_from_workspace(&workspace).unwrap_or_default();

    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    let ninox_bin = ninox_core::config::AppConfig::ninox_bin_dir();
    let ninox_bin_str = ninox_bin.display().to_string();

    let orch_id_env = orchestrator_id.as_deref().unwrap_or("").to_string();

    // Append worker context so every agent knows its session ID, delivery
    // contract, orchestrator ID, and how to communicate back when done.
    let mut effective_prompt = worker_prompt_for_canonical_workspace(
        &prompt,
        &workspace,
        &canonical_source_workspace,
        &effective_workspace,
        delivery,
    );
    if !orch_id_env.is_empty() {
        effective_prompt.push_str(&worker_context_footer(&id, &orch_id_env, delivery));
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
        terminal_at: None, gate_status: None,
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
    println!("spawned {}", session.id);

    // Prepend the ninox bin dir inside the shell command rather than via tmux
    // -e PATH=..., because the login shell (-l) sources rc files that may
    // re-prepend Homebrew or nvm directories, pushing our wrapper behind the
    // real `gh`. By exporting PATH here we win the race after rc files run.
    let cmd_base = registry
        .worker_cmd(&agent, &effective_prompt, &claude_session_id)
        .expect("worker-capability checked before any side effect above");
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

    let env_vec = worker_env_vars(
        &id,
        &sessions_dir_str,
        &orch_id_env,
        ninox_brain_env.as_deref(),
        ninox_config_env.as_deref(),
    );
    // The session was already upserted as Working above; if tmux refuses
    // the spawn (e.g. the workspace dir doesn't exist — create_session
    // rejects that rather than letting the pane silently start in $HOME),
    // mark it Terminated before bailing. A pid-less Working ghost is
    // invisible to poll_pids and would linger until the next app restart's
    // reconciliation.
    if let Err(e) = tmux::create_session(&id, &effective_workspace, &cmd, &env_vec).await {
        rollback_worker_incarnation(
            store.clone(),
            &id,
            &incarnation.incarnation_id,
            checkout.as_ref(),
        )
        .await;
        if let Ok(Some(mut s)) = store.get_session(&id) {
            s.status = SessionStatus::Terminated;
            s.workspace_path = Some(workspace.clone());
            let _ = store.upsert_session(&s);
        }
        return Err(e);
    }

    Ok(())
}

async fn rollback_worker_incarnation(
    store: Arc<Store>,
    session_id: &str,
    incarnation_id: &str,
    checkout: Option<&spawn_util::WorkerCheckout>,
) {
    if let Ok(Some(claim)) = store.claim_worker_cleanup(session_id, incarnation_id) {
        if let Some(checkout) = checkout {
            let _ = rollback_worker_checkout(store.clone(), checkout, session_id).await;
        }
        let _ = store.complete_worker_claim(
            session_id,
            &claim.incarnation_id,
            ninox_core::types::WorkerIncarnationState::CleanupClaimed,
        );
    }
}

fn reject_recursive_worker_spawn(caller_type: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        caller_type != Some("worker"),
        "worker sessions cannot recursively spawn workers; return the request to the orchestrator"
    );
    Ok(())
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

/// Make the allocated workspace authoritative even when the source checkout's
/// absolute path appears in the task prompt.
#[cfg(test)]
fn worker_prompt_for_workspace(
    prompt: &str,
    source_workspace: &str,
    worker_workspace: &str,
    delivery: WorkerDelivery,
) -> String {
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
        source_workspace,
        &canonical_source,
        worker_workspace,
        delivery,
    )
}

fn worker_prompt_for_canonical_workspace(
    prompt: &str,
    source_workspace: &str,
    canonical_source: &str,
    worker_workspace: &str,
    delivery: WorkerDelivery,
) -> String {
    let remapped = remap_workspace_paths(
        prompt,
        workspace_path_mappings(source_workspace, canonical_source, worker_workspace),
        worker_workspace,
    )
    .expect("workspace remapping must not retain source-checkout aliases");
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
    format!(
        "{remapped}\n\n---\n\
         **Ninox workspace:** `{worker_workspace}` is the authoritative workspace. \
         {workspace_contract}{source_note}"
    )
}

fn workspace_path_mappings(
    supplied_source_workspace: &str,
    canonical_source_workspace: &str,
    worker_workspace: &str,
) -> Vec<(String, String)> {
    let canonical_supplied = std::path::Path::new(supplied_source_workspace)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(supplied_source_workspace));
    let canonical_root = std::path::Path::new(canonical_source_workspace)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(canonical_source_workspace));
    let relative = canonical_supplied
        .strip_prefix(&canonical_root)
        .unwrap_or_else(|_| std::path::Path::new(""));
    let supplied_target = std::path::Path::new(worker_workspace).join(relative);
    let mut mappings = vec![
        (
            supplied_source_workspace.to_string(),
            supplied_target.to_string_lossy().into_owned(),
        ),
        (
            canonical_supplied.to_string_lossy().into_owned(),
            supplied_target.to_string_lossy().into_owned(),
        ),
        (
            canonical_source_workspace.to_string(),
            worker_workspace.to_string(),
        ),
        (
            canonical_root.to_string_lossy().into_owned(),
            worker_workspace.to_string(),
        ),
    ];
    mappings.retain(|(source, target)| !source.is_empty() && source != target);
    mappings.sort_unstable_by_key(|(source, _)| std::cmp::Reverse(source.len()));
    mappings.dedup_by(|left, right| left.0 == right.0);
    mappings
}

fn remap_workspace_paths(
    prompt: &str,
    mappings: Vec<(String, String)>,
    worker_workspace: &str,
) -> anyhow::Result<String> {
    use anyhow::Context as _;

    let mut mappings = mappings
        .into_iter()
        .map(|(source, target)| {
            let source = normalize_absolute_path(std::path::Path::new(&source))
                .context("normalize source workspace mapping")?;
            let target = normalize_absolute_path(std::path::Path::new(&target))
                .context("normalize worker workspace mapping")?;
            Ok((source, target))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    mappings.sort_unstable_by_key(|(source, _)| {
        std::cmp::Reverse(source.components().count())
    });
    let worker_root = normalize_absolute_path(std::path::Path::new(worker_workspace))
        .context("normalize authoritative worker workspace")?;
    let source_roots = mappings
        .iter()
        .map(|(source, _)| source.clone())
        .collect::<Vec<_>>();
    let mut output = String::with_capacity(prompt.len());
    let mut cursor = 0;
    while let Some((start, end)) = next_workspace_path_token(prompt, cursor) {
        output.push_str(&prompt[cursor..start]);
        let token = &prompt[start..end];
        let (path_token, punctuation) = split_path_token_punctuation(token);
        let (normalized, render_prefix, escaped_spaces, file_uri) =
            normalize_prompt_path_token(prompt, start, path_token, &worker_root)
                .with_context(|| format!("normalize workspace path token {token}"))?;
        if let Some((source, target)) = mappings
            .iter()
            .find(|(source, _)| normalized.strip_prefix(source).is_ok())
        {
            let relative = normalized.strip_prefix(source)?;
            let mapped = normalize_absolute_path(&target.join(relative))
                .context("normalize remapped worker path")?;
            anyhow::ensure!(
                mapped.starts_with(&worker_root),
                "workspace path remap escaped authoritative worker workspace"
            );
            output.push_str(render_prefix);
            let mapped = mapped.to_string_lossy();
            if file_uri {
                output.push_str(&percent_encode_file_uri_path(&mapped));
            } else if escaped_spaces {
                output.push_str(&mapped.replace(' ', "\\ "));
            } else {
                output.push_str(&mapped);
            }
            output.push_str(punctuation);
        } else {
            output.push_str(token);
        }
        cursor = end;
    }
    output.push_str(&prompt[cursor..]);

    let mut cursor = 0;
    while let Some((start, end)) = next_workspace_path_token(&output, cursor) {
        let token = &output[start..end];
        let (path_token, _) = split_path_token_punctuation(token);
        let (normalized, _, _, _) =
            normalize_prompt_path_token(&output, start, path_token, &worker_root)
                .with_context(|| format!("validate remapped workspace path token {token}"))?;
        anyhow::ensure!(
            !source_roots.iter().any(|source| {
                source != &worker_root && normalized.starts_with(source)
            }),
            "prompt retains a normalized source-checkout path after remapping: {token}"
        );
        cursor = end;
    }
    Ok(output)
}

fn split_path_token_punctuation(token: &str) -> (&str, &str) {
    if token.ends_with('.') && !token.ends_with("/.") && !token.ends_with("/..") {
        (&token[..token.len() - 1], ".")
    } else {
        (token, "")
    }
}

fn normalize_absolute_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Some(normalized)
}

fn normalize_prompt_path_token<'a>(
    input: &'a str,
    start: usize,
    token: &str,
    worker_root: &std::path::Path,
) -> Option<(std::path::PathBuf, &'a str, bool, bool)> {
    let file_scheme = input
        .as_bytes()
        .get(..start)?
        .get(start.saturating_sub("file:".len())..)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case(b"file:"));
    let localhost = token
        .as_bytes()
        .get(.."//localhost".len())
        .is_some_and(|authority| authority.eq_ignore_ascii_case(b"//localhost"))
        && token.as_bytes().get("//localhost".len()) == Some(&b'/');
    let (path, render_prefix) = if file_scheme && localhost {
        (&token["//localhost".len()..], "//localhost")
    } else if file_scheme && token.starts_with("///") {
        (token, "//")
    } else {
        (token, "")
    };
    let file_uri = !render_prefix.is_empty();
    let escaped_spaces = path.contains("\\ ");
    let decoded = if file_uri {
        percent_decode_file_uri_path(path)?
    } else {
        path.replace("\\ ", " ")
    };
    let path = std::path::Path::new(&decoded);
    let normalized = if path.is_absolute() {
        normalize_absolute_path(path)?
    } else {
        normalize_absolute_path(&worker_root.join(path))?
    };
    Some((normalized, render_prefix, escaped_spaces, file_uri))
}

fn percent_decode_file_uri_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] != b'%' {
            decoded.push(bytes[cursor]);
            cursor += 1;
            continue;
        }
        let value = hex_value(*bytes.get(cursor + 1)?)? * 16
            + hex_value(*bytes.get(cursor + 2)?)?;
        if value == 0 {
            return None;
        }
        decoded.push(value);
        cursor += 3;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_file_uri_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

fn next_workspace_path_token(input: &str, from: usize) -> Option<(usize, usize)> {
    for (relative, character) in input[from..].char_indices() {
        let start = from + relative;
        if is_workspace_path_delimiter(character) {
            continue;
        }
        let before = input[..start].chars().next_back();
        if !before.is_none_or(is_workspace_path_delimiter) {
            continue;
        }
        let quote = before.filter(|character| matches!(character, '\'' | '"' | '`'));
        let end = workspace_path_token_end(input, start, quote);
        let candidate = &input[start..end];
        let relative_path = !candidate.starts_with('/')
            && candidate.contains('/')
            && candidate
                .split('/')
                .any(|component| matches!(component, "." | ".."));
        if character != '/' && !relative_path {
            continue;
        }
        return Some((start, end));
    }
    None
}

fn workspace_path_token_end(input: &str, start: usize, quote: Option<char>) -> usize {
    let mut escaped = false;
    input[start..]
        .char_indices()
        .skip(1)
        .find_map(|(offset, character)| {
            if escaped {
                escaped = false;
                return None;
            }
            if character == '\\' {
                escaped = true;
                return None;
            }
            if quote.is_some_and(|quote| character == quote) {
                return Some(start + offset);
            }
            (quote.is_none() && is_workspace_path_delimiter(character))
                .then_some(start + offset)
        })
        .unwrap_or(input.len())
}

fn is_workspace_path_delimiter(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '`' | '\'' | '"' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | ':'
                | '!' | '?' | '=' | '@'
        )
}

fn worker_context_footer(
    id: &str,
    orch_id: &str,
    delivery: WorkerDelivery,
) -> String {
    match delivery {
        WorkerDelivery::Pr if !orch_id.is_empty() => pr_worker_context_footer(id, orch_id),
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
                    "Report back with a blocker-or-completion handoff:\n\
                     ```bash\n\
                     ninox send {orch_id} \"<blocked and needs a decision, or complete with artifacts/direct changes and validation>\"\n\
                     ```\n\
                     Stop after reporting that you are blocked or the direct delivery is complete."
                )
            },
        ),
    }
}

/// The context footer appended to every worker's task prompt: its own
/// session id, its orchestrator's id, the channels back to the orchestrator,
/// and the one-worker-one-PR scope rule.
fn pr_worker_context_footer(id: &str, orch_id: &str) -> String {
    format!(
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
         Report back when: (a) you are blocked and need a decision, \
         or (b) the PR is open and the task is done.",
    )
}

/// The tmux env for a spawned worker: always the session id + data dir, plus
/// whichever of orchestrator id / brain path / config path are actually
/// present (an empty `orch_id` or `None` env value is omitted rather than
/// forwarded as an empty string).
fn worker_env_vars<'a>(
    id: &'a str,
    sessions_dir: &'a str,
    orch_id: &'a str,
    ninox_brain: Option<&'a str>,
    ninox_config: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut env_vec: Vec<(&str, &str)> = vec![
        ("NINOX_SESSION",  id),
        ("NINOX_CALLER_TYPE", "worker"),
        ("NINOX_DATA_DIR", sessions_dir),
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

/// `ninox release` — hand a finalized, clean worker checkout back to the
/// pool for warm reuse by a later worker on the same repository.
async fn run_release(
    store: Arc<Store>,
    session_id: &str,
    orchestrator_id: Option<String>,
) -> anyhow::Result<()> {
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let env_session = env("NINOX_SESSION");
    let orchestrators = store.list_orchestrators()?;
    let orchestrator_ids: Vec<&str> =
        orchestrators.iter().map(|orchestrator| orchestrator.id.as_str()).collect();
    let caller = caller_is_orchestrator(
        env_session.as_deref(),
        &orchestrator_ids,
        env("NINOX_CALLER_TYPE").as_deref(),
    );
    let orchestrator_id = resolve_release_orchestrator(
        orchestrator_id,
        env("NINOX_ORCHESTRATOR_ID"),
        caller,
        env_session,
    )?;
    let session = store
        .get_session(session_id)?
        .with_context(|| format!("no worker named {session_id}"))?;
    anyhow::ensure!(
        session.orchestrator_id.as_deref() == Some(orchestrator_id.as_str()),
        "worker {session_id} is not owned by orchestrator {orchestrator_id}"
    );
    let worker = store
        .current_worker_incarnation(session_id)?
        .with_context(|| format!("worker {session_id} has no durable checkout capability"))?;
    anyhow::ensure!(
        worker.orchestrator_id.as_deref() == Some(orchestrator_id.as_str()),
        "worker checkout ownership does not match orchestrator {orchestrator_id}"
    );
    let _release_lock = lock_worker_release(&store, &worker)?;
    let worker = store
        .current_worker_incarnation(session_id)?
        .with_context(|| format!("worker {session_id} release completed concurrently"))?;
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
    let result = release_retained_worker_checkout(store.clone(), &claim).await;
    match result {
        Ok(()) => {
            anyhow::ensure!(
                store.complete_worker_claim(
                    session_id,
                    &claim.incarnation_id,
                    ninox_core::types::WorkerIncarnationState::ReleaseClaimed,
                )?,
                "worker release completion lost its exact incarnation claim"
            );
            println!("released {session_id}; preserved branch/ref and warm cache");
            Ok(())
        }
        Err(error) => {
            let _ = store.abort_worker_claim(
                session_id,
                &claim.incarnation_id,
                ninox_core::types::WorkerIncarnationState::ReleaseClaimed,
                ninox_core::types::WorkerIncarnationState::Retained,
            );
            Err(error)
        }
    }
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
/// `ninox request-work` — record a work request in this worker's session
/// metadata. The engine's poller notices it within one tick, notifies the
/// UI, and forwards it to the orchestrator's terminal.
fn run_request_work(description: &str) -> anyhow::Result<()> {
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
fn run_inbox(action: InboxAction) {
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
    let sessions_dir = std::env::var("NINOX_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(AppConfig::sessions_dir);

    let response = match action {
        InboxAction::DrainStop   => ninox_core::inbox::drain_for_stop(&sessions_dir, &session_id),
        InboxAction::DrainPrompt => ninox_core::inbox::drain_for_prompt_submit(&sessions_dir, &session_id),
    };
    match response {
        Ok(Some(json)) => println!("{json}"),
        Ok(None) => {}
        Err(e) => eprintln!("ninox inbox: {e}"),
    }
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
            terminal_at: None, gate_status: None,
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
    match ninox_core::embeddings::FastEmbedEmbedder::try_new() {
        Ok(embedder) => Some(Arc::new(embedder)),
        Err(err) => {
            tracing::warn!("brain: embedding model unavailable, falling back to keyword-only search: {err}");
            None
        }
    }
}

async fn run_tui(store: Arc<Store>, port_arg: Option<u16>, headless: bool) -> anyhow::Result<()> {
    let config = AppConfig::load().unwrap_or_default();
    let port = port_arg.unwrap_or(config.port);
    let orchestrator_root = config.resolved_orchestrator_root();
    let orchestrator_agent = config.orchestrator.clone();
    let config_path = AppConfig::config_path().to_string_lossy().to_string();
    let brain_path = config.resolved_brain_path();

    let ninox_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ninox".to_string());

    if let Err(e) = app::setup_orchestrator_root(&orchestrator_root, &ninox_bin, &config_path).await {
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

    let poller = Poller::new(engine.clone());
    tokio::spawn({
        let t = token.clone();
        async move { poller.start(t).await }
    });

    tokio::spawn({
        let e = engine.clone();
        let b = brain.clone();
        let emb = embedder.clone();
        async move {
            if let Err(err) = ninox_server::start(e, b, emb, port).await {
                tracing::error!("server: {err}");
            }
        }
    });

    tracing::info!("ninox ready on :{port}");

    if headless || !has_display() {
        tokio::signal::ctrl_c().await?;
        token.cancel();
        return Ok(());
    }

    if let Err(e) = tmux::require_version().await {
        eprintln!("{e}");
        std::process::exit(1);
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

fn has_display() -> bool {
    #[cfg(target_os = "macos")]
    { true }
    #[cfg(not(target_os = "macos"))]
    { std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() }
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
        let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Pr);
        assert!(footer.contains("`w1`"), "must name the worker's own session");
        assert!(footer.contains("ninox send orch1"), "must keep the message-back channel");
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
            worker_context_footer("w1", "orch1", WorkerDelivery::Pr),
            pr_worker_context_footer("w1", "orch1"),
        );
    }

    #[test]
    fn direct_worker_contract_has_no_pr_delivery_workflow() {
        let footer = worker_context_footer("w1", "orch1", WorkerDelivery::Direct);
        assert!(footer.contains("validated artifacts or direct changes"));
        assert!(footer.contains("blocked") && footer.contains("complete"));
        assert!(!footer.contains("complete the task and open a pull request"));
        assert!(!footer.contains("one worker, one task, one pull request"));
        assert!(!footer.contains("ninox open --pr"));
        assert!(!footer.contains("ninox close --pr"));
    }

    #[test]
    fn direct_worker_without_orchestrator_still_gets_no_git_delivery_contract() {
        let footer = worker_context_footer("w1", "", WorkerDelivery::Direct);

        assert!(footer.contains("Do not create branches"));
        assert!(footer.contains("do not push"));
        assert!(footer.contains("open pull requests"));
        assert!(!footer.contains("ninox send "));
        assert!(!footer.contains("ninox request-work"));
    }

    #[test]
    fn worker_prompt_remaps_source_checkout_paths_to_the_managed_worktree() {
        let prompt =
            "Edit /Users/mu/dev/repo/src/main.rs, then run git in /Users/mu/dev/repo.";
        let mapped = worker_prompt_for_workspace(
            prompt,
            "/Users/mu/dev/repo",
            "/Users/mu/dev/_wts/repo/worker-1",
            WorkerDelivery::Pr,
        );

        assert!(!mapped.contains("/Users/mu/dev/repo"));
        assert_eq!(
            mapped.matches("/Users/mu/dev/_wts/repo/worker-1").count(),
            3,
        );
        assert!(mapped.contains("do not read, write, or run Git commands there"));
    }

    #[cfg(unix)]
    #[test]
    fn worker_prompt_maps_nested_symlink_aliases_by_path_components() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("repo");
        let alias = parent.path().join("repo-alias");
        let worker = parent.path().join("repo-w1");
        std::fs::create_dir_all(source.join("src/nested")).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", source.to_str().unwrap(), "init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        symlink(&source, &alias).unwrap();

        let supplied = alias.join("src/nested");
        let prompt = format!(
            "Edit {}/deep.rs and {}/root.rs; preserve {}-archive/root.rs.",
            supplied.display(),
            source.display(),
            source.display(),
        );
        let mapped = worker_prompt_for_canonical_workspace(
            &prompt,
            supplied.to_str().unwrap(),
            source.to_str().unwrap(),
            worker.to_str().unwrap(),
            WorkerDelivery::Pr,
        );

        assert!(
            mapped.contains(&format!("{}/src/nested/deep.rs", worker.display())),
            "{mapped}"
        );
        assert!(
            mapped.contains(&format!("{}/root.rs", worker.display())),
            "{mapped}"
        );
        assert!(mapped.contains(&format!("{}-archive/root.rs", source.display())));
        assert!(!mapped.contains(alias.to_str().unwrap()));
    }

    #[test]
    fn direct_worker_workspace_prompt_avoids_git_workflow_guidance() {
        let prompt = worker_prompt_for_workspace(
            "Write the incident report.",
            "/tmp/research",
            "/tmp/research",
            WorkerDelivery::Direct,
        );

        assert!(prompt.contains("write artifacts or direct changes"));
        assert!(!prompt.contains("Git command"));
        assert!(!prompt.contains("repository read"));
    }

    #[test]
    fn forwards_brain_and_config_when_present() {
        let env = worker_env_vars("w1", "/data", "orch1", Some("/brain.db"), Some("/cfg.toml"));
        assert!(env.contains(&("NINOX_ORCHESTRATOR_ID", "orch1")));
        assert!(env.contains(&("NINOX_BRAIN", "/brain.db")));
        assert!(env.contains(&("NINOX_CONFIG", "/cfg.toml")));
        assert!(env.contains(&("NINOX_SESSION", "w1")));
        assert!(env.contains(&("NINOX_CALLER_TYPE", "worker")));
        assert!(env.contains(&("NINOX_DATA_DIR", "/data")));
        // The legacy ATHENE_* transition names are gone.
        assert!(!env.iter().any(|(k, _)| k.starts_with("ATHENE_")));
    }

    #[test]
    fn omits_brain_config_and_orchestrator_id_when_absent() {
        let env = worker_env_vars("w1", "/data", "", None, None);
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_ORCHESTRATOR_ID"));
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_BRAIN"));
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_CONFIG"));
    }

    #[test]
    fn recursive_worker_spawn_is_rejected_before_allocation() {
        assert!(reject_recursive_worker_spawn(Some("worker")).is_err());
        assert!(reject_recursive_worker_spawn(Some("orchestrator")).is_ok());
        assert!(reject_recursive_worker_spawn(None).is_ok());
    }
}
