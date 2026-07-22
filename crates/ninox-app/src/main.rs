mod app;
mod components;
mod input;
mod models;
mod spawn_util;
mod style;
mod theme;

use spawn_util::{create_worker_worktree, repo_from_workspace, seed_worker_brain_skill};
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
use clap::{Parser, Subcommand};
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
        /// Display name for the session (defaults to first four words of prompt)
        #[arg(long, short)]
        name: Option<String>,
        /// Orchestrator session ID (read from NINOX_ORCHESTRATOR_ID if not supplied)
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
}

#[derive(Subcommand)]
enum BrainAction {
    /// Rebuild the knowledge index
    Index,
    /// Pull and push all changes to the brain's remote (no-op for a brain
    /// without a remote — see `ninox brain remote set`)
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Fires on every assistant turn (event-driven) or every `refreshInterval`
    // seconds for every session Ninox spawns — must stay fast and never
    // trigger the tmux-config/wrapper-hook/self-shim setup below, none of
    // which this subcommand needs.
    if matches!(args.command, Some(Command::Statusline)) {
        run_statusline(args.db.unwrap_or_else(default_db_path));
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

    match args.command {
        Some(Command::Spawn { prompt, workspace, name, orchestrator_id }) => {
            let config = AppConfig::load().unwrap_or_default();
            run_spawn(store, config, prompt, workspace, name, orchestrator_id).await
        }
        Some(Command::Send { session_id, message }) => {
            ninox_core::tmux::send_keys(&session_id, &message).await
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
        None => run_tui(store, args.port, args.headless).await,
    }
}

async fn run_spawn(
    store: Arc<Store>,
    config: AppConfig,
    prompt: String,
    workspace: String,
    name: Option<String>,
    orchestrator_id: Option<String>,
) -> anyhow::Result<()> {
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

    // Create an isolated git worktree so workers don't share a branch.
    // Falls back to the shared workspace if the repo check fails (e.g. not git).
    let effective_workspace = match create_worker_worktree(&workspace, &id).await {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("worktree creation failed for {id}, using shared workspace: {e}");
            workspace.clone()
        }
    };
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

    // Append worker context so every agent knows its session ID, its
    // orchestrator's ID, and how to communicate back when done or stuck.
    let mut effective_prompt = prompt;
    if !orch_id_env.is_empty() {
        effective_prompt.push_str(&worker_context_footer(&id, &orch_id_env));
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
        terminal_at: None,
    };

    store.upsert_session(&session)?;
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
    tmux::create_session(&id, &effective_workspace, &cmd, &env_vec).await?;

    Ok(())
}

/// The context footer appended to every worker's task prompt: its own
/// session id, its orchestrator's id, the channels back to the orchestrator,
/// and the one-worker-one-PR scope rule.
fn worker_context_footer(id: &str, orch_id: &str) -> String {
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
            terminal_at: None,
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

fn has_display() -> bool {
    #[cfg(target_os = "macos")]
    { true }
    #[cfg(not(target_os = "macos"))]
    { std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() }
}

#[cfg(test)]
mod worker_env_tests {
    use super::{first_line, worker_context_footer, worker_env_vars};

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
        let footer = worker_context_footer("w1", "orch1");
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
    fn forwards_brain_and_config_when_present() {
        let env = worker_env_vars("w1", "/data", "orch1", Some("/brain.db"), Some("/cfg.toml"));
        assert!(env.contains(&("NINOX_ORCHESTRATOR_ID", "orch1")));
        assert!(env.contains(&("NINOX_BRAIN", "/brain.db")));
        assert!(env.contains(&("NINOX_CONFIG", "/cfg.toml")));
        assert!(env.contains(&("NINOX_SESSION", "w1")));
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
}
