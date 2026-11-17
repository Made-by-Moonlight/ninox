use std::ops::ControlFlow;

use crate::spawn_util::{
    create_worker_worktree, interactive_env_vars, repo_from_workspace, resolve_standalone_workspace, seed_worker_skills,
};
use crate::{connect, fleet, machine, restart, runtime_cli, server_restart, service, update_cli};
use ninox_core::{
    capabilities::Audience,
    config::{AppConfig, SendMechanism},
    events::Engine,
    github::resolve_token,
    harness::{permission_mode, LaunchKind, PermissionMode},
    lifecycle::{poller::Poller, repo_discovery},
    slugify,
    store::Store,
    tmux,
    types::{Session, SessionStatus},
    workspace::{self, Ownership, OrphanStatus, SkipReason, WorkspaceAction, WorkspaceFacts, WorkspaceRow},
    BrainIndex, QueryFilters,
};
use clap::{Parser, Subcommand};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

// `name` is pinned rather than left to clap's `CARGO_PKG_NAME` default:
// this struct is defined here, in the shared crate, so that default would
// print "ninox-ui-shared" for every binary — `ninox` matches what users
// type and what `ninox update`'s version check expects, regardless of
// whether the running binary is ninox-gui, ninox-tui, or its `nx` alias.
#[derive(Parser)]
#[command(name = "ninox", disable_version_flag = true)]
struct Args {
    /// Print the version and the configured update channel
    #[arg(short = 'V', long = "version")]
    show_version: bool,
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    headless: bool,
}

/// The new session's brain set. Omit both to inherit the spawner's set
/// (or, outside a session, every searchable source writing to the default).
#[derive(clap::Args, Debug, Clone, Default)]
pub struct BrainArgs {
    /// A brain catalogue the session reads (repeatable). Inside a session
    /// each must be one the spawner itself reads.
    #[arg(long = "brain", value_name = "NAME")]
    pub reads: Vec<String>,
    /// The catalogue the session writes to: one of its reads, not read-only.
    #[arg(long = "brain-write", value_name = "NAME")]
    pub write: Option<String>,
}

impl BrainArgs {
    fn resolve(&self, store: &Store, caller_session_id: Option<&str>, config: &AppConfig) -> anyhow::Result<ninox_core::brain_set::BrainSet> {
        ninox_core::brain_set::resolve_child(store, caller_session_id, config, &self.reads, self.write.as_deref())
    }
}

/// The calling session's pane-verified id (pane identity, then process
/// ancestry) — never the forgeable `NINOX_SESSION`.
async fn verified_caller_session_id(store: &Store) -> Option<String> {
    let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten().map(|p| p.physical_tmux_name);
    ninox_core::orchestrator_auth::resolve_live_caller_session_id(store, pane.as_deref())
}

#[derive(Subcommand)]
enum ModelsAction {
    /// Print the resolved model for each role and worker type
    Show {
        #[arg(long)]
        json: bool,
    },
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
        /// Named worker type (see `ninox_core::worker_type`) whose harness,
        /// model (if set) and permission profile apply to this spawn, and
        /// whose brief template (if any) composes with `--prompt`. Omit to
        /// default to `Implementor`.
        #[arg(long = "type")]
        worker_type: Option<String>,
        /// Additional success criterion this worker must attest to before
        /// `ninox complete` lets it finish (repeatable). Appended to the
        /// worker type's default criteria unless `--success-only` is also
        /// given.
        #[arg(long)]
        success: Vec<String>,
        /// Replace the worker type's default success criteria outright with
        /// the `--success` items given (if none, the worker has no criteria
        /// to attest to at all).
        #[arg(long)]
        success_only: bool,
        /// Fail instead of queueing when this machine is at its concurrent
        /// session limit (see `ninox limits`).
        #[arg(long)]
        no_queue: bool,
        #[command(flatten)]
        brains: BrainArgs,
        /// Model for this one spawn, overriding the worker type's model and
        /// `[worker].model` (see `ninox models show`).
        #[arg(long)]
        model: Option<String>,
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
        /// Fail instead of queueing when at the concurrent session limit.
        #[arg(long)]
        no_queue: bool,
        #[command(flatten)]
        brains: BrainArgs,
        /// Model for this orchestrator, overriding `[orchestrator].model`.
        #[arg(long)]
        model: Option<String>,
    },
    /// Spawn a sub-orchestrator nested under an already-verified orchestrator
    /// (or, for a worker whose worker-type profile explicitly grants it,
    /// nested under that worker's own parent) — real delegation, not a peer.
    /// Enforces the configured depth cap (`[nested_orchestration]`, default
    /// 3): a request that would exceed it is rejected outright.
    SpawnSubOrchestrator {
        /// Display name for the sub-orchestrator; slugified into its session ID
        #[arg(long, short)]
        name: String,
        /// Initial brief, delivered once the new sub-orchestrator's harness
        /// is ready for input.
        #[arg(long, short)]
        prompt: Option<String>,
        /// Parent orchestrator id (read from NINOX_ORCHESTRATOR_ID if not
        /// supplied) — verified before being trusted, exactly like `ninox
        /// spawn`'s `--orchestrator-id`.
        #[arg(long)]
        orchestrator_id: Option<String>,
        /// Fail instead of queueing when at the concurrent session limit.
        #[arg(long)]
        no_queue: bool,
        #[command(flatten)]
        brains: BrainArgs,
        /// Model for this sub-orchestrator, overriding `[orchestrator].model`.
        #[arg(long)]
        model: Option<String>,
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
    /// Inspect and clean up leftover agent worktrees across every repo ninox
    /// has spawned into (see `ninox_core::workspace`). Read-only unless the
    /// `prune` action is used.
    Workspaces {
        #[command(subcommand)]
        action: WorkspacesAction,
    },
    /// Send a text message to a session's terminal (injected as keyboard input)
    Send {
        /// Target session ID
        session_id: String,
        /// Message text to inject (Enter is sent automatically)
        message: String,
        /// Label the message for the dedup/notification logic and the
        /// delivered-text prefix tag (defaults to `freeform`)
        #[arg(long, value_enum)]
        kind: Option<MessageKindArg>,
        /// Print the delivery receipt as JSON
        #[arg(long)]
        json: bool,
    },
    /// Messages addressed to you (or to another session), with each
    /// attempt's outcome, plus anything still held for delivery. Use it when
    /// you suspect a message didn't reach you.
    Messages {
        /// Messages to the calling session (the default without --to)
        #[arg(long)]
        to_me: bool,
        /// Messages to this session instead
        #[arg(long, conflicts_with = "to_me")]
        to: Option<String>,
        /// Only messages at or after this: a duration back from now (`30m`,
        /// `2h`, `1d`) or epoch milliseconds. Default `1d`.
        #[arg(long)]
        since: Option<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Reply to a message sent via the fleet-overview TUI's "ninox prompt"
    /// (see the ninox-prompt-reply skill) — answer text and/or node ids to
    /// highlight, read back by the TUI that sent the prompt
    /// (`ninox_core::prompt_reply`). Not a general-purpose message: use
    /// `ninox send` for an ordinary session message instead.
    PromptAnswer {
        /// Correlation id from the "reply with `ninox prompt-answer
        /// <reply_to> ...`" line in the prompt message
        reply_to: String,
        /// Answer text to show inline in the prompt area
        answer: Option<String>,
        /// Node id(s) to highlight/select in the graph, comma-separated
        #[arg(long, value_delimiter = ',')]
        select: Vec<String>,
    },
    /// Ask the orchestrator to schedule additional work discovered outside
    /// this worker's task (used by workers; Ninox delivers it and the
    /// orchestrator spawns a dedicated worker)
    RequestWork {
        /// Description of the additional work
        description: String,
    },
    /// Escalate a question or decision from a worker session to a human and
    /// (by default) block until it's answered (used by workers).
    UserRequest {
        #[command(subcommand)]
        action: UserRequestCommand,
    },
    /// Self-completion: a worker (`$NINOX_SESSION`) attests it has met its
    /// Definition of done and finishes itself. Verifies what it can,
    /// refuses on a dirty/unpushed worktree without `--force`, stores the
    /// outcome, notifies the orchestrator, and marks the session finished —
    /// a worker can only ever complete itself, never another session.
    Complete {
        /// What was done — becomes the worker's final report
        #[arg(long)]
        summary: String,
        /// Supporting link(s)/path(s) (repeatable)
        #[arg(long)]
        evidence: Vec<String>,
        /// Complete anyway despite a dirty or unpushed worktree
        #[arg(long)]
        force: bool,
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
    /// GitHub PR state as Ninox's watcher last saw it.
    Pr {
        #[command(subcommand)]
        action: PrAction,
    },
    /// Print this session's own identity (orchestrator vs. worker, owner,
    /// workspace, branch, PR, enabled capabilities), resolved fresh from the
    /// store — never cached. Explicit "not managed"/"terminated" output
    /// when ninox doesn't recognize or no longer manages the session.
    Whoami {
        /// Emit JSON instead of plain text
        #[arg(long)]
        json: bool,
    },
    /// `UserPromptSubmit`/`SessionStart` identity-reminder hooks (see
    /// `ninox_core::identity`). Installed only when `[role_rehydration]`
    /// is enabled. Invoked by Claude Code's own hooks, not intended for
    /// direct use.
    IdentityHook {
        #[command(subcommand)]
        action: IdentityHookAction,
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
    /// List idle live workers grouped by orchestrator — how long idle, what
    /// each waits on, and its (cached) worktree disk use. Reads stored
    /// state only; never calls GitHub.
    Idle {
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
        /// Send each orchestrator with idle workers one message listing its
        /// own (rate-limited per orchestrator and worker set)
        #[arg(long)]
        prompt: bool,
        /// With --prompt, prompt only this orchestrator
        #[arg(long, requires = "prompt")]
        orchestrator: Option<String>,
    },
    /// Show which model each role and worker type resolves to, and why
    /// (explicit > type > `[orchestrator]`/`[worker]` setting > harness default).
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Register (or inspect) this orchestrator's goals/plan markdown doc,
    /// rendered live in the desktop app.
    Plan {
        #[command(subcommand)]
        action: PlanAction,
    },
    /// GitHub polling state: the shared token's GraphQL budget.
    Github {
        #[command(subcommand)]
        action: GithubAction,
    },
    /// Show or change the per-location concurrent session limit. Spawns
    /// over the limit are queued and start automatically as slots free up.
    Limits {
        #[command(subcommand)]
        action: LimitsAction,
    },
    /// Read or write `config.toml` by dotted key, or dump the full resolved
    /// config (orchestrator-only — see `ninox_core::config_cli`).
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Define, inspect, and manage worker types — the named spawn profiles
    /// `ninox spawn --type <name>` applies (brief template, harness/model,
    /// permission profile, and which other types this one may spawn via
    /// `can_spawn`). Built-ins (Orchestrator, Implementor, Researcher,
    /// Evaluator) always exist; this command lets any caller — orchestrator
    /// or worker — define more, no `ninox config` authorization required.
    WorkerType {
        #[command(subcommand)]
        action: WorkerTypeAction,
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
    /// Restart a live, healthy session in place so it picks up a tooling
    /// update (a newer `ninox` build, harness version, MCP config, or
    /// reseeded skills) — conversation resumed wherever the harness
    /// supports it. For a crashed/interrupted session, use `ninox fleet
    /// restore` instead.
    Restart {
        #[command(flatten)]
        args: restart::RestartArgs,
    },
    /// Start the headless engine at login (launchd / systemd user unit).
    Service {
        #[command(subcommand)]
        action: service::ServiceAction,
    },
    /// The background server (`ninox --headless`) on the configured port.
    Server {
        #[command(subcommand)]
        action: server_restart::ServerAction,
    },
    /// SSH-connected remote machines: add, list, or remove a tracked
    /// machine profile (opaque connection metadata only — see
    /// `ninox_core::config::MachineProfile`).
    Machine {
        #[command(subcommand)]
        action: machine::MachineAction,
    },
    /// Open the desktop app, even when run from a terminal.
    Gui,
    /// Update this binary (and, with --app, Ninox.app) to the latest
    /// prebuilt Apple silicon release from CodeArtifact, via the `aws` CLI.
    Update {
        /// Only report whether a newer version is published
        #[arg(long)]
        check: bool,
        /// Reinstall even when already on the latest version (never downgrades)
        #[arg(long)]
        force: bool,
        /// Install this exact version (downgrades allowed)
        #[arg(long, value_name = "X.Y.Z")]
        version: Option<String>,
        /// Also replace an installed /Applications (or ~/Applications) Ninox.app
        #[arg(long)]
        app: bool,
        /// Release channel to follow (`stable` or `alpha`); persisted to
        /// `[update] channel` in config.toml. Defaults to the configured one.
        #[arg(long, value_name = "CHANNEL")]
        channel: Option<ninox_core::config::UpdateChannel>,
    },
    /// Point the shared `ninox` shim (what agents' PATH and seeded hooks
    /// run) at this binary. Launching ninox never replaces a working shim,
    /// so run this after an upgrade, or to put workers on a feature build.
    ResetBin,
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
    /// The "Ask ninox" prompt agent: a per-machine singleton session the
    /// GUI/TUI prompt bar talks to (see `ninox_ui_shared::prompt_agent`).
    Prompt {
        #[command(subcommand)]
        action: PromptAction,
    },
}

#[derive(Subcommand)]
enum PromptAction {
    /// Deliver text to the prompt agent, starting it if this is the first
    /// ask (or restarting it transparently if it died since the last one).
    Ask {
        /// Text to ask the prompt agent
        text: String,
    },
    /// Reset the prompt agent's conversation (`/clear` for claude-code, a
    /// fresh restart for any other harness).
    Clear,
    /// Print the prompt agent's current lifecycle state.
    Status {
        /// Emit JSON instead of plain text
        #[arg(long)]
        json: bool,
    },
}

/// `ask` creates a request and (unless `--async`) blocks for its answer;
/// `wait` re-attaches to one already created — the restart-recovery path
/// after an `ask --async` or an earlier `wait` was interrupted; `status`
/// polls without blocking. None of these carry a session-identifying
/// argument: the requester is always this process's own resolved identity
/// (`worker_status::resolve_session_id`), the same posture `ninox send`
/// takes for its own sender — a caller cannot claim to be a session it
/// merely names.
#[derive(Subcommand)]
enum UserRequestCommand {
    /// Create a user request and, unless `--async`, block until it's
    /// answered or `--timeout` elapses.
    Ask {
        /// The question or decision to put to the human
        prompt: String,
        /// Print the request id immediately instead of blocking for the answer
        #[arg(long = "async")]
        async_mode: bool,
        /// Seconds to wait for an answer before giving up (ignored with --async)
        #[arg(long, default_value_t = 240)]
        timeout: u64,
    },
    /// Block on an existing request's answer.
    Wait {
        /// Request id, as printed by `ask`
        id: String,
        /// Seconds to wait for an answer before giving up
        #[arg(long, default_value_t = 240)]
        timeout: u64,
    },
    /// Report whether a request is still pending or already answered, without blocking.
    Status {
        /// Request id, as printed by `ask`
        id: String,
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
    /// Installed in both a worker's worktree settings and an orchestrator
    /// root's settings; not intended for direct use.
    HookPrompt,
    /// Stop hook entry point: marks the session idle (unless it declared
    /// itself blocked). Installed in both a worker's worktree settings and
    /// an orchestrator root's settings; not intended for direct use.
    HookStop,
    /// PreToolUse hook entry point (matcher: AskUserQuestion, ExitPlanMode):
    /// marks the session as awaiting a human's answer to an interactive
    /// prompt. Installed in both a worker's worktree settings and an
    /// orchestrator root's settings; not intended for direct use.
    HookNeedsInput,
    /// PermissionRequest hook entry point: marks the session as awaiting a
    /// permission decision. Installed in both a worker's worktree settings
    /// and an orchestrator root's settings; not intended for direct use.
    HookPermission,
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

#[derive(Subcommand, Debug)]
pub enum PrAction {
    /// Cached checks, reviews, unresolved threads and mergeable state, with
    /// no GitHub call. Without a PR: your own PR plus the PRs you watch.
    Status {
        /// A PR URL, owner/repo#N, or #N
        pr:   Option<String>,
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
}

/// CLI-facing subset of `ninox_core::types::MessageKind` — mirrors
/// `ActivityStateArg`'s wrapper pattern so `ninox send --kind` gets clap's
/// `value_enum` parsing/help without `MessageKind` itself taking a clap
/// dependency.
///
/// Deliberately excludes `Completion`, `RequestForWork` and `System`:
/// those three are reserved for the one code path each that is allowed to
/// emit them (`ninox complete`, the poller's `deliver_work_requests`, and
/// Ninox's own internally-generated reactions, respectively — none of
/// which go through this CLI arg). Exposing them here would let any agent
/// forge a `--kind completion` message to suppress its own real
/// done/retired notice (`Store::has_completion_since`'s dedup) without
/// ever actually completing, or stamp `[system]` to impersonate a
/// Ninox-generated notice (security review on PR #274).
#[derive(Clone, Copy, clap::ValueEnum)]
enum MessageKindArg {
    Status,
    Question,
    Freeform,
}

impl From<MessageKindArg> for ninox_core::types::MessageKind {
    fn from(k: MessageKindArg) -> Self {
        match k {
            MessageKindArg::Status => Self::Status,
            MessageKindArg::Question => Self::Question,
            MessageKindArg::Freeform => Self::Freeform,
        }
    }
}

#[cfg(test)]
mod send_cli_tests {
    use super::{Args, Command, MessageKindArg};
    use clap::Parser;

    #[test]
    fn send_default_kind_is_freeform() {
        let args = Args::parse_from(["ninox", "send", "w1", "hello"]);
        let Some(Command::Send { kind, .. }) = args.command else { panic!("expected Send") };
        assert!(kind.is_none(), "no --kind flag must leave the CLI arg unset — dispatch() defaults it to Freeform");
    }

    #[test]
    fn send_kind_flag_overrides_the_default() {
        let args = Args::parse_from(["ninox", "send", "w1", "hello", "--kind", "status"]);
        let Some(Command::Send { kind, .. }) = args.command else { panic!("expected Send") };
        assert!(matches!(kind, Some(MessageKindArg::Status)));
    }

    /// `completion`/`request_for_work`/`system` are reserved for the code
    /// paths that alone are allowed to emit them (`ninox complete`, `ninox
    /// request-work`'s poller delivery, and Ninox's own internal reactions,
    /// respectively) — see the security review on PR #274: without this,
    /// any agent could forge a `--kind completion` message to suppress its
    /// own real done/retired notice without ever actually completing, or
    /// stamp `[system]` to impersonate a Ninox-generated notice.
    #[test]
    fn send_kind_rejects_completion() {
        assert!(Args::try_parse_from(["ninox", "send", "w1", "hello", "--kind", "completion"]).is_err());
    }

    #[test]
    fn send_kind_rejects_request_for_work() {
        assert!(Args::try_parse_from(["ninox", "send", "w1", "hello", "--kind", "request_for_work"]).is_err());
    }

    #[test]
    fn send_kind_rejects_system() {
        assert!(Args::try_parse_from(["ninox", "send", "w1", "hello", "--kind", "system"]).is_err());
    }
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
    /// With a plan id: print that ghost plan and its nodes. Without one:
    /// print this orchestrator's current plan-doc registration.
    Show {
        plan_id: Option<String>,
    },
    /// Generate a validated ghost-node plan for `task` via the `[planning]`
    /// subprocess. Prints the plan; never spawns anything.
    Generate {
        /// Natural-language description of the work to plan.
        task: String,
        /// Workspace the planner explores read-only. Defaults to the current
        /// directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Also store the generated plan as a pending proposal (like
        /// `ninox plan propose`) and print its plan id.
        #[arg(long)]
        propose: bool,
    },
    /// Store a pending ghost plan for review. Input is the `plan generate`
    /// JSON (`parent_id` + `sections`), each section optionally adding
    /// `name`, `workspace`, `model`, `brain_reads`, `brain_write`, `linear`
    /// and `await_done`; a null `parent_id` proposes new top-level
    /// sessions: Orchestrators, or standalone sessions the user owns for any
    /// other worker type. Prints the plan id and node ids. Never spawns: the
    /// user (or the parent orchestrator) must accept it.
    Propose {
        /// JSON file to read; `-` or omitted reads stdin.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// List ghost plans, newest first.
    List {
        /// Only plans still waiting for a decision.
        #[arg(long)]
        pending: bool,
    },
    /// Accept a ghost plan (or some of its nodes) and spawn them. Only the
    /// user, or an orchestrator for plans under itself, may accept.
    Accept {
        plan_id: String,
        /// Accept only these node ids (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        nodes: Vec<String>,
    },
    /// Discard a ghost plan's undecided nodes (or only some of them).
    Discard {
        plan_id: String,
        #[arg(long, value_delimiter = ',')]
        nodes: Vec<String>,
    },
}

#[derive(Subcommand)]
enum GithubAction {
    /// The last rate-limit reading the poller stored: remaining points,
    /// time to reset, and whether polling is backing off.
    Status {
        #[arg(long)]
        json: bool,
    },
}

fn run_github(store: &Store, action: GithubAction) -> anyhow::Result<()> {
    match action {
        GithubAction::Status { json } => {
            let budget = store.get_github_budget()?;
            let config = AppConfig::load_or_default().github;
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "budget": budget,
                    "poll_interval_secs": config.effective_poll_interval_secs(),
                    "detail_refresh_secs": config.effective_detail_refresh_secs(),
                }))?);
            } else {
                println!("{}", github_budget_line(budget.as_ref()));
                println!(
                    "poll every {}s, full re-read at least every {}s",
                    config.effective_poll_interval_secs(), config.effective_detail_refresh_secs(),
                );
            }
        }
    }
    Ok(())
}

fn github_budget_line(budget: Option<&ninox_core::github_budget::GithubBudget>) -> String {
    match budget {
        Some(b) => b.summary_now(),
        None => "GitHub budget: not read yet (no sweep has run)".to_string(),
    }
}

#[derive(Subcommand)]
enum LimitsAction {
    /// Running sessions, limit and queue length for each location.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Set a location's limit: `local`, or a remote machine's label or id,
    /// then a number or `unlimited`. Takes effect on the next poll (about
    /// 5s); lowering it never stops running sessions.
    Set {
        location: String,
        value:    String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print a config value by dotted key, e.g. `orchestrator.harness`.
    Get {
        key: String,
    },
    /// Set a config value by dotted key, e.g. `worker.harness codex`.
    /// `orchestrator.harness`/`worker.harness` are validated against the
    /// harness registry before writing.
    Set {
        key:   String,
        value: String,
    },
    /// Print the full resolved config as TOML.
    Show,
}

/// `ninox worker-type` — define and manage the named spawn profiles `ninox
/// spawn --type <name>` applies. Built-in types (Orchestrator, Implementor,
/// Researcher, Evaluator) always exist and show up in `list`; `create` adds
/// more. A type's `can_spawn` list controls which OTHER types a session of
/// this type may itself pass to `ninox spawn --type` — see `show`'s output.
///
/// `list`/`show`/`export` are read-only and open to every caller.
/// `create`/`edit`/`delete`/`reset`/`import` are NOT — not even for an
/// orchestrator's own session: they require either no calling session at
/// all (a human at a terminal, or the GUI/TUI) or a worker type with
/// `can_manage_types: true` (see `show`'s output for whether yours has it)
/// — otherwise refused with a message to ask the user to run it instead.
/// An authorized caller is still refused if it tries to: mutate the one
/// type its own running session currently has, set or clear
/// `can_manage_types` on any type (only the user may), create/edit/delete/
/// reset/import over a hidden/system type, or give a type more `can_spawn`
/// reach than its own type has.
#[derive(Subcommand)]
enum WorkerTypeAction {
    /// List the worker types you may spawn, each with when to use it, the
    /// model it runs and what it may spawn. Run it before `ninox spawn
    /// --type` and pick the type whose description fits the task. Run by
    /// you at a terminal, it lists every user-facing type.
    List {
        /// Print machine-readable JSON instead of a human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Print one worker type's full effective spec: brief template,
    /// harness/model, permission profile, and `can_spawn` — the list of
    /// type names a session of THIS type is allowed to pass to `ninox spawn
    /// --type` (or `*` for every user-facing type, or empty for none).
    Show {
        /// Type name, or an accepted alias (e.g. `Implementation`/`Worker`
        /// both resolve to `Implementor`).
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Define a new worker type.
    Create {
        /// New type's name — letters, digits, `-`, `_` only, and not
        /// already in use (built-in, alias, or another custom type).
        #[arg(long)]
        name: String,
        /// Brief template composed with `ninox spawn --prompt` at spawn
        /// time. MUST contain the literal placeholder `{prompt}` — e.g.
        /// `"Investigate and report findings: {prompt}"`. Prefix with `@`
        /// to read the template from a file instead, e.g. `@brief.txt`.
        #[arg(long)]
        brief: String,
        /// One line telling orchestrators, the planner and the Ninox prompt
        /// when to pick this type (shown by `ninox worker-type list`).
        #[arg(long = "use-when")]
        use_when: Option<String>,
        /// Default harness for this type's spawns (must be an enabled
        /// harness — see `ninox config show`'s `[harnesses]`). Defaults to
        /// the locked-in default harness.
        #[arg(long)]
        harness: Option<String>,
        /// Default model id for this type's spawns. Omit to use the
        /// harness's own default model.
        #[arg(long)]
        model: Option<String>,
        /// A Claude Code permission rule to allow, e.g. `Read` or
        /// `Bash(gh pr merge:*)`. Repeat for multiple rules.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// A permission rule to deny. Repeat for multiple rules.
        #[arg(long = "deny")]
        deny: Vec<String>,
        /// A permission rule that should prompt instead of auto-deciding.
        /// Repeat for multiple rules.
        #[arg(long = "ask")]
        ask: Vec<String>,
        /// Comma-separated type names this type's own sessions may spawn
        /// (e.g. `"Researcher,Evaluator"`), or `*` for every user-facing
        /// type. Each name must already exist. Omit for "none" (the
        /// default for a new custom type).
        #[arg(long = "can-spawn")]
        can_spawn: Option<String>,
        /// Grants this type permission to itself run `worker-type create/
        /// edit/delete/reset/import` (not just `list`/`show`/`export`,
        /// which are open to everyone). Omit for `false` (the default for
        /// a new custom type). Only settable by the user — a session with
        /// no calling context (a human at a terminal, or the GUI/TUI); an
        /// agent passing this flag at all is refused, even one already
        /// authorized to manage other types.
        #[arg(long = "can-manage-types")]
        can_manage_types: Option<bool>,
    },
    /// Change an existing worker type. Only the flags you pass are changed
    /// — everything else (including a built-in's compiled-in defaults) is
    /// left as-is. Editing a built-in writes a config overlay; see `reset`
    /// to remove it again.
    Edit {
        /// Type name, or an accepted alias.
        name: String,
        /// New brief template (same `{prompt}`/`@file` rules as `create`).
        /// Pass an empty string to clear it (spawns of this type then pass
        /// `--prompt` through unchanged).
        #[arg(long)]
        brief: Option<String>,
        /// When to pick this type, in one line. Pass an empty string to
        /// clear it.
        #[arg(long = "use-when")]
        use_when: Option<String>,
        #[arg(long)]
        harness: Option<String>,
        /// Pass an empty string to clear the model override.
        #[arg(long)]
        model: Option<String>,
        /// Replaces the ENTIRE allow list with the rules given (repeat the
        /// flag for multiple). Use `--clear-allow` to empty it instead.
        #[arg(long = "allow")]
        allow: Vec<String>,
        #[arg(long = "clear-allow")]
        clear_allow: bool,
        /// Replaces the entire deny list. Use `--clear-deny` to empty it.
        #[arg(long = "deny")]
        deny: Vec<String>,
        #[arg(long = "clear-deny")]
        clear_deny: bool,
        /// Replaces the entire ask list. Use `--clear-ask` to empty it.
        #[arg(long = "ask")]
        ask: Vec<String>,
        #[arg(long = "clear-ask")]
        clear_ask: bool,
        /// Replaces `can_spawn` (same comma-separated/`*` shape as
        /// `create`). Pass an empty string to clear it to "none".
        #[arg(long = "can-spawn")]
        can_spawn: Option<String>,
        /// Grants or revokes this type's permission to itself run
        /// `worker-type create/edit/delete/reset/import` — see `create`'s
        /// `--can-manage-types`.
        #[arg(long = "can-manage-types")]
        can_manage_types: Option<bool>,
    },
    /// Remove a CUSTOM worker type entirely. Refused for a built-in (by its
    /// canonical name or an alias) — use `reset` for those instead.
    Delete {
        name: String,
    },
    /// Remove a built-in worker type's config overlay, reverting it to its
    /// compiled-in default. Refused for a type with no overlay to remove
    /// (already at its default, or not a built-in at all).
    Reset {
        name: String,
    },
    /// Print one or more worker types as a `[[worker_types]]` TOML
    /// document to stdout — redirect it to a file (`> types.toml`) to
    /// share it. With no names, exports every user-facing type.
    Export {
        names: Vec<String>,
    },
    /// Read a `[[worker_types]]` TOML document (as produced by `export`)
    /// and define/overwrite each entry it contains.
    Import {
        /// Path to the TOML file to import.
        file: PathBuf,
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
enum IdentityHookAction {
    /// UserPromptSubmit hook entry point: appends a fresh identity reminder
    /// into `hookSpecificOutput.additionalContext`. Unlike the inbox drain
    /// hooks, this always prints something — including an explicit
    /// "not managed"/"terminated" line — never silently skipping output.
    Prompt,
    /// SessionStart hook entry point: same reminder, for cold start and
    /// `--resume`.
    SessionStart,
}

#[derive(Subcommand)]
enum BrainAction {
    /// Rebuild the write catalogue's knowledge index (`--all`: every brain
    /// this session reads)
    Index {
        #[arg(long)]
        all: bool,
    },
    /// Sync the write catalogue with its remote (requires a configured
    /// remote — see `ninox brain remote set`). `--all` also covers every
    /// brain this session reads: read-only ones are pulled, never pushed.
    Sync {
        #[arg(long)]
        all: bool,
    },
    /// Search entries by full-text across every brain this session reads;
    /// results are tagged `source:id` when it reads more than one
    Query {
        /// Search text
        text: String,
        /// Filter by entry type
        #[arg(long)]
        entry_type: Option<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Search only this catalogue
        #[arg(long)]
        catalogue: Option<String>,
    },
    /// Print a single entry
    Show {
        /// Entry id (`source:path`, or a bare relative path such as
        /// people/alice.md, looked up in each brain in turn)
        path: String,
        /// Look only in this catalogue
        #[arg(long)]
        catalogue: Option<String>,
    },
    /// Entries related to one (shared links, co-citation, similarity)
    Related {
        /// Entry id (`source:path` or a bare relative path)
        id: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Look only in this catalogue
        #[arg(long)]
        catalogue: Option<String>,
    },
    /// Entries linking to one, across every brain this session reads
    Backlinks {
        /// Entry id (`source:path` or a bare relative path)
        id: String,
        /// Look only in this catalogue
        #[arg(long)]
        catalogue: Option<String>,
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
        /// Write to this catalogue instead of the session's write catalogue;
        /// it must be one this session reads, and writable
        #[arg(long)]
        catalogue: Option<String>,
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

#[derive(Subcommand)]
enum WorkspacesAction {
    /// List every worktree under `.claude/worktrees/` across every known
    /// repo (or just `--repo`): path, session id, status, size, dirty,
    /// unpushed count, upstream/merged state, and the action the sweeper
    /// would take. Never deletes anything.
    List {
        /// Emit JSON instead of a table
        #[arg(long)]
        json: bool,
        /// Restrict to one repo root (default: every repo ninox has spawned into)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Clean up leftover worktrees — by default, exactly what the periodic
    /// orphan sweeper would (merged or empty orphans only). Dirty or
    /// unpushed worktrees are never pruned by this command, with or
    /// without any flag below — delete those by hand after reviewing
    /// `ninox workspaces list`.
    Prune {
        /// Preview what would be pruned without touching anything
        #[arg(long)]
        dry_run: bool,
        /// Also prune clean, fully-pushed worktrees that just haven't been
        /// merged yet (their branch is kept; only the worktree is removed)
        #[arg(long)]
        include_unmerged: bool,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
        /// Restrict to one repo root (default: every repo ninox has spawned into)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Print one worktree's full report, looked up by path or session id
    Show {
        /// Worktree path, or the session id it's tied to
        target: String,
    },
}

/// What the caller (each binary's thin `main.rs`) should do once shared
/// dispatch is done: `Continue` means the command fully executed and the
/// process should exit with this result; `Break` means the user asked for
/// an interactive UI (or the implicit default), which only the calling
/// binary itself can provide.
pub struct UiArgs {
    pub store:    Arc<Store>,
    pub port_arg: Option<u16>,
    pub headless: bool,
    pub db_path:  PathBuf,
}

pub enum UiRequest {
    /// No subcommand given — each binary applies its own default.
    Default(UiArgs),
    /// `ninox tui` explicitly.
    Tui(UiArgs),
    /// `ninox gui` explicitly.
    Gui(UiArgs),
}

/// Prints the error for a request this binary can't satisfy because it's
/// the wrong UI (`ninox-gui` asked to open the terminal UI, or `ninox-tui`
/// asked for the desktop one), then exits 1. Never names `ninox <subcommand>`
/// as a fix: the shared `ninox` shim can be fixed to one binary
/// (`scripts/install-macos.sh`) or point at whichever installed first
/// (`cargo install`), so it isn't a reliable way to reach the other UI from
/// here — only installing that binary directly (and, for the `cargo
/// install` case, `ninox reset-bin` once it's in) always works.
pub fn exit_wrong_ui(running: &str, missing_ui: &str, install_bin: &str, bare_alias: Option<&str>) -> ! {
    let alias_hint = bare_alias.map(|a| format!(" (bare `{a}` always opens it)")).unwrap_or_default();
    eprintln!(
        "{running} has no {missing_ui}; install {install_bin} directly{alias_hint}. \
The shared `ninox` shim may point at either binary, so don't rely on reaching it through `ninox`; \
once {install_bin} is installed, `ninox reset-bin` can repoint the shim there if needed."
    );
    std::process::exit(1);
}

/// Parses argv and runs every command this crate's two binaries share.
/// Called from each binary's own `main()` as the first thing it does, from
/// inside a tokio runtime context (`#[tokio::main]` for ninox-tui; an
/// explicit `Runtime::block_on` for ninox-gui, which must keep `main`
/// itself synchronous — see that crate's `main.rs`). It must be called
/// before anything else — every hot-path subcommand below (`Statusline`,
/// `Inbox`, etc.) short-circuits *inside* this function, so no setup the
/// caller does ahead of this call shares that guarantee.
/// `install_nx_alias`: whether this binary should also install the `nx`
/// alias once past the hot-path checks — `true` for `ninox-tui` only,
/// since bare `nx` has always meant "open the terminal UI".
pub async fn dispatch(install_nx_alias: bool) -> anyhow::Result<ControlFlow<UiRequest, ()>> {
    let args = Args::parse();
    if args.show_version {
        print!("{}", update_cli::version_text());
        return Ok(ControlFlow::Continue(()));
    }
    let mut command = args.command;

    // Fires on every assistant turn (event-driven) or every `refreshInterval`
    // seconds for every session Ninox spawns — must stay fast and never
    // trigger the tmux-config/wrapper-hook/self-shim setup below, none of
    // which this subcommand needs.
    if matches!(command, Some(Command::Statusline)) {
        run_statusline(args.db.unwrap_or_else(default_db_path));
        return Ok(ControlFlow::Continue(()));
    }

    // The PTY host, pane bridges and screen reads are runtime plumbing
    // (`ninox read` is agent-invoked mid-session); none of them touch the
    // store or need the setup below.
    if let Some(Command::Ptyd { .. } | Command::Pane { .. } | Command::Read { .. }) = &command {
        let db_path = args.db.clone().unwrap_or_else(default_db_path);
        let result = match command.unwrap() {
            Command::Ptyd { takeover, action } => runtime_cli::run_ptyd(takeover, action).await,
            Command::Pane { action } => runtime_cli::run_pane(action, db_path).await,
            Command::Read { session_id, lines, ansi } => runtime_cli::run_read(&session_id, lines, ansi).await,
            _ => unreachable!(),
        };
        return result.map(ControlFlow::Continue);
    }

    // Fires on every Stop/UserPromptSubmit turn of every worker with inbox
    // messaging enabled — same "stay fast, skip the heavy setup" reasoning
    // as Statusline above; this subcommand needs none of it either.
    if let Some(Command::Inbox { action }) = command {
        run_inbox(action, args.db.clone().unwrap_or_else(default_db_path));
        return Ok(ControlFlow::Continue(()));
    }

    // Fires on every UserPromptSubmit/SessionStart turn of every session
    // with `[role_rehydration]` enabled — same "stay fast" reasoning as
    // Inbox above.
    if let Some(Command::IdentityHook { action }) = command {
        run_identity_hook(action, args.db.clone().unwrap_or_else(default_db_path)).await;
        return Ok(ControlFlow::Continue(()));
    }

    // An orchestrator calls this directly (via NINOX_BIN) to reply to a
    // ninox-prompt message — a plain local file write, same "stay fast"
    // reasoning as Inbox/IdentityHook above: no store, no tmux.
    if let Some(Command::PromptAnswer { reply_to, answer, select }) = command {
        run_prompt_answer(&inbox_sessions_dir(), &reply_to, answer, select)?;
        return Ok(ControlFlow::Continue(()));
    }

    // Agents call this directly (via NINOX_BIN) mid-session to re-confirm
    // their own identity, so it must stay as cheap as Capabilities below:
    // `Store::open` + `AppConfig::load`, no tmux, no wrappers — except for an
    // orchestrator target, which needs the live pane-identity check (see
    // `ninox_core::identity::resolve_for_caller`).
    if let Some(Command::Whoami { json }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::fs::create_dir_all(db_path.parent().unwrap())?;
        let store = Store::open(&db_path)?;
        let config = AppConfig::load_or_default();
        let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
        let caller_type = std::env::var("NINOX_CALLER_TYPE").ok().filter(|s| !s.is_empty());
        let cwd = std::env::current_dir().ok();
        println!("{}", run_whoami(&store, &config, env_session.as_deref(), cwd.as_deref(), caller_type.as_deref(), json).await?);
        return Ok(ControlFlow::Continue(()));
    }

    // Workers call `ask`/`wait` mid-session to escalate to a human and
    // (usually) block on the answer, and `status` to poll without blocking
    // — same "stay fast" posture as Whoami above: `Store::open` +
    // `resolve_session_id`, no tmux, no wrapper/self-shim setup.
    if let Some(Command::UserRequest { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::fs::create_dir_all(db_path.parent().unwrap())?;
        let store = Store::open(&db_path)?;
        let result = match action {
            // The caller is always the CALLING process's own resolved
            // identity, same as `ninox send`'s sender — never taken from a
            // CLI argument, so a caller cannot claim to be any session it
            // merely names.
            UserRequestCommand::Ask { prompt, async_mode, timeout } => {
                let requester_session_id = resolve_user_request_caller(&store, "ask")?;
                run_user_request_ask(&store, &requester_session_id, &prompt, async_mode, Duration::from_secs(timeout))
            }
            UserRequestCommand::Wait { id, timeout } => {
                let caller_session_id = resolve_user_request_caller(&store, "wait")?;
                run_user_request_wait(&store, &caller_session_id, &id, Duration::from_secs(timeout))
            }
            UserRequestCommand::Status { id } => run_user_request_status(&store, &id),
        };
        println!("{}", result?);
        return Ok(ControlFlow::Continue(()));
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
            return Ok(ControlFlow::Continue(()));
        }

        // Terminal attach only needs Store + tmux, like List{prs:false} above.
        if let Command::Connect { session_id } = cmd {
            warn_if_daemon_down(args.port).await;
            connect::run_connect(&store, &session_id).await?;
            return Ok(ControlFlow::Continue(()));
        }

        // PR watch operations (Open, Close, List{prs:true}) share config setup
        let enabled = AppConfig::load_or_default().pr_watch.enabled;
        let opener = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
        let action = match cmd {
            Command::Open { pr }  => PrWatchCliAction::Open { pr },
            Command::Close { pr } => PrWatchCliAction::Close { pr },
            Command::List { prs: true, .. } => PrWatchCliAction::List,
            _ => unreachable!(),
        };
        println!("{}", run_pr_watch(&store, enabled, action, opener)?);
        return Ok(ControlFlow::Continue(()));
    }

    // Agents run this to discover what ninox can do for them (the bootstrap
    // line in every worker footer and the orchestrator AGENTS.md points
    // here), so it must stay as cheap as Statusline/Inbox/Open above: it
    // reads the config and nothing else — no store, no tmux, no wrappers.
    //
    // Unlike Open/Close/List above, this arm *can* bind its fields directly:
    // all three are `bool` (Copy), so the pattern copies them out instead of
    // moving `command`, and the arm returns unconditionally anyway.
    // Workers run this instead of polling gh; it reads the watcher's cache
    // in the store, so like Whoami it needs only `Store::open`.
    if let Some(Command::Pr { action: PrAction::Status { pr, json } }) = &command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::fs::create_dir_all(db_path.parent().unwrap())?;
        let store = Store::open(&db_path)?;
        let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
        let cwd = std::env::current_dir().ok();
        let caller = ninox_core::worker_status::resolve_session_id(&store, env_session.as_deref(), cwd.as_deref())
            .ok()
            .flatten();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        println!("{}", crate::pr_status::run_pr_status(&store, pr.as_deref(), caller.as_deref(), *json, now)?);
        return Ok(ControlFlow::Continue(()));
    }

    if let Some(Command::Capabilities { worker, orchestrator, json }) = command {
        let filter = match (worker, orchestrator) {
            (true, false) => Some(Audience::Worker),
            (false, true) => Some(Audience::Orchestrator),
            // Neither flag, or both — no filter, list everything.
            _ => None,
        };
        let config = AppConfig::load_or_default();
        println!("{}", run_capabilities(&config, filter, json));
        return Ok(ControlFlow::Continue(()));
    }

    if let Some(Command::Idle { json, prompt, orchestrator }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::fs::create_dir_all(db_path.parent().unwrap())?;
        let store = Store::open(&db_path)?;
        let config = AppConfig::load_or_default();
        let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
        let caller = ninox_core::orchestrator_auth::resolve_live_caller_session_id(
            &store, pane.as_ref().map(|p| p.physical_tmux_name.as_str()),
        );
        let out = crate::idle_cli::run_idle(
            &store, &config, json, prompt, orchestrator.as_deref(), caller.as_deref(), args.port.unwrap_or(config.port),
        )
        .await?;
        println!("{out}");
        return Ok(ControlFlow::Continue(()));
    }

    if let Some(Command::Models { action: ModelsAction::Show { json } }) = command {
        let config = AppConfig::load_or_default();
        let rows = ninox_core::model_resolution::model_table(&config);
        if json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else {
            println!("{}", ninox_core::model_resolution::format_model_table(&rows));
        }
        return Ok(ControlFlow::Continue(()));
    }

    if let Some(Command::Plan { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        std::process::exit(run_plan_cli(action, db_path).await);
    }

    // Orchestrator-only, same gating as Plan above; needs only Store (for
    // orchestrator_auth) + the live pane identity, not the tmux-config/
    // wrapper-hook/self-shim setup below.
    if let Some(Command::Config { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        return run_config_cli(action, db_path).await.map(ControlFlow::Continue);
    }

    // Same hot-path rationale as Config above, but — unlike `ninox
    // config` — not gated to orchestrators specifically: `list`/`show`/
    // `export` are open to any caller, and a mutating action (`create`/
    // `edit`/`delete`/`reset`/`import`) is authorized per-call against the
    // caller's own LIVE session identity (see
    // `ninox_core::worker_type_cli::authorize_mutation`) — never the
    // forgeable `NINOX_SESSION` env var, since `can_manage_types`
    // enforcement would otherwise be defeated by `env -u NINOX_SESSION
    // ninox worker-type edit ...`. The pane-identity resolution itself
    // (async, so done here rather than inside `run_worker_type_cli`) is
    // just the fast path; `run_worker_type_cli` runs the full fallback
    // (`orchestrator_auth::resolve_live_caller_session_id`) once it has a
    // `Store` open, for the ptyd/ACP env-stripping gap that resolution
    // closes — see that function's doc comment.
    if let Some(Command::WorkerType { action }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        let pane_session_id = ninox_core::runtime::current_private_pane_identity().await.ok().flatten()
            .map(|p| p.physical_tmux_name);
        return run_worker_type_cli(action, db_path, pane_session_id).await.map(ControlFlow::Continue);
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
                return Ok(ControlFlow::Continue(()));
            }
            WorkerStatusAction::HookStop => {
                run_worker_status_hook(db_path, WorkerStatusHookKind::Stop, now);
                return Ok(ControlFlow::Continue(()));
            }
            WorkerStatusAction::HookNeedsInput => {
                run_worker_status_hook(db_path, WorkerStatusHookKind::NeedsInput, now);
                return Ok(ControlFlow::Continue(()));
            }
            WorkerStatusAction::HookPermission => {
                run_worker_status_hook(db_path, WorkerStatusHookKind::Permission, now);
                return Ok(ControlFlow::Continue(()));
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
                    WorkerStatusAction::HookPrompt
                    | WorkerStatusAction::HookStop
                    | WorkerStatusAction::HookNeedsInput
                    | WorkerStatusAction::HookPermission =>
                        unreachable!("hook verbs return above"),
                };
                println!("{}", run_worker_status(&store, parsed, self_session, now)?);
                return Ok(ControlFlow::Continue(()));
            }
        }
    }

    // `fleet ack`/`status`/`brief` are agent-invoked; only a real restore
    // needs the wrapper/shim setup below (it launches sessions).
    if let Some(Command::Fleet { action: fleet::FleetAction::Import(import) }) = command {
        let db_path = args.db.unwrap_or_else(default_db_path);
        let port = args.port.unwrap_or_else(|| AppConfig::load_or_default().port);
        return crate::fleet_import::run_cli(import, db_path, port).await.map(ControlFlow::Continue);
    }
    if let Some(Command::Fleet { action }) = command {
        if fleet::is_lightweight(&action) {
            let db_path = args.db.unwrap_or_else(default_db_path);
            std::fs::create_dir_all(db_path.parent().unwrap())?;
            let store = Arc::new(Store::open(&db_path)?);
            return fleet::run_cli(action, store, AppConfig::load_or_default()).await.map(ControlFlow::Continue);
        }
        command = Some(Command::Fleet { action });
    }
    if let Some(Command::Server { action: server_restart::ServerAction::Restart })
        | Some(Command::Service { action: service::ServiceAction::Restart }) = command
    {
        let db_path = args.db.unwrap_or_else(default_db_path);
        return server_restart::run_cli(args.port, db_path).await.map(ControlFlow::Continue);
    }
    if let Some(Command::Service { action }) = command {
        return service::run(action).map(ControlFlow::Continue);
    }
    if let Some(Command::Machine { action }) = command {
        return machine::run(action).map(ControlFlow::Continue);
    }
    if let Some(Command::Update { check, force, version, app, channel }) = command {
        return update_cli::run(update_cli::UpdateArgs { check, force, version, app, channel }).map(ControlFlow::Continue);
    }
    if let Some(Command::ResetBin) = command {
        let exe = ninox_core::hooks::canonical_exe()?;
        ninox_core::hooks::reset_self_shim(&exe, env!("CARGO_PKG_VERSION"))?;
        println!("{} -> {} ({})", ninox_core::hooks::self_shim_path().display(), exe.display(), env!("CARGO_PKG_VERSION"));
        return Ok(ControlFlow::Continue(()));
    }

    // Reached only once every hot-path subcommand above has already
    // returned, so this never slows down an agent-invoked command — see
    // MLOPS-5457. `AppConfig::load_or_default()` calls elsewhere already
    // cover the "best-effort config" case; this is the one place a plain
    // CLI invocation actually tells the user their config.toml is broken.
    if let Err(e) = AppConfig::load_detailed() {
        eprintln!("{}", e.banner_message());
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
    // The `ninox` self-shim forwards to whichever of ninox-gui/ninox-tui
    // installed it first (see the install-if-absent note below — switching
    // which one it targets is the deliberate `ninox reset-bin`); the `nx`
    // alias is installed only when the caller passes `install_nx_alias`
    // (ninox-tui only) since bare `nx` has always meant "open the terminal
    // UI". Both share this `exe` and this gate — reached only once every
    // hot-path subcommand above has already returned — so neither adds the
    // current-exe/canonicalize cost to a hot-path invocation.
    if let Ok(exe) = ninox_core::hooks::canonical_exe() {
        if AppConfig::is_scoped_instance() {
            // Same install-if-absent rule, minus the drift hint: a scoped
            // instance's shim lives in its own bin dir, so a mismatch there
            // is that instance's business, not a cue to touch the shared
            // one. The `nx` alias lives on the user's PATH, which belongs to
            // the default instance.
            if let Err(e) = ninox_core::hooks::install_self_shim(&exe, env!("CARGO_PKG_VERSION")) {
                tracing::warn!("failed to install ninox self-shim: {e}");
            }
        } else {
            // Install-if-absent: a working shim is only ever replaced by an
            // explicit `ninox reset-bin`, so just hint when it's behind.
            match ninox_core::hooks::install_self_shim(&exe, env!("CARGO_PKG_VERSION")) {
                // Which binary won a first install between two racing
                // processes is otherwise invisible — print it so the outcome
                // is observable instead of silent.
                Ok(true) => eprintln!("ninox: shared shim now points to {}", exe.display()),
                Ok(false) => {
                    if let Some(hint) = ninox_core::hooks::self_shim_drift_hint(&exe, env!("CARGO_PKG_VERSION")) {
                        eprintln!("{hint}");
                    }
                }
                Err(e) => tracing::warn!("failed to install ninox self-shim: {e}"),
            }
            if install_nx_alias {
                if let Err(e) = ninox_core::hooks::install_nx_alias(&exe, std::env::var_os("PATH").as_deref()) {
                    tracing::debug!("nx alias not installed: {e}");
                }
            }
        }
    }

    let db_path = args.db.unwrap_or_else(default_db_path);
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let store = Arc::new(Store::open(&db_path)?);
    // Only the long-lived process that owns the instance (a UI or the
    // daemon) records its store; a one-off `--db` CLI call must not repoint
    // every session of the instance.
    if matches!(command, None | Some(Command::Tui) | Some(Command::Gui)) {
        if let Err(e) = AppConfig::register_instance_db(&db_path) {
            tracing::warn!("failed to record this instance's store path: {e}");
        }
    }

    // Tui/Gui/no-subcommand all need an interactive UI only the calling
    // binary can provide — handed back as a `UiRequest` instead of being
    // dispatched from here.
    if matches!(command, Some(Command::Tui)) {
        return Ok(ControlFlow::Break(UiRequest::Tui(UiArgs {
            store, port_arg: args.port, headless: args.headless, db_path,
        })));
    }
    if matches!(command, Some(Command::Gui)) {
        return Ok(ControlFlow::Break(UiRequest::Gui(UiArgs {
            store, port_arg: args.port, headless: args.headless, db_path,
        })));
    }
    if command.is_none() {
        return Ok(ControlFlow::Break(UiRequest::Default(UiArgs {
            store, port_arg: args.port, headless: args.headless, db_path,
        })));
    }

    let result: anyhow::Result<()> = match command {
        Some(Command::Spawn { prompt, workspace, name, orchestrator_id, worker_type, success, success_only, no_queue, brains, model }) => {
            let config = AppConfig::load_or_default();
            // Resolved here (not inside `run_spawn`) so the verification
            // step stays a pure function of explicit inputs — see
            // `run_spawn`'s doc comment on `orchestrator_id`.
            let orchestrator_id = orchestrator_id.or_else(|| fleet::env_nonempty("NINOX_ORCHESTRATOR_ID"));
            // One pane lookup serves both: the pane `authorize_orchestrator`
            // verifies a claimed `orchestrator_id` against below, and the
            // fast path for the calling session's own LIVE identity
            // (`caller_session_id`) — deliberately never the forgeable
            // `NINOX_SESSION` env var, since `can_spawn` enforcement would
            // otherwise be defeated by `env -u NINOX_SESSION ninox spawn
            // ...`. A pane-identity miss falls back to a reverse ancestry
            // scan against every live managed session (closes the same gap
            // for a ptyd/ACP-backed caller whose env var was stripped) —
            // see `resolve_live_caller_session_id`'s doc comment.
            let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
            let pane_session_id = pane.as_ref().map(|p| p.physical_tmux_name.clone());
            let caller_session_id = ninox_core::orchestrator_auth::resolve_live_caller_session_id(&store, pane_session_id.as_deref());
            let caller = CallerRuntime {
                caller_type: fleet::env_nonempty("NINOX_CALLER_TYPE"),
                pane,
            };
            let brain = brains.resolve(&store, caller_session_id.as_deref(), &config)?;
            run_spawn_opts(store, config, prompt, workspace, name, orchestrator_id, caller, worker_type, caller_session_id, success, success_only, no_queue, Some(brain), model).await
        }
        Some(Command::SpawnOrchestrator { name, prompt, user_requested, no_queue, brains, model }) => {
            let config = AppConfig::load_or_default();
            let brain = brains.resolve(&store, verified_caller_session_id(&store).await.as_deref(), &config)?;
            run_spawn_orchestrator(store, config, name, prompt, user_requested, no_queue, brain, model).await
        }
        Some(Command::SpawnSubOrchestrator { name, prompt, orchestrator_id, no_queue, brains, model }) => {
            let config = AppConfig::load_or_default();
            let brain = brains.resolve(&store, verified_caller_session_id(&store).await.as_deref(), &config)?;
            let orchestrator_id = orchestrator_id.or_else(|| fleet::env_nonempty("NINOX_ORCHESTRATOR_ID"));
            let caller_session_id = fleet::env_nonempty("NINOX_SESSION");
            let caller = CallerRuntime {
                caller_type: fleet::env_nonempty("NINOX_CALLER_TYPE"),
                pane: ninox_core::runtime::current_private_pane_identity().await.ok().flatten(),
            };
            run_spawn_sub_orchestrator(store, config, name, prompt, orchestrator_id, caller_session_id, caller, no_queue, brain, model).await
        }
        Some(Command::Orchestrate { name, prompt, no_attach }) => {
            let config = AppConfig::load_or_default();
            let port = args.port.unwrap_or(config.port);
            run_orchestrate(store, config, port, db_path, name, prompt, no_attach).await
        }
        Some(Command::Tui) => unreachable!("Tui short-circuits and returns earlier in dispatch()"),
        Some(Command::Reap { session_ids, all, force, orchestrator_id }) => {
            run_reap(store, session_ids, all, force, orchestrator_id).await
        }
        Some(Command::Workspaces { action }) => {
            run_workspaces(&store, action).await
        }
        Some(Command::Send { session_id, message, kind, json }) => {
            let config = AppConfig::load_or_default();
            let sessions_dir = AppConfig::sessions_dir();
            // The sender is the CALLING process's own resolved identity —
            // same env-var + cwd resolution `worker-status` hooks use to
            // identify themselves — never taken from a CLI argument, so a
            // caller cannot simply claim to be any session it names.
            let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
            let cwd = std::env::current_dir().ok();
            let from = ninox_core::worker_status::resolve_session_id(
                &store, env_session.as_deref(), cwd.as_deref(),
            )
            .ok()
            .flatten();
            // The same process that is actually sending this message — its
            // live pane identity is the proof `attest_sender` cross-checks
            // `from` against (see `orchestrator_auth::attest_sender`).
            let sender_pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
            // This CLI process never holds an ACP connection itself — only
            // `ninox-server` does, for a session's full lifetime (see the
            // cross-harness-messaging design spec's "Connection ownership"
            // section) — so routing to one goes over its API rather than an
            // in-process `SessionBackend` call.
            let acp = ninox_core::acp_client::HttpAcpRouter { port: args.port.unwrap_or(config.port) };
            let kind: ninox_core::types::MessageKind = kind.map(Into::into).unwrap_or_default();
            let receipt = ninox_core::messaging::deliver_message_or_hold(
                &store, &sessions_dir, &session_id, &message, config.send_mechanism(),
                from.as_deref(), sender_pane.as_ref(), kind, &acp,
            )
            .await?;
            print_send_receipt(&receipt, json)
        }
        Some(Command::Messages { to_me: _, to, since, json }) => {
            let to = match to {
                Some(t) => t,
                None => {
                    let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
                    let cwd = std::env::current_dir().ok();
                    ninox_core::worker_status::resolve_session_id(&store, env_session.as_deref(), cwd.as_deref())
                        .ok()
                        .flatten()
                        .ok_or_else(|| anyhow::anyhow!("not running inside a Ninox session: pass --to <session>"))?
                }
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            println!("{}", run_messages(&store, &to, since.as_deref(), json, now)?);
            Ok(())
        }
        Some(Command::RequestWork { description }) => {
            run_request_work(&store, &description)
        }
        Some(Command::Prompt { action }) => {
            let config = AppConfig::load_or_default();
            run_prompt(store, config, action).await
        }
        Some(Command::Complete { summary, evidence, force }) => {
            let config = AppConfig::load_or_default();
            let port = args.port.unwrap_or(config.port);
            run_complete(store, config, port, summary, evidence, force).await
        }
        Some(Command::Fleet { action }) => {
            fleet::run_cli(action, store, AppConfig::load_or_default()).await
        }
        Some(Command::Restart { args }) => {
            let config = AppConfig::load_or_default();
            restart::run_cli(args, store, config, db_path).await
        }
        Some(Command::Service { .. }) | Some(Command::Server { .. }) => {
            unreachable!("Service and Server short-circuit and return earlier in main()")
        }
        Some(Command::Machine { .. }) => {
            unreachable!("Machine short-circuits and returns earlier in main()")
        }
        Some(Command::Update { .. }) => {
            unreachable!("Update short-circuits and returns earlier in main()")
        }
        Some(Command::ResetBin) => {
            unreachable!("ResetBin short-circuits and returns earlier in main()")
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
        Some(Command::Idle { .. }) => {
            unreachable!("Idle short-circuits and returns earlier in main()")
        }
        Some(Command::Models { .. }) => {
            unreachable!("Models short-circuits and returns earlier in main()")
        }
        Some(Command::Whoami { .. }) => {
            unreachable!("Whoami short-circuits and returns earlier in main()")
        }
        Some(Command::Pr { .. }) => {
            unreachable!("Pr short-circuits and returns earlier in dispatch()")
        }
        Some(Command::UserRequest { .. }) => {
            unreachable!("UserRequest short-circuits and returns earlier in dispatch()")
        }
        Some(Command::IdentityHook { .. }) => {
            unreachable!("IdentityHook short-circuits and returns earlier in main()")
        }
        Some(Command::PromptAnswer { .. }) => {
            unreachable!("PromptAnswer short-circuits and returns earlier in main()")
        }
        Some(Command::Plan { .. }) => {
            unreachable!("Plan short-circuits and returns earlier in main()")
        }
        Some(Command::Config { .. }) => {
            unreachable!("Config short-circuits and returns earlier in dispatch()")
        }
        Some(Command::Github { action }) => run_github(&store, action),
        Some(Command::Limits { action }) => {
            let pane_session_id = ninox_core::runtime::current_private_pane_identity().await.ok().flatten()
                .map(|p| p.physical_tmux_name);
            run_limits(&store, action, pane_session_id).await
        }
        Some(Command::WorkerType { .. }) => {
            unreachable!("WorkerType short-circuits and returns earlier in dispatch()")
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
        Some(Command::Gui) => unreachable!("Gui short-circuits and returns earlier in dispatch()"),
        None => unreachable!("absence of a command short-circuits and returns earlier in dispatch()"),
    };
    result.map(ControlFlow::Continue)
}

const PLAN_CLI_SCHEMA_VERSION: u32 = 1;

fn plan_error(code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "code": code, "message": message.into() })
}

/// `plan_error` for a generation failure, plus a per-problem `details` array
/// when the plan failed validation.
fn plan_error_with_details(err: &ninox_core::lifecycle::auto_plan::AutoPlanError) -> serde_json::Value {
    let mut value = plan_error(err.code(), err.to_string());
    if let ninox_core::lifecycle::auto_plan::AutoPlanError::ValidationFailed(errors) = err {
        value["details"] = errors
            .iter()
            .map(|e| serde_json::json!({ "code": e.code(), "message": e.to_string() }))
            .collect();
    }
    value
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
    let action = match action {
        PlanAction::Show { plan_id: None } => PlanAction::Show { plan_id: None },
        PlanAction::Show { plan_id: Some(id) } => return run_ghost_show(&store, &id),
        PlanAction::Propose { file } => {
            let caller = match ghost_caller(&store, runtime.as_ref(), false) {
                Ok(c) => c,
                Err(code) => return code,
            };
            return run_ghost_propose(&store, file, caller.as_deref());
        }
        PlanAction::List { pending } => return run_ghost_list(&store, pending),
        PlanAction::Accept { plan_id, nodes } => {
            let caller = match ghost_caller(&store, runtime.as_ref(), true) {
                Ok(c) => c,
                Err(code) => return code,
            };
            let config = AppConfig::load_or_default();
            let only = (!nodes.is_empty()).then_some(nodes);
            let result = crate::ghost_accept::accept(&store, &config, &plan_id, only.as_deref(), caller.as_deref(), now_millis_i64()).await;
            return emit_decision(result);
        }
        PlanAction::Discard { plan_id, nodes } => {
            let caller = match ghost_caller(&store, runtime.as_ref(), true) {
                Ok(c) => c,
                Err(code) => return code,
            };
            let only = (!nodes.is_empty()).then_some(nodes);
            return emit_decision(crate::ghost_accept::discard(&store, &plan_id, only.as_deref(), caller.as_deref(), now_millis_i64()));
        }
        other => other,
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
        PlanAction::Show { .. } => match store.get_orchestrator_plan(&orchestrator_id) {
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
        PlanAction::Propose { .. } | PlanAction::List { .. } | PlanAction::Accept { .. } | PlanAction::Discard { .. } => {
            unreachable!("ghost-plan actions return before orchestrator authorization")
        }
        PlanAction::Generate { task, workspace, propose } => {
            let config = AppConfig::load_or_default();
            let cap = config.nested_orchestration.max_depth;
            // A nested caller's sub-orchestrators sit deeper in absolute
            // terms, so it gets only the remaining depth.
            let caller_depth = match ninox_core::orchestrator_auth::orchestrator_depth(&store, &orchestrator_id, cap) {
                Ok(depth) => depth,
                Err(error) => {
                    emit_plan_envelope(false, serde_json::Value::Null, plan_error("operation_failed", error.to_string()));
                    return 1;
                }
            };
            let workspace = match workspace.map_or_else(std::env::current_dir, Ok) {
                Ok(workspace) => workspace,
                Err(error) => {
                    emit_plan_envelope(false, serde_json::Value::Null, plan_error("operation_failed", error.to_string()));
                    return 1;
                }
            };
            match ninox_core::lifecycle::auto_plan::generate_plan(
                &ninox_core::lifecycle::auto_plan::ClaudePlanRunner,
                &config.worker_type_registry(),
                &task,
                workspace.clone(),
                &orchestrator_id,
                &config.planning.harness,
                config.planning.model.clone(),
                cap.saturating_sub(caller_depth),
            )
            .await
            {
                Ok(plan) => {
                    let _ = store.clear_harness_auth(&format!("plan:{}", config.planning.harness));
                    if propose {
                        let proposal = ninox_core::ghost_plan::Proposal {
                            parent_id: Some(plan.parent_id.clone()),
                            title: Some(first_words(&task, 8)),
                            sections: plan.sections.iter().map(|s| ninox_core::ghost_plan::ProposalSection {
                                id: s.id.clone(),
                                parent_id: Some(s.parent_id.clone()),
                                worker_type: s.worker_type.clone(),
                                name: None,
                                brief: s.brief.clone(),
                                dependencies: s.dependencies.clone(),
                                await_done: false,
                                workspace: (s.worker_type.as_deref() != Some("Orchestrator"))
                                    .then(|| workspace.to_string_lossy().into_owned()),
                                model: None,
                                brain_reads: Vec::new(),
                                brain_write: None,
                                linear: None,
                            }).collect(),
                        };
                        return store_proposal(&store, &config, &proposal, Some(&orchestrator_id), &orchestrator_id);
                    }
                    emit_plan_envelope(true, serde_json::to_value(&plan).unwrap_or_default(), serde_json::Value::Null);
                    0
                }
                Err(error) => {
                    if let ninox_core::lifecycle::auto_plan::AutoPlanError::SubprocessFailed(detail) = &error {
                        ninox_core::harness_auth::record_output_failure(&store, &config.planning.harness, "plan", detail);
                    }
                    emit_plan_envelope(false, serde_json::Value::Null, plan_error_with_details(&error));
                    1
                }
            }
        }
    }
}

/// The pane-verified caller for a ghost-plan command, failing closed (see
/// `ghost_plan::resolve_decider_caller`). For a decision, a process
/// orphaned to launchd/init is refused too: a detached child of a managed
/// session would otherwise have no managed ancestor left and pass as the
/// user. Emits the error envelope and returns the exit code on refusal.
fn ghost_caller(store: &Store, pane: Option<&ninox_core::runtime::PaneIdentity>, decision: bool) -> Result<Option<String>, i32> {
    let refuse = |message: String| {
        emit_plan_envelope(false, serde_json::Value::Null, plan_error("authorization_failed", message));
        2
    };
    let caller = ninox_core::ghost_plan::resolve_decider_caller(store, pane.map(|p| p.physical_tmux_name.as_str()))
        .map_err(|e| refuse(format!("{e:#}")))?;
    if decision && caller.is_none() && std::os::unix::process::parent_id() == 1 {
        return Err(refuse("refusing a detached process (its parent is pid 1); run this from a terminal".into()));
    }
    Ok(caller)
}

fn now_millis_i64() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// The JSON `ninox plan list/show` print per plan.
pub fn ghost_plan_json(plan: &ninox_core::ghost_plan::PlanWithNodes) -> serde_json::Value {
    serde_json::json!({
        "plan_id": plan.plan.id,
        "status": plan.plan.status,
        "proposer": plan.plan.proposer,
        "target_parent": plan.plan.target_parent,
        "title": plan.plan.title,
        "created_at": plan.plan.created_at,
        "expires_at": plan.plan.expires_at,
        "decided_by": plan.plan.decided_by,
        "decided_at": plan.plan.decided_at,
        "nodes": plan.nodes.iter().map(|n| serde_json::json!({
            "node_id": n.node_id,
            "parent_node_id": n.parent_node_id,
            "worker_type": n.worker_type,
            "name": n.name,
            "brief": n.brief,
            "workspace": n.workspace,
            "model": n.model,
            "brain": n.brain,
            "dependencies": n.dependencies,
            "await_done": n.await_done,
            "linear": n.linear_ref,
            "state": n.state,
            "session_id": n.session_id,
            "note": n.error,
        })).collect::<Vec<_>>(),
    })
}

fn store_proposal(
    store: &Store,
    config: &AppConfig,
    proposal: &ninox_core::ghost_plan::Proposal,
    proposer_session: Option<&str>,
    proposer_label: &str,
) -> i32 {
    let validated = match ninox_core::ghost_plan::prepare_proposal(store, config, proposal, proposer_session) {
        Ok(v) => v,
        Err(errors) => {
            let mut error = plan_error("invalid_plan", format!("the plan was not stored: {} problem(s)", errors.len()));
            error["details"] = errors.iter().map(|e| serde_json::json!({ "code": e.code, "message": e.message })).collect();
            emit_plan_envelope(false, serde_json::Value::Null, error);
            return 1;
        }
    };
    let now = now_millis_i64();
    let id = ninox_core::ghost_plan::new_plan_id(now);
    let expires_at = now + config.plan_proposals.expiry_ms();
    if let Err(e) = store.insert_ghost_plan(&id, proposer_label, &validated, now, expires_at) {
        emit_plan_envelope(false, serde_json::Value::Null, plan_error("database_error", e.to_string()));
        return 4;
    }
    emit_plan_envelope(
        true,
        serde_json::json!({
            "plan_id": id,
            "status": "pending",
            "node_ids": validated.nodes.iter().map(|n| n.node_id.clone()).collect::<Vec<_>>(),
            "expires_at": expires_at,
            "note": "proposed only — nothing is spawned until the user accepts it",
        }),
        serde_json::Value::Null,
    );
    0
}

fn run_ghost_propose(store: &Store, file: Option<PathBuf>, caller: Option<&str>) -> i32 {
    let raw = match file.filter(|f| f.as_os_str() != "-") {
        Some(path) => std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display())),
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf).map(|_| buf).map_err(|e| e.to_string())
        }
    };
    let raw = match raw {
        Ok(raw) => raw,
        Err(e) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("operation_failed", e));
            return 1;
        }
    };
    let proposal: ninox_core::ghost_plan::Proposal = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("invalid_plan", format!("not a plan: {e}")));
            return 1;
        }
    };
    let config = AppConfig::load_or_default();
    store_proposal(store, &config, &proposal, caller, caller.unwrap_or(ninox_core::ghost_plan::USER))
}

fn run_ghost_list(store: &Store, pending: bool) -> i32 {
    let _ = store.expire_ghost_plans(now_millis_i64());
    match store.list_ghost_plans(pending) {
        Ok(plans) => {
            emit_plan_envelope(true, serde_json::json!({ "plans": plans.iter().map(ghost_plan_json).collect::<Vec<_>>() }), serde_json::Value::Null);
            0
        }
        Err(e) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("database_error", e.to_string()));
            4
        }
    }
}

fn run_ghost_show(store: &Store, id: &str) -> i32 {
    let _ = store.expire_ghost_plans(now_millis_i64());
    match store.get_ghost_plan(id) {
        Ok(Some(plan)) => {
            emit_plan_envelope(true, ghost_plan_json(&plan), serde_json::Value::Null);
            0
        }
        Ok(None) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("not_found", format!("no plan '{id}'")));
            3
        }
        Err(e) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("database_error", e.to_string()));
            4
        }
    }
}

fn emit_decision(result: anyhow::Result<crate::ghost_accept::DecisionReport>) -> i32 {
    match result {
        Ok(report) => {
            let any_failed = report.nodes.iter().any(|n| matches!(n.outcome, crate::ghost_accept::NodeOutcome::Failed { .. }));
            let mut data = serde_json::to_value(&report).unwrap_or_default();
            data["summary"] = report.summary().into();
            emit_plan_envelope(true, data, serde_json::Value::Null);
            i32::from(any_failed)
        }
        Err(e) => {
            emit_plan_envelope(false, serde_json::Value::Null, plan_error("refused", format!("{e:#}")));
            2
        }
    }
}

/// Unlike `run_plan_cli`'s bespoke JSON-envelope/exit-code protocol (`plan`
/// is machine-parsed), `ninox config` is a direct human/agent CLI surface —
/// errors bubble up as a plain `anyhow::Result`, matching `run_spawn`'s
/// orchestrator-claim verification (`cli.rs`, `run_spawn`).
/// `none`/`0`/`unlimited` → `None`.
fn parse_limit_value(raw: &str) -> anyhow::Result<Option<u32>> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "unlimited" | "none" | "0" => Ok(None),
        n => n.parse::<u32>().map(Some).map_err(|_| anyhow::anyhow!(
            "'{raw}' is not a session limit — use a whole number or `unlimited`"
        )),
    }
}

pub(crate) fn render_limits(summary: &[ninox_core::session_limits::LocationStatus]) -> String {
    summary
        .iter()
        .map(|l| {
            let limit = l.limit.map_or("unlimited".to_string(), |n| n.to_string());
            format!("{:<20} {:>3} workers  limit {:<9}  {} queued", l.label, l.running, limit, l.queued)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_limits(store: &Store, action: LimitsAction, pane_session_id: Option<String>) -> anyhow::Result<()> {
    use ninox_core::session_limits;
    match action {
        LimitsAction::Show { json } => {
            let config = AppConfig::load_or_default();
            let summary = session_limits::summarize(store, &config)?;
            if json {
                let rows: Vec<_> = summary.iter().map(|l| serde_json::json!({
                    "location": l.label,
                    "machine_id": l.machine_id,
                    "running": l.running,
                    "orchestrators": l.orchestrators,
                    "limit": l.limit,
                    "queued": l.queued,
                })).collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("{}", render_limits(&summary));
                println!("{}", github_budget_line(store.get_github_budget()?.as_ref()));
            }
        }
        LimitsAction::Set { location, value } => {
            // Workers may not change how many sessions run; orchestrators and
            // a human outside any ninox session may.
            let caller = ninox_core::orchestrator_auth::resolve_live_caller_session_id(store, pane_session_id.as_deref());
            if caller.is_some() {
                let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
                ninox_core::orchestrator_auth::authorize_orchestrator(
                    store,
                    std::env::var("NINOX_ORCHESTRATOR_ID").ok().as_deref(),
                    std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
                    pane.as_ref(),
                )
                .map_err(|e| anyhow::anyhow!("not authorized to change session limits: {e}"))?;
            }
            let limit = parse_limit_value(&value)?;
            let config = AppConfig::load()?;
            let machine = session_limits::parse_location(&config, &location)?;
            let (saved, ()) = AppConfig::update(|c| c.limits.set_limit(machine.as_deref(), limit))?;
            let label = session_limits::location_label(&saved, machine.as_deref());
            match saved.limits.limit_for(machine.as_deref()) {
                Some(n) => println!("{label}: at most {n} concurrent sessions"),
                None => println!("{label}: unlimited"),
            }
        }
    }
    Ok(())
}

async fn run_config_cli(action: ConfigAction, db_path: PathBuf) -> anyhow::Result<()> {
    if let Some(parent) = db_path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let store = Store::open(db_path)?;
    let runtime = ninox_core::runtime::current_private_pane_identity().await?;
    ninox_core::orchestrator_auth::authorize_orchestrator(
        &store,
        std::env::var("NINOX_ORCHESTRATOR_ID").ok().as_deref(),
        std::env::var("NINOX_CALLER_TYPE").ok().as_deref(),
        runtime.as_ref(),
    )
    .map_err(|e| anyhow::anyhow!("not authorized to run `ninox config`: {e}"))?;

    match action {
        ConfigAction::Get { key } => println!("{}", ninox_core::config_cli::get(&key)?),
        ConfigAction::Set { key, value } => {
            let candidate = ninox_core::config_cli::set(&key, &value)?;
            if !ninox_core::config_cli::is_known_key(&key, &candidate) {
                eprintln!("warning: '{key}' isn't a recognized config field; it was written but ninox will ignore it");
            }
            let echoed = if ninox_core::config_cli::is_secret_key(&key) { "<redacted>" } else { &value };
            println!("{key} = {echoed}");
        }
        ConfigAction::Show => println!("{}", ninox_core::config_cli::show()?),
    }
    Ok(())
}

/// Resolves a `--brief`/`--brief`-like value that may be `@<path>` (read the
/// template from a file) or a literal string.
fn resolve_text_or_file(value: &str) -> anyhow::Result<String> {
    match value.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
            .map_err(|e| anyhow::anyhow!("failed to read brief template from '{path}': {e}")),
        None => Ok(value.to_string()),
    }
}

fn print_worker_type_entry(entry: &ninox_core::worker_type_cli::Entry, json: bool) -> anyhow::Result<()> {
    if json {
        #[derive(serde::Serialize)]
        struct Json<'a> {
            #[serde(flatten)]
            spec: &'a ninox_core::worker_type::WorkerTypeSpec,
            built_in: bool,
        }
        println!("{}", serde_json::to_string_pretty(&Json { spec: &entry.spec, built_in: entry.built_in })?);
        return Ok(());
    }
    let badge = if entry.built_in { " (built-in)" } else { "" };
    println!("{}{badge}", entry.spec.name);
    println!("  brief_template: {}", entry.spec.brief_template.as_deref().unwrap_or("(none — prompt passed through unchanged)"));
    println!("  harness: {}  model: {}", entry.spec.harness, entry.spec.model.as_deref().unwrap_or("(harness default)"));
    println!(
        "  can_spawn: {}",
        if entry.spec.can_spawn.is_empty() { "none".to_string() } else { entry.spec.can_spawn.join(", ") },
    );
    println!(
        "  can_manage_types: {} (whether a session of this type may run `worker-type create/edit/delete/reset/import`)",
        entry.spec.can_manage_types,
    );
    println!(
        "  permissions: {} (allow: {}; deny: {}; ask: {})",
        if entry.spec.permissions.enabled { "enforced" } else { "not enforced" },
        entry.spec.permissions.allow.join(", "),
        entry.spec.permissions.deny.join(", "),
        entry.spec.permissions.ask.join(", "),
    );
    if entry.spec.hidden {
        println!("  hidden: true (system type — not spawnable via `ninox spawn`)");
    }
    Ok(())
}

/// `ninox worker-type list`: for each type the caller may spawn, its name,
/// when to use it, the harness and model it really runs on, and what it may
/// itself spawn. The JSON keeps the full spec (as before) plus the resolved
/// harness/model.
fn format_worker_type_list(
    entries: &[ninox_core::worker_type_cli::Entry],
    config: &AppConfig,
    json: bool,
) -> anyhow::Result<String> {
    use ninox_core::model_resolution::{effective_agent, SpawnModelRequest};
    let resolved = |spec: &ninox_core::worker_type::WorkerTypeSpec| {
        let req = if spec.name == "Orchestrator" { SpawnModelRequest::orchestrator(Some(spec)) } else { SpawnModelRequest::worker(Some(spec)) };
        effective_agent(&req, config)
    };
    if json {
        #[derive(serde::Serialize)]
        struct Json<'a> {
            #[serde(flatten)]
            spec: &'a ninox_core::worker_type::WorkerTypeSpec,
            built_in: bool,
            use_when_line: &'a str,
            resolved_harness: String,
            resolved_model: Option<String>,
        }
        let rows: Vec<Json> = entries.iter().map(|e| {
            let r = resolved(&e.spec);
            Json {
                spec: &e.spec,
                built_in: e.built_in,
                use_when_line: e.spec.use_when_line(),
                resolved_harness: r.agent.harness.clone(),
                resolved_model: r.display_model.clone(),
            }
        }).collect();
        return Ok(format!("{}\n", serde_json::to_string_pretty(&rows)?));
    }
    if entries.is_empty() {
        return Ok("no worker types this session may spawn\n".to_string());
    }
    let mut out = String::new();
    for e in entries {
        let r = resolved(&e.spec);
        let can_spawn = if e.spec.can_spawn.is_empty() { "none".to_string() } else { e.spec.can_spawn.join(", ") };
        out.push_str(&format!(
            "{}{} — {}\n    {} · model {} · can spawn: {}\n",
            e.spec.name,
            if e.built_in { " (built-in)" } else { "" },
            e.spec.use_when_line(),
            r.agent.harness,
            r.describe(),
            can_spawn,
        ));
    }
    out.push_str(&format!("Default when none fits better: {}\n", ninox_core::worker_type::DEFAULT_SPAWN_TYPE));
    Ok(out)
}

fn worker_type_fields_from_create(
    harness: Option<String>, model: Option<String>, allow: Vec<String>, deny: Vec<String>,
    ask: Vec<String>, can_spawn: Option<String>, can_manage_types: Option<bool>,
) -> ninox_core::worker_type_cli::Fields {
    ninox_core::worker_type_cli::Fields { brief: None, harness, model, allow, deny, ask, can_spawn, can_manage_types, ..Default::default() }
}

async fn run_worker_type_cli(action: WorkerTypeAction, db_path: PathBuf, pane_session_id: Option<String>) -> anyhow::Result<()> {
    use ninox_core::worker_type_cli::{create, delete, edit, export, import, reset, show, Fields};

    // show/export never touch the store. `list` and the mutating actions
    // below need it to resolve the caller's own worker type (`list` filters
    // by what it may spawn; mutations go through `authorize_mutation`).
    match action {
        WorkerTypeAction::Show { name, json } => {
            print_worker_type_entry(&show(&name)?, json)?;
            return Ok(());
        }
        WorkerTypeAction::Export { names } => {
            print!("{}", export(&names)?);
            return Ok(());
        }
        _ => {}
    }

    if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let store = Store::open(db_path)?;
    // The pane-identity fast path from dispatch(), with the ptyd/ACP
    // env-stripping fallback (reverse ancestry scan against every live
    // managed session) now that a `Store` is available to scan — see
    // `resolve_live_caller_session_id`'s doc comment.
    let caller_session_id = ninox_core::orchestrator_auth::resolve_live_caller_session_id(&store, pane_session_id.as_deref());
    let caller_session_id = caller_session_id.as_deref();

    match action {
        WorkerTypeAction::List { json } => {
            let config = AppConfig::load_or_default();
            let entries = ninox_core::worker_type_cli::list_spawnable(&store, caller_session_id)?;
            print!("{}", format_worker_type_list(&entries, &config, json)?);
        }
        WorkerTypeAction::Create { name, brief, use_when, harness, model, allow, deny, ask, can_spawn, can_manage_types } => {
            let brief = resolve_text_or_file(&brief)?;
            let spec = create(
                &store, caller_session_id, &name, &brief,
                Fields { use_when, ..worker_type_fields_from_create(harness, model, allow, deny, ask, can_spawn, can_manage_types) },
            )?;
            println!("created worker type '{}'", spec.name);
        }
        WorkerTypeAction::Edit {
            name, brief, use_when, harness, model, allow, clear_allow, deny, clear_deny, ask, clear_ask, can_spawn, can_manage_types,
        } => {
            let brief = brief.map(|b| resolve_text_or_file(&b)).transpose()?;
            let spec = edit(&store, caller_session_id, &name, Fields {
                brief, use_when, harness, model, allow, deny, ask, clear_allow, clear_deny, clear_ask, can_spawn, can_manage_types,
                permissions_enabled: None,
            })?;
            println!("updated worker type '{}'", spec.name);
        }
        WorkerTypeAction::Delete { name } => {
            delete(&store, caller_session_id, &name)?;
            println!("deleted worker type '{name}'");
        }
        WorkerTypeAction::Reset { name } => {
            let spec = reset(&store, caller_session_id, &name)?;
            println!("reset '{}' to its compiled-in default", spec.name);
        }
        WorkerTypeAction::Import { file } => {
            let src = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("failed to read '{}': {e}", file.display()))?;
            let imported = import(&store, caller_session_id, &src)?;
            println!("imported {} worker type(s): {}", imported.len(), imported.join(", "));
        }
        WorkerTypeAction::Show { .. } | WorkerTypeAction::Export { .. } => {
            unreachable!("handled and returned above")
        }
    }
    Ok(())
}

/// The caller's own `NINOX_CALLER_TYPE` and live tmux-pane identity, as
/// resolved once by `dispatch` and threaded through to `run_spawn`'s
/// orchestrator-claim verification — bundled (rather than two more loose
/// `run_spawn` params) so the verification step stays a pure, directly
/// testable function of explicit inputs (mirrors
/// `resolve_reap_orchestrator`/`caller_is_orchestrator`).
struct CallerRuntime {
    caller_type: Option<String>,
    pane:        Option<ninox_core::runtime::PaneIdentity>,
}

/// Why a requested `--type`/Worker-type-field name can't be spawned —
/// shared between `run_spawn` and `spawn_standalone_worker` so a future
/// restriction added to one path can't silently miss the other. Each
/// caller still renders its own error text: the two paths offer different
/// remedies (`ninox spawn-orchestrator` vs. picking another dialog field).
pub(crate) enum InvalidWorkerType {
    Unknown,
    Hidden,
    IsOrchestrator,
}

/// Looks up `requested_type_name` and rejects it if it's unknown, a hidden
/// system type, or `Orchestrator` (which has its own dedicated spawn path).
pub(crate) fn resolve_requested_worker_type(
    type_registry: &ninox_core::worker_type::WorkerTypeRegistry,
    requested_type_name: &str,
) -> Result<ninox_core::worker_type::WorkerTypeSpec, InvalidWorkerType> {
    let requested_spec = type_registry.get(requested_type_name).cloned().ok_or(InvalidWorkerType::Unknown)?;
    if requested_spec.hidden {
        return Err(InvalidWorkerType::Hidden);
    }
    if requested_spec.name == "Orchestrator" {
        return Err(InvalidWorkerType::IsOrchestrator);
    }
    Ok(requested_spec)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_spawn(
    store: Arc<Store>,
    config: AppConfig,
    prompt: String,
    workspace: String,
    name: Option<String>,
    orchestrator_id: Option<String>,
    caller: CallerRuntime,
    worker_type: Option<String>,
    caller_session_id: Option<String>,
    success: Vec<String>,
    success_only: bool,
) -> anyhow::Result<()> {
    run_spawn_opts(
        store, config, prompt, workspace, name, orchestrator_id, caller, worker_type,
        caller_session_id, success, success_only, false, None, None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_spawn_opts(
    store: Arc<Store>,
    config: AppConfig,
    prompt: String,
    workspace: String,
    name: Option<String>,
    // The claimed orchestrator (explicit `--orchestrator-id`, or the ambient
    // `NINOX_ORCHESTRATOR_ID` fallback) — resolved by the caller (`dispatch`)
    // but NOT yet trusted; see the verification below.
    orchestrator_id: Option<String>,
    caller: CallerRuntime,
    // Named `WorkerTypeSpec` to resolve for this spawn (`ninox spawn --type
    // <name>`), or `None` to default to `Implementor` (today's unaffected,
    // backward-compatible path — see below).
    worker_type: Option<String>,
    // The calling session's own LIVE, pane-verified id — resolved by the
    // caller (`dispatch`, from the live tmux/ptyd pane's own name, never
    // the forgeable `NINOX_SESSION` env var) and passed in — same
    // "resolved once, threaded through" rationale as `orchestrator_id`/
    // `caller` above. `None` means no ninox-managed pane at all (a bare
    // human/GUI/TUI call). Drives `can_spawn` enforcement and
    // non-orchestrator parenting below — see
    // `orchestrator_auth::resolve_caller_worker_type`'s doc comment.
    caller_session_id: Option<String>,
    // `ninox spawn --success`/`--success-only` — see `resolve_success_criteria`.
    success: Vec<String>,
    success_only: bool,
    // `ninox spawn --no-queue`: fail instead of queueing over the limit.
    no_queue: bool,
    // Already narrowed from the caller's set; `None` inherits it.
    brain: Option<ninox_core::brain_set::BrainSet>,
    // `ninox spawn --model`: an explicit per-spawn model.
    model: Option<String>,
) -> anyhow::Result<()> {
    let type_registry = config.worker_type_registry();
    // Always resolved, even with no `--type` (defaults to `Implementor`),
    // so the hidden/Orchestrator/can_spawn checks below apply uniformly to
    // every spawn — an untyped spawn is just a spawn of the default type.
    let requested_type_name = worker_type.clone().unwrap_or_else(|| "Implementor".to_string());
    let requested_spec = resolve_requested_worker_type(&type_registry, &requested_type_name).map_err(|e| match e {
        InvalidWorkerType::Unknown => anyhow::anyhow!(
            "unknown worker type '{requested_type_name}' — valid types: {}",
            type_registry.user_facing_names().join(", "),
        ),
        InvalidWorkerType::Hidden => anyhow::anyhow!(
            "worker type '{requested_type_name}' is a system type and cannot be spawned via `ninox spawn`",
        ),
        InvalidWorkerType::IsOrchestrator => anyhow::anyhow!(
            "spawning an Orchestrator isn't done through `ninox spawn --type Orchestrator` — \
             use `ninox spawn-orchestrator` (a peer) or `ninox spawn-sub-orchestrator` \
             (nested under an already-verified orchestrator) instead"
        ),
    })?;
    // The CALLING session's own type (not the type being requested) — see
    // `resolve_caller_worker_type`'s doc comment for the orchestrator/
    // untyped-session resolution rules this leans on.
    let caller_worker_type = ninox_core::orchestrator_auth::resolve_caller_worker_type(&store, caller_session_id.as_deref());
    if let Some(caller_type_name) = &caller_worker_type {
        let caller_spec = type_registry.get(caller_type_name).cloned().unwrap_or_default();
        if !caller_spec.allows_spawning(&requested_spec.name) {
            anyhow::bail!(
                "worker type '{caller_type_name}' is not allowed to spawn '{}' — its \
                 can_spawn is [{}]",
                requested_spec.name,
                if caller_spec.can_spawn.is_empty() { "none".to_string() } else { caller_spec.can_spawn.join(", ") },
            );
        }
    }
    // Harness/model go through `model_resolution::effective_agent`. A named
    // `--type` supplies the harness and, if set, the model; a bare `ninox
    // spawn` keeps `[worker].harness` (the backend spec's "unaffected,
    // backward compatible" guarantee) and only picks up Implementor's model
    // when it is set for that harness. If the type carries a brief
    // template it composes with the task prompt before anything downstream
    // reads `prompt`. `raw_prompt` (pre-template) is what the display name
    // and fleet summary derive from below — the composed prompt would
    // otherwise collapse every untyped-name spawn of a type to the same
    // name/summary.
    let raw_prompt = prompt.clone();
    let model_request = ninox_core::model_resolution::SpawnModelRequest::worker(Some(&requested_spec)).with_model(model.as_deref());
    let (model_request, worker_type_name, prompt) = match worker_type.as_deref() {
        None => (model_request.with_harness(Some(&config.worker.harness)), Some("Implementor".to_string()), prompt),
        Some(_) => {
            let composed_prompt = requested_spec.compose_prompt(&prompt)?;
            (model_request, Some(requested_spec.name.clone()), composed_prompt)
        }
    };
    let agent = ninox_core::model_resolution::effective_agent(&model_request, &config).agent;
    // `requested_spec` is already fully resolved above (defaults to
    // Implementor for an untyped spawn, hidden/hidden-Orchestrator/unknown
    // name already rejected) — its `success` is the type default this
    // spawn's criteria build from.
    let resolved_criteria = resolve_success_criteria(&requested_spec.success, &success, success_only);
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
    registry.ensure_permissions_enforceable(&agent.harness, Some(&requested_spec))?;
    // Parent resolution. An orchestrator caller (or no calling session at
    // all) keeps today's exact claim-and-verify flow below, unchanged. A
    // non-orchestrator caller that just cleared its `can_spawn` check above
    // is parented to ITSELF — the brief's "a session spawned by a
    // non-orchestrator type is parented to its spawner" — ignoring any
    // `--orchestrator-id`/`NINOX_ORCHESTRATOR_ID` claim entirely (that
    // names the CALLER's own parent, not where this new sub-session
    // belongs) and skipping `authorize_orchestrator`'s pane-identity
    // verification: `can_spawn` is a policy guardrail on which roles may
    // create what, not a trust boundary like claiming to BE a verified
    // orchestrator, so no stronger proof than the session row is required.
    let orchestrator_id = match &caller_worker_type {
        Some(t) if t != "Orchestrator" => caller_session_id.clone(),
        _ => match orchestrator_id {
            // A claimed orchestrator is just that — a claim.
            // `--orchestrator-id` and `NINOX_ORCHESTRATOR_ID` are both
            // forgeable by anything running in the same shell (every
            // worker already carries its parent's id in the latter).
            // Require the same tmux-pane-identity + process-ancestry
            // cross-check `ninox plan` already requires
            // (`orchestrator_auth::authorize_orchestrator`) before trusting
            // it, and do it BEFORE any side effect (worktree creation,
            // session upsert) below — a rejected claim must leave nothing
            // behind. No claim at all (`None`) needs no verification: a
            // standalone worker spawned without an orchestrator is
            // unaffected.
            None => None,
            Some(claimed) => Some(
                ninox_core::orchestrator_auth::authorize_orchestrator(
                    &store, Some(&claimed), caller.caller_type.as_deref(), caller.pane.as_ref(),
                )
                .map_err(|e| anyhow::anyhow!(
                    "refusing to spawn a worker under orchestrator '{claimed}': caller could not \
                     be verified as that orchestrator ({e})"
                ))?,
            ),
        },
    };
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
    // See `messaging::is_reserved_session_id` for what each reserved id guards.
    if ninox_core::messaging::is_reserved_session_id(&id) {
        anyhow::bail!("'{id}' is reserved for ninox itself — pick another name");
    }
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
    let display_name = name.unwrap_or_else(|| first_words(&raw_prompt, 4));
    // The fleet card summary: the first line of the raw task prompt, before
    // the worker-context footer (or a worker type's brief template) is
    // applied.
    let summary = first_line(&raw_prompt, 140);

    let brain = match brain {
        Some(b) => b,
        None => ninox_core::brain_set::resolve_child(&store, caller_session_id.as_deref(), &config, &[], None)?,
    };
    let plan = crate::session_queue::WorkerLaunch {
        id: id.clone(),
        workspace,
        display_name,
        summary,
        prompt,
        orchestrator_id,
        agent,
        worker_type: worker_type_name,
        success_criteria: resolved_criteria,
        started_at: ts,
        ninox_brain: Some(brain.write_str()),
        ninox_config: std::env::var("NINOX_CONFIG").ok(),
        brain: Some(brain),
    };
    let spawned = admit_and_launch_worker(&store, &config, plan, no_queue).await?;
    match spawned.queued {
        Some(message) => println!("{message}"),
        None => println!("spawned {}", spawned.id),
    }
    Ok(())
}

/// Admission under the session limit, then the launch: the shared tail of
/// `ninox spawn` and an accepted ghost-plan worker node.
pub(crate) async fn admit_and_launch_worker(
    store: &Store,
    config: &AppConfig,
    plan: crate::session_queue::WorkerLaunch,
    no_queue: bool,
) -> anyhow::Result<SpawnedSession> {
    let id = plan.id.clone();
    let payload = crate::session_queue::QueuedLaunch::CliWorker(plan.clone());
    match crate::session_queue::admit(store, ninox_core::store::QueueKind::Spawn, &plan.skeleton(), &payload, no_queue)? {
        crate::session_queue::Admit::Queued { message, .. } => {
            return Ok(SpawnedSession { id, workspace: String::new(), queued: Some(message), warnings: Vec::new() });
        }
        crate::session_queue::Admit::Admitted => {}
    }
    let result = launch_cli_worker(store, config, plan).await;
    crate::session_queue::release(store, &id);
    result.map(|()| SpawnedSession { id, workspace: String::new(), queued: None, warnings: Vec::new() })
}

/// A fresh launch's permission mode, from the same function every
/// relaunch consults (`restart::session_permission_mode`).
pub(crate) fn fresh_permission_mode(config: &AppConfig, kind: LaunchKind, skeleton: &Session) -> PermissionMode {
    let types = config.worker_type_registry();
    permission_mode(kind, skeleton.worker_type.as_deref().and_then(|t| types.get(t)))
}

/// The launch half of `ninox spawn`: everything after validation and
/// admission. Also how a queued `ninox spawn` is started later.
pub(crate) async fn launch_cli_worker(
    store: &Store,
    config: &AppConfig,
    plan: crate::session_queue::WorkerLaunch,
) -> anyhow::Result<()> {
    let skeleton = plan.skeleton();
    let permission_mode = fresh_permission_mode(config, LaunchKind::Worker, &skeleton);
    let brain = plan.brain_set();
    let crate::session_queue::WorkerLaunch {
        id, workspace, prompt, orchestrator_id, agent, success_criteria: resolved_criteria,
        ninox_brain, ninox_config, ..
    } = plan;
    let brain_reads_env = brain.as_ref().map(|b| b.reads_env_value());
    let registry = config.registry();
    // Re-checked here: a queued spawn launches later, against whatever the
    // config says by then.
    if registry.spec(&agent.harness).worker_args.is_none() {
        anyhow::bail!("harness '{}' can no longer run workers (no worker_args) — not starting {id}", agent.harness);
    }

    // Create an isolated git worktree so workers don't share a branch.
    // Falls back to the shared workspace if the repo check fails (e.g. not git).
    let effective_workspace = match create_worker_worktree(
        &workspace, &id,
        config.send_mechanism() == SendMechanism::Inbox,
        config,
    ).await {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("worktree creation failed for {id}, using shared workspace: {e}");
            workspace.clone()
        }
    };
    crate::spawn_util::seed_permissions_for_type_name(
        std::path::Path::new(&effective_workspace), &agent.harness, skeleton.worker_type.as_deref(), config,
    )?;
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
    seed_worker_skills(&effective_workspace, config, &agent.harness).await;

    // Derive the GitHub repo slug from the workspace's git remote so that
    // poll_github can call the GitHub API with the correct owner/repo.
    let repo = repo_from_workspace(&workspace).unwrap_or_default();

    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    let orch_id_env = orchestrator_id.as_deref().unwrap_or("").to_string();

    // Append worker context so every agent knows its session ID, its
    // orchestrator's ID, and how to communicate back when done or stuck.
    let task_brief = prompt.clone();
    let effective_prompt = compose_effective_prompt(
        prompt, &id, &orch_id_env, config.pr_watch.enabled, &resolved_criteria,
    );

    let claude_session_id = ninox_core::harness::new_claude_session_id();

    let session = Session {
        repo,
        workspace_path: Some(effective_workspace.clone()),
        claude_session_id: Some(claude_session_id.clone()),
        ..skeleton
    };

    store.upsert_session(&session)?;
    store.set_permission_mode(&id, permission_mode)?;
    // The task brief and branch are what a fleet restore re-briefs this
    // worker with if its conversation can't be resumed.
    let branch = ninox_core::fleet::probe::current_branch(std::path::Path::new(&effective_workspace));
    if let Err(e) = store.record_spawn_facts(&id, &task_brief, branch.as_deref()) {
        tracing::warn!("record spawn facts for {id}: {e}");
    }

    let cmd_base = registry
        .worker_cmd(&agent, permission_mode, &effective_prompt, &claude_session_id)
        .ok_or_else(|| anyhow::anyhow!("harness '{}' has no worker mode", agent.harness))?;
    let cmd = ninox_core::hooks::with_bin_dir_on_path(&cmd_base);
    if let Some(warning) = registry.cross_session_warning(&agent) {
        eprintln!("warning: {warning}");
    }

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
    let ninox_brain_env = ninox_brain;
    let ninox_config_env = ninox_config;

    let mut env_vec = worker_env_vars(
        &id,
        &sessions_dir_str,
        &orch_id_env,
        ninox_brain_env.as_deref(),
        brain_reads_env.as_deref(),
        ninox_config_env.as_deref(),
    );
    // Opt-in per-repo shared build cache (e.g. a shared sccache wrapper) —
    // see `ninox_core::config::WorkspacesConfig`.
    let shared_build_env = config.workspaces.effective_shared_build_env_for(std::path::Path::new(&effective_workspace));
    env_vec.extend(shared_build_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    // The session was already upserted as Working above; if tmux refuses
    // the spawn (e.g. the workspace dir doesn't exist — create_session
    // rejects that rather than letting the pane silently start in $HOME),
    // mark it Terminated before bailing. A pid-less Working ghost is
    // invisible to poll_pids and would linger until the next app restart's
    // reconciliation.
    if let Err(e) = ninox_core::runtime::create_session(config.runtime.backend, &id, &effective_workspace, &cmd, &env_vec).await {
        if let Ok(Some(mut s)) = store.get_session(&id) {
            s.status = SessionStatus::Terminated;
            let _ = store.upsert_session(&s);
        }
        return Err(e);
    }
    ninox_core::lifecycle::pane_pid::record_pane_pid(store, &id).await;

    Ok(())
}

/// The context footer appended to every worker's task prompt: its own
/// session id, its orchestrator's id, the channels back to the orchestrator,
/// and the one-worker-one-PR scope rule. Always ends with the `ninox
/// capabilities` bootstrap line so a worker can discover the rest of what
/// ninox offers it without that list being restated here. When
/// `pr_watch_enabled` (mirrors `AppConfig.pr_watch.enabled`), also tells the
/// worker to register PR watches instead of polling `gh` directly.
fn worker_context_footer(id: &str, orch_id: &str, pr_watch_enabled: bool) -> String {
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
         Text you write in your own pane never reaches the orchestrator: \
         every read-back, question and report must go through `ninox send`. \
         It prints a receipt; `queued, target unreachable` (exit 75) means \
         Ninox will retry, so do not resend.\n\
         Report back with `ninox send` when (a) you are blocked and need \
         a decision, or (b) the PR is open. You finish yourself with \
         `ninox complete`, as described in the Definition of done.",
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

/// The full prompt sent to a spawned worker's harness: the task prompt (or
/// worker-type-composed prompt), plus the orchestrator-context footer (only
/// when this worker has an orchestrator), plus the Definition-of-done block
/// (always — even a standalone worker needs to know how to finish itself).
fn compose_effective_prompt(
    prompt: String,
    id: &str,
    orch_id_env: &str,
    pr_watch_enabled: bool,
    criteria: &[ninox_core::worker_type::SuccessCriterion],
) -> String {
    let mut effective_prompt = prompt;
    if !orch_id_env.is_empty() {
        effective_prompt.push_str(&worker_context_footer(id, orch_id_env, pr_watch_enabled));
    }
    effective_prompt.push_str(&ninox_core::lifecycle::reactions::format_definition_of_done(criteria));
    effective_prompt
}

#[cfg(test)]
mod plan_generate_tests {
    use super::plan_error_with_details;
    use ninox_core::lifecycle::auto_plan::{AutoPlanError, PlanValidationError};

    #[test]
    fn auto_plan_error_maps_to_envelope_error_json() {
        let value = plan_error_with_details(&AutoPlanError::UnsupportedHarness("codex".into()));
        assert_eq!(value["code"], "unsupported_harness");
        assert!(value["message"].as_str().unwrap().contains("codex"));
        assert!(value.get("details").is_none());
    }

    #[test]
    fn validation_failed_maps_details_array() {
        let err = AutoPlanError::ValidationFailed(vec![PlanValidationError::DuplicateSectionId { id: "a".into() }]);
        let value = plan_error_with_details(&err);
        assert_eq!(value["code"], "validation_failed");
        assert_eq!(value["details"][0]["code"], "duplicate_section_id");
        assert!(value["details"][0]["message"].as_str().unwrap().contains("'a'"));
    }
}

#[cfg(test)]
mod spawn_warning_tests {
    use super::*;

    #[test]
    fn initial_prompt_problems_become_spawn_warnings() {
        let mut warnings = Vec::new();
        note_initial_prompt(Ok(None), &mut warnings);
        assert!(warnings.is_empty());
        note_initial_prompt(Ok(Some("w1 is still starting up — sent the prompt anyway".into())), &mut warnings);
        note_initial_prompt(Err("could not deliver the initial prompt to w1: gone".into()), &mut warnings);
        assert_eq!(warnings, ["w1 is still starting up — sent the prompt anyway", "could not deliver the initial prompt to w1: gone"]);
    }
}

#[cfg(test)]
mod compose_effective_prompt_tests {
    use super::compose_effective_prompt;
    use ninox_core::worker_type::SuccessCriterion;

    #[test]
    fn includes_the_definition_of_done_block() {
        let prompt = compose_effective_prompt(
            "do the task".into(), "w1", "", false, &[SuccessCriterion::PrMerged],
        );
        assert!(prompt.contains("Definition of done"));
        assert!(prompt.to_lowercase().contains("merged"));
        assert!(prompt.contains("ninox complete"));
    }

    #[test]
    fn includes_the_definition_of_done_block_even_without_an_orchestrator() {
        let prompt = compose_effective_prompt("do the task".into(), "w1", "", false, &[]);
        assert!(prompt.contains("Definition of done"));
    }

    #[test]
    fn appends_the_orchestrator_footer_only_when_present() {
        let without = compose_effective_prompt("do the task".into(), "w1", "", false, &[]);
        assert!(!without.contains("Ninox session"));

        let with = compose_effective_prompt("do the task".into(), "w1", "orch1", false, &[]);
        assert!(with.contains("Ninox session `w1` · orchestrator `orch1`"));
    }
}

/// Resolves a spawn's effective success criteria: the named worker type's
/// defaults, optionally overridden/extended by `ninox spawn --success`/
/// `--success-only`. `success_only` replaces the type defaults outright;
/// otherwise each `--success` text is appended as a `Check` item.
pub(crate) fn resolve_success_criteria(
    type_defaults: &[ninox_core::worker_type::SuccessCriterion],
    success_args:  &[String],
    success_only:  bool,
) -> Vec<ninox_core::worker_type::SuccessCriterion> {
    use ninox_core::worker_type::SuccessCriterion;
    let checks = success_args.iter().cloned().map(SuccessCriterion::Check);
    if success_only {
        checks.collect()
    } else {
        type_defaults.iter().cloned().chain(checks).collect()
    }
}

#[cfg(test)]
mod resolve_success_criteria_tests {
    use super::resolve_success_criteria;
    use ninox_core::worker_type::SuccessCriterion;

    #[test]
    fn appends_checks_to_type_defaults() {
        let defaults = vec![SuccessCriterion::PrMerged];
        let args = vec!["smoke test passes".to_string()];
        assert_eq!(
            resolve_success_criteria(&defaults, &args, false),
            vec![SuccessCriterion::PrMerged, SuccessCriterion::Check("smoke test passes".into())],
        );
    }

    #[test]
    fn success_only_replaces_defaults() {
        let defaults = vec![SuccessCriterion::PrMerged];
        let args = vec!["x".to_string(), "y".to_string()];
        assert_eq!(
            resolve_success_criteria(&defaults, &args, true),
            vec![SuccessCriterion::Check("x".into()), SuccessCriterion::Check("y".into())],
        );
    }

    #[test]
    fn with_no_args_keeps_type_defaults() {
        let defaults = vec![SuccessCriterion::PrMerged];
        assert_eq!(resolve_success_criteria(&defaults, &[], false), defaults);
    }
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
    ninox_brain_reads: Option<&'a str>,
    ninox_config: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut env_vec: Vec<(&str, &str)> = vec![
        ("NINOX_SESSION",  id),
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
    if let Some(v) = ninox_brain_reads {
        env_vec.push((ninox_core::brain_set::READS_ENV, v));
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

/// `ninox reap` — clean up the calling orchestrator's workers: kill each
/// one's session and reclaim its worktree and hook artifacts. The store
/// record survives in a terminal state so the fleet board keeps the card for
/// the retention window and `sweep_retired_sessions` stays the only thing
/// that deletes a session row (see `Engine::reap_workers`).
/// Drops `orch_id`'s waiting queue entries named in `ids` (or all of them
/// with `all`). Only that orchestrator's own entries: the same ownership
/// rule `reap_workers` applies to live ones.
fn cancel_queued_workers(store: &Store, orch_id: &str, ids: &[String], all: bool) -> anyhow::Result<Vec<String>> {
    let mut cancelled = Vec::new();
    for e in store.list_queue()? {
        let owned = e.session.orchestrator_id.as_deref() == Some(orch_id);
        let named = ids.contains(&e.id);
        if owned && (named || all) && store.cancel_queued(&e.id)? {
            cancelled.push(e.id);
        }
    }
    Ok(cancelled)
}

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

    // Queued sessions have nothing to kill: reaping one just drops it from
    // the queue (with any messages held for it).
    let cancelled = cancel_queued_workers(&store, &orch_id, &session_ids, all)?;
    for id in &cancelled {
        println!("{id}: removed from the queue (it never started)");
    }
    let session_ids: Vec<String> = session_ids.into_iter().filter(|id| !cancelled.contains(id)).collect();
    if !cancelled.is_empty() && session_ids.is_empty() && !all {
        return Ok(());
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

/// `ninox workspaces` — list, prune, or inspect leftover agent worktrees.
/// Read-only (`List`, `Show`) except `Prune` without `--dry-run`, which
/// still only ever acts through `workspace::sweep_repo_orphans_with_policy`
/// — the same safety-checked path the poller's own periodic sweep uses.
async fn run_workspaces(store: &Store, action: WorkspacesAction) -> anyhow::Result<()> {
    let retention_ms = AppConfig::load_or_default().session_retention.retention_millis();

    match action {
        WorkspacesAction::List { json, repo } => {
            let roots = workspaces_repo_roots(store, repo)?;
            let mut rows = Vec::new();
            for root in &roots {
                rows.extend(workspace::build_workspace_rows(store, root, retention_ms).await?);
            }
            println!("{}", render_workspaces_list(&rows, json));
            Ok(())
        }
        WorkspacesAction::Prune { dry_run, include_unmerged, yes, repo } => {
            run_workspaces_prune(store, retention_ms, dry_run, include_unmerged, yes, repo).await
        }
        WorkspacesAction::Show { target } => {
            let roots = workspaces_repo_roots(store, None)?;
            let mut rows = Vec::new();
            for root in &roots {
                rows.extend(workspace::build_workspace_rows(store, root, retention_ms).await?);
            }
            let row = rows.iter().find(|r| workspace_row_matches(r, &target));
            match row {
                Some(row) => {
                    println!("{}", render_workspace_row_detail(row));
                    Ok(())
                }
                None => anyhow::bail!("no worktree matching {target:?} — check `ninox workspaces list`"),
            }
        }
    }
}

fn workspaces_repo_roots(store: &Store, repo: Option<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    match repo {
        Some(r) => Ok(vec![r]),
        None => workspace::discover_repo_roots(store),
    }
}

fn workspace_row_matches(row: &WorkspaceRow, target: &str) -> bool {
    row.path.to_string_lossy() == target
        || row.path.file_name().is_some_and(|n| n.to_string_lossy() == target)
        || row.session_id.as_deref() == Some(target)
}

async fn run_workspaces_prune(
    store: &Store,
    retention_ms: i64,
    dry_run: bool,
    include_unmerged: bool,
    yes: bool,
    repo: Option<PathBuf>,
) -> anyhow::Result<()> {
    let roots = workspaces_repo_roots(store, repo)?;
    let mut candidates: Vec<WorkspaceRow> = Vec::new();
    for root in &roots {
        let rows = workspace::build_workspace_rows(store, root, retention_ms).await?;
        candidates.extend(rows.into_iter().filter(|r| {
            workspace::is_default_prune_candidate(r.action)
                || (include_unmerged && workspace::is_unmerged_prune_candidate(r.action))
        }));
    }

    if candidates.is_empty() {
        println!("nothing to prune — every leftover worktree is either live, dirty, unpushed, or already clean");
        return Ok(());
    }

    let total_bytes: u64 = candidates.iter().map(|r| r.size_bytes).sum();
    println!("would prune {} worktree(s), reclaiming {}:", candidates.len(), human_size(total_bytes));
    for row in &candidates {
        println!("  {}  {}", row.path.display(), format_workspace_action(row.action));
    }

    if dry_run {
        return Ok(());
    }

    if !yes {
        print!("\nproceed? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("aborted — nothing pruned");
            return Ok(());
        }
    }

    for root in &roots {
        let report = workspace::sweep_repo_orphans_with_policy(store, root, retention_ms, include_unmerged).await?;
        for action in &report.actions {
            if let Some(outcome) = &action.outcome {
                println!("{}: {}", action.path.display(), outcome.describe());
            }
        }
    }
    Ok(())
}

fn render_workspaces_list(rows: &[WorkspaceRow], json: bool) -> String {
    if json {
        let items: Vec<_> = rows.iter().map(workspace_row_to_json).collect();
        return serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string());
    }
    if rows.is_empty() {
        return "no worktrees found".to_string();
    }
    let header = format!(
        "{:<50}{:<14}{:<18}{:>8}  {:<6}{:<10}{:<16}{}",
        "PATH", "SESSION", "STATUS", "SIZE", "DIRTY", "UNPUSHED", "UPSTREAM", "ACTION",
    );
    std::iter::once(header)
        .chain(rows.iter().map(|row| {
            format!(
                "{:<50}{:<14}{:<18}{:>8}  {:<6}{:<10}{:<16}{}",
                truncate(&row.path.display().to_string(), 48),
                row.session_id.as_deref().unwrap_or("—"),
                status_str(row.status),
                human_size(row.size_bytes),
                if row.facts.dirty { "yes" } else { "no" },
                row.facts.unpushed_commit_count,
                upstream_state_str(&row.facts),
                format_workspace_action(row.action),
            )
        }))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_workspace_row_detail(row: &WorkspaceRow) -> String {
    format!(
        "path:       {}\nsession:    {}\nstatus:     {}\nsize:       {}\ndirty:      {}\nunpushed:   {}\nupstream:   {}\naction:     {}",
        row.path.display(),
        row.session_id.as_deref().unwrap_or("—"),
        status_str(row.status),
        human_size(row.size_bytes),
        row.facts.dirty,
        row.facts.unpushed_commit_count,
        upstream_state_str(&row.facts),
        format_workspace_action(row.action),
    )
}

fn workspace_row_to_json(row: &WorkspaceRow) -> serde_json::Value {
    serde_json::json!({
        "path": row.path.display().to_string(),
        "session_id": row.session_id,
        "status": status_str(row.status),
        "size_bytes": row.size_bytes,
        "dirty": row.facts.dirty,
        "unpushed_commit_count": row.facts.unpushed_commit_count,
        "has_upstream": row.facts.has_upstream,
        "merged": row.facts.merged,
        "action": format_workspace_action(row.action),
    })
}

fn status_str(status: OrphanStatus) -> &'static str {
    match status {
        OrphanStatus::Live => "live",
        OrphanStatus::NotOrphanYet => "not-orphan-yet",
        OrphanStatus::NotOurs => "not-ours",
        OrphanStatus::Orphan(Ownership::Tracked) => "orphan(tracked)",
        OrphanStatus::Orphan(Ownership::LegacyMatch) => "orphan(legacy)",
        OrphanStatus::Orphan(Ownership::NotOurs) => "orphan(not-ours)",
    }
}

fn upstream_state_str(facts: &WorkspaceFacts) -> String {
    let push = if facts.has_upstream { "pushed" } else { "no-upstream" };
    if facts.merged {
        format!("{push}/merged")
    } else {
        push.to_string()
    }
}

fn format_workspace_action(action: WorkspaceAction) -> String {
    match action {
        WorkspaceAction::None => "none".to_string(),
        WorkspaceAction::WouldClean { unmerged_with_content: true, .. } => {
            "would clean worktree (unmerged — needs --include-unmerged)".to_string()
        }
        WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false } => {
            "would clean worktree + branch".to_string()
        }
        WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: false } => {
            "would clean worktree".to_string()
        }
        WorkspaceAction::NeedsAttention(reason) => format!("needs attention: {}", skip_reason_str(reason)),
    }
}

fn skip_reason_str(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::LiveSessionRow => "live session",
        SkipReason::Interrupted => "interrupted",
        SkipReason::Dirty => "dirty",
        SkipReason::UnpushedCommits => "unpushed commits",
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("…{}", &s[s.len().saturating_sub(max - 1)..])
    }
}

#[cfg(test)]
mod workspaces_cli_tests {
    use super::{run_workspaces, run_workspaces_prune, workspace, Arc, PathBuf, Store, WorkspacesAction};
    use ninox_core::types::{Session, SessionStatus};
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    const RETENTION_MS: i64 = 2 * 24 * 60 * 60 * 1000;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .status()
            .unwrap()
            .success()
    }

    fn init_repo() -> tempfile::TempDir {
        let repo = tempdir().unwrap();
        run_git(repo.path(), &["init", "-q", "-b", "main"]);
        run_git(repo.path(), &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "init"]);
        repo
    }

    fn store() -> Arc<Store> {
        Arc::new(Store::open(tempdir().unwrap().keep().join("t.db")).unwrap())
    }

    fn terminal_session(id: &str, workspace_path: &str, merged_at: Option<i64>) -> Session {
        let mut s = super::test_fixtures::session(id, None, SessionStatus::Terminated);
        s.workspace_path = Some(workspace_path.into());
        s.terminal_at = Some(0);
        s.merged_at = merged_at;
        s
    }

    /// A worktree whose session row is terminal and well past the retention
    /// window, with content already merged — the plain "merged orphan"
    /// default-prune candidate.
    fn merged_orphan(repo: &Path) -> PathBuf {
        let wt = repo.join(".claude/worktrees/merged-orphan");
        run_git(repo, &["worktree", "add", "-q", "-b", "merged-orphan", wt.to_str().unwrap()]);
        wt
    }

    /// A worktree whose branch is clean and fully pushed but has commits
    /// beyond base and no merge — `WouldClean{unmerged_with_content: true}`.
    fn unmerged_pushed_orphan(repo: &Path) -> PathBuf {
        let wt = repo.join(".claude/worktrees/unmerged-orphan");
        run_git(repo, &["worktree", "add", "-q", "-b", "unmerged-orphan", wt.to_str().unwrap()]);
        run_git(&wt, &["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "--allow-empty", "-q", "-m", "real work"]);
        run_git(repo, &["push", "-q", "-u", "origin", "unmerged-orphan"]);
        wt
    }

    fn dirty_orphan(repo: &Path) -> PathBuf {
        let wt = repo.join(".claude/worktrees/dirty-orphan");
        run_git(repo, &["worktree", "add", "-q", "-b", "dirty-orphan", wt.to_str().unwrap()]);
        std::fs::write(wt.join("scratch.txt"), "wip").unwrap();
        wt
    }

    fn with_origin(repo: &Path) {
        // `.keep()` so the bare remote survives past this function's return
        // — a plain `TempDir` would delete it on drop right here, and every
        // later push in the calling test would fail against a vanished path.
        let remote = tempdir().unwrap().keep();
        run_git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        run_git(repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
        run_git(repo, &["push", "-q", "origin", "main"]);
        run_git(repo, &["remote", "set-head", "origin", "-a"]);
    }

    #[tokio::test]
    async fn list_renders_path_status_and_action_for_each_row() {
        let repo = init_repo();
        let wt = merged_orphan(repo.path());
        let store = store();
        store.upsert_session(&terminal_session("merged-orphan", wt.to_str().unwrap(), Some(1))).unwrap();

        let rows = workspace::build_workspace_rows(&store, repo.path(), RETENTION_MS).await.unwrap();
        let rendered = super::render_workspaces_list(&rows, false);

        assert!(rendered.contains("merged-orphan"));
        assert!(rendered.contains("orphan(tracked)"));
        assert!(rendered.contains("would clean worktree + branch"));

        let json = super::render_workspaces_list(&rows, true);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn prune_dry_run_never_removes_anything() {
        let repo = init_repo();
        let wt = merged_orphan(repo.path());
        let store = store();
        store.upsert_session(&terminal_session("merged-orphan", wt.to_str().unwrap(), Some(1))).unwrap();

        run_workspaces_prune(&store, RETENTION_MS, true, false, true, Some(repo.path().to_path_buf())).await.unwrap();

        assert!(wt.exists(), "dry-run must never call cleanup()");
    }

    #[tokio::test]
    async fn prune_default_cleans_merged_but_leaves_unmerged_content_alone() {
        let _iso = ninox_core::test_isolation::isolate();
        let repo = init_repo();
        with_origin(repo.path());
        let merged_wt = merged_orphan(repo.path());
        let unmerged_wt = unmerged_pushed_orphan(repo.path());
        let store = store();
        store.upsert_session(&terminal_session("merged-orphan", merged_wt.to_str().unwrap(), Some(1))).unwrap();
        store.upsert_session(&terminal_session("unmerged-orphan", unmerged_wt.to_str().unwrap(), None)).unwrap();

        run_workspaces_prune(&store, RETENTION_MS, false, false, true, Some(repo.path().to_path_buf())).await.unwrap();

        assert!(!merged_wt.exists(), "merged orphan is pruned by default");
        assert!(unmerged_wt.exists(), "unmerged content needs --include-unmerged");
    }

    #[tokio::test]
    async fn prune_include_unmerged_also_cleans_the_unmerged_bucket_but_keeps_its_branch() {
        let _iso = ninox_core::test_isolation::isolate();
        let repo = init_repo();
        with_origin(repo.path());
        let unmerged_wt = unmerged_pushed_orphan(repo.path());
        let store = store();
        store.upsert_session(&terminal_session("unmerged-orphan", unmerged_wt.to_str().unwrap(), None)).unwrap();

        run_workspaces_prune(&store, RETENTION_MS, false, true, true, Some(repo.path().to_path_buf())).await.unwrap();

        assert!(!unmerged_wt.exists(), "worktree is removed once opted in");
        assert!(branch_exists(repo.path(), "unmerged-orphan"), "branch survives — never merged");
    }

    #[tokio::test]
    async fn prune_never_touches_dirty_or_unpushed_regardless_of_flags() {
        let repo = init_repo();
        with_origin(repo.path());
        let dirty_wt = dirty_orphan(repo.path());
        let store = store();
        store.upsert_session(&terminal_session("dirty-orphan", dirty_wt.to_str().unwrap(), None)).unwrap();

        run_workspaces_prune(&store, RETENTION_MS, false, true, true, Some(repo.path().to_path_buf())).await.unwrap();

        assert!(dirty_wt.exists(), "dirty worktrees are never pruned by this command, any flags");
    }

    #[tokio::test]
    async fn show_resolves_by_path_and_by_session_id_and_errors_on_no_match() {
        let _iso = ninox_core::test_isolation::isolate();
        let repo = init_repo();
        let wt = merged_orphan(repo.path());
        let store = store();
        store.upsert_session(&terminal_session("merged-orphan", wt.to_str().unwrap(), Some(1))).unwrap();

        run_workspaces(&store, WorkspacesAction::Show { target: wt.to_string_lossy().to_string() }).await.unwrap();
        run_workspaces(&store, WorkspacesAction::Show { target: "merged-orphan".to_string() }).await.unwrap();
        let err = run_workspaces(&store, WorkspacesAction::Show { target: "no-such-thing".to_string() }).await.unwrap_err();
        assert!(err.to_string().contains("no worktree matching"));
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
#[allow(clippy::too_many_arguments)]
async fn run_spawn_orchestrator(
    store:          Arc<Store>,
    config:         AppConfig,
    name:           String,
    prompt:         Option<String>,
    user_requested: bool,
    no_queue:       bool,
    brain:          ninox_core::brain_set::BrainSet,
    model:          Option<String>,
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
    let agent = model.as_deref().map(|m| resolve_orchestrator_agent(&config, Some(m)));
    let spawned = spawn_orchestrator_common_opts(&store, &config, &name, prompt, None, agent, Some(brain), no_queue).await?;
    print_spawn_warnings(&spawned);
    if let Some(queued) = spawned.queued {
        println!("{queued}");
        return Ok(());
    }
    println!("spawned orchestrator {}", spawned.id);
    if !want_brief {
        println!("send it a brief with: ninox send {} \"<your message>\"", spawned.id);
    }
    Ok(())
}

/// What a successful `spawn_orchestrator_common`/`spawn_standalone_worker`
/// call produced — just enough for a caller to notify and select the new
/// row (the session itself is already upserted).
#[derive(Debug)]
pub struct SpawnedSession {
    pub id:        String,
    /// Empty when queued.
    pub workspace: String,
    /// `Some(message)` when the spawn was queued under the session limit
    /// rather than started.
    pub queued:    Option<String>,
    /// Non-fatal problems (the first message sent before the agent was
    /// ready, or not delivered at all) for the caller to show: the CLI on
    /// stderr, the TUI as notifications, since stderr would draw over its
    /// screen.
    pub warnings:  Vec<String>,
}

/// `ninox spawn-sub-orchestrator`: real nested delegation, distinct from
/// `run_spawn_orchestrator`'s peer semantics. Verifies the caller (either an
/// already-verified orchestrator, or a worker whose `worker_type` grants
/// `allow_nested_spawn`), enforces the configured depth cap, and only then
/// spawns — mirroring `run_spawn`'s own pre-side-effect verification
/// ordering.
#[allow(clippy::too_many_arguments)]
async fn run_spawn_sub_orchestrator(
    store:             Arc<Store>,
    config:            AppConfig,
    name:              String,
    prompt:            Option<String>,
    // The claimed parent orchestrator (explicit `--orchestrator-id`, or the
    // `NINOX_ORCHESTRATOR_ID` fallback) — only meaningful for an
    // orchestrator caller; a worker caller's parent is instead read
    // authoritatively from its own recorded session row.
    orchestrator_id:   Option<String>,
    caller_session_id: Option<String>,
    caller:            CallerRuntime,
    no_queue:          bool,
    brain:             ninox_core::brain_set::BrainSet,
    model:             Option<String>,
) -> anyhow::Result<()> {
    let registry = config.worker_type_registry();
    let parent_id = ninox_core::orchestrator_auth::authorize_nested_spawn(
        &store,
        orchestrator_id.as_deref(),
        caller.caller_type.as_deref(),
        caller_session_id.as_deref(),
        caller.pane.as_ref(),
        &registry,
    )
    .map_err(|e| match (caller.caller_type.as_deref(), orchestrator_id.as_deref()) {
        // Only claim an identity-verification failure when the orchestrator
        // path actually ran — a worker caller also carries `orchestrator_id`
        // (its own parent, via `NINOX_ORCHESTRATOR_ID`), but its rejection
        // is a permission failure, not a forged/unowned orchestrator claim.
        (Some("orchestrator"), Some(claimed)) => anyhow::anyhow!(
            "refusing to spawn a sub-orchestrator under claimed orchestrator '{claimed}': \
             caller could not be verified as that orchestrator ({e})"
        ),
        _ => anyhow::anyhow!("refusing to spawn a sub-orchestrator: caller could not be verified ({e})"),
    })?;

    let cap = config.nested_orchestration.max_depth;
    let depth = ninox_core::orchestrator_auth::orchestrator_depth(&store, &parent_id, cap)?;
    let new_depth = depth + 1;
    anyhow::ensure!(
        new_depth < cap,
        "spawning a sub-orchestrator under '{parent_id}' would reach depth {new_depth}, at or \
         beyond the configured depth cap ({cap}) — spawn a plain worker instead and have it \
         report back up the tree"
    );

    let spawned = spawn_orchestrator_common_opts(
        &store, &config, &name, prompt, Some(&parent_id),
        model.as_deref().map(|m| resolve_orchestrator_agent(&config, Some(m))), Some(brain), no_queue,
    ).await?;
    print_spawn_warnings(&spawned);
    if let Some(queued) = spawned.queued {
        println!("{queued}");
        return Ok(());
    }
    println!("spawned sub-orchestrator {} under {}", spawned.id, parent_id);
    Ok(())
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
        match ninox_core::daemon::ensure_daemon(port, &exe, &db_path).await {
            ninox_core::daemon::DaemonStatus::Failed(e) => {
                eprintln!("warning: background services not running ({e}) — statuses may go stale");
            }
            ninox_core::daemon::DaemonStatus::AlreadyRunning => {
                if let Some(warning) = crate::server_restart::check_daemon_build(port).await {
                    eprintln!("warning: {warning}");
                }
            }
            _ => {}
        }
    }
    let brain = BrainArgs::default().resolve(&store, verified_caller_session_id(&store).await.as_deref(), &config)?;
    let spawned = spawn_orchestrator_common(&store, &config, &name, prompt, None, None, Some(brain)).await?;
    print_spawn_warnings(&spawned);
    if let Some(queued) = spawned.queued {
        println!("{queued}");
        println!("connect once it starts with: ninox connect {}", spawned.id);
        return Ok(());
    }
    println!("spawned orchestrator {}", spawned.id);
    if no_attach {
        println!("dir: {}", spawned.workspace);
        println!("connect with: ninox connect {}", spawned.id);
        return Ok(());
    }
    connect::exec_attach(ninox_core::runtime::attach_args(&spawned.id).await)
}

#[allow(clippy::too_many_arguments)]
pub async fn spawn_orchestrator_common(
    store:  &Store,
    config: &AppConfig,
    name:   &str,
    prompt: Option<String>,
    parent_orchestrator_id: Option<&str>,
    agent_override: Option<ninox_core::config::AgentConfig>,
    brain_override: Option<ninox_core::brain_set::BrainSet>,
) -> anyhow::Result<SpawnedSession> {
    spawn_orchestrator_common_opts(
        store, config, name, prompt, parent_orchestrator_id, agent_override,
        brain_override, false,
    )
    .await
}

/// An orchestrator's agent under the one model rule: `explicit_model`,
/// else the Orchestrator type's model, else `[orchestrator].model`, else
/// the harness default.
pub fn resolve_orchestrator_agent(config: &AppConfig, explicit_model: Option<&str>) -> ninox_core::config::AgentConfig {
    let registry = config.worker_type_registry();
    ninox_core::model_resolution::effective_agent(
        &ninox_core::model_resolution::SpawnModelRequest::orchestrator(registry.get("Orchestrator")).with_model(explicit_model),
        config,
    ).agent
}

/// [`spawn_orchestrator_common`] with `--no-queue`.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_orchestrator_common_opts(
    store:  &Store,
    config: &AppConfig,
    name:   &str,
    prompt: Option<String>,
    // The new orchestrator's lineage parent, for the nested-spawn path
    // (`run_spawn_sub_orchestrator`) — `None` for the existing peer-spawn
    // paths (`run_spawn_orchestrator`, `run_orchestrate`), which keep
    // `orchestrator_id: None` unchanged.
    parent_orchestrator_id: Option<&str>,
    // The TUI/GUI spawn dialogs let a human pick an agent/catalogue per
    // spawn (MLOPS-5350 item 1); every CLI call site keeps today's exact
    // behavior by passing `None` here.
    agent_override: Option<ninox_core::config::AgentConfig>,
    brain_override: Option<ninox_core::brain_set::BrainSet>,
    no_queue: bool,
) -> anyhow::Result<SpawnedSession> {
    let id = slugify(name);
    if id.is_empty() {
        anyhow::bail!("--name must contain at least one alphanumeric character");
    }
    // See `messaging::is_reserved_session_id` for what each reserved id guards.
    if ninox_core::messaging::is_reserved_session_id(&id) {
        anyhow::bail!("'{id}' is reserved for ninox itself — pick another name");
    }
    // Same hazard the app's modal guards: a duplicate id would upsert over an
    // existing record, then fail the tmux create and mark the hijacked
    // session Terminated.
    if store.get_session(&id)?.is_some()
        || store.list_orchestrators()?.iter().any(|o| o.id == id)
    {
        anyhow::bail!("a session named {id} already exists — pick another name");
    }

    let agent = agent_override.unwrap_or_else(|| resolve_orchestrator_agent(config, None));
    // CLI callers pass the set already narrowed from their own (see
    // `brain_set::resolve_child`); `None` is an out-of-session caller.
    let brain = brain_override.unwrap_or_else(|| ninox_core::brain_set::BrainSet::default_for(config));
    let plan = crate::session_queue::OrchestratorLaunch {
        id: id.clone(),
        name: name.to_string(),
        prompt,
        parent_orchestrator_id: parent_orchestrator_id.map(str::to_string),
        agent,
        catalogue_path: brain.write_str(),
        brain: Some(brain),
        spawner: std::env::var("NINOX_ORCHESTRATOR_ID").ok().filter(|s| !s.is_empty()),
        started_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64,
    };
    // A queued orchestrator's brief is held as a message, attested now
    // while the spawner's own pane can still vouch for it; the payload
    // carries none.
    let payload = crate::session_queue::QueuedLaunch::Orchestrator(crate::session_queue::OrchestratorLaunch {
        prompt: None,
        ..plan.clone()
    });
    match crate::session_queue::admit(store, ninox_core::store::QueueKind::Spawn, &plan.skeleton(), &payload, no_queue)? {
        crate::session_queue::Admit::Queued { message, .. } => {
            if let Some(brief) = &plan.prompt {
                let text = format!("{brief}{}", orchestrator_context_footer(&id, plan.spawner.as_deref()));
                let sender_pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
                ninox_core::messaging::deliver_message(
                    store, &AppConfig::sessions_dir(), &id, &text, config.send_mechanism(),
                    plan.spawner.as_deref(), sender_pane.as_ref(), ninox_core::types::MessageKind::Freeform,
                )
                .await?;
            }
            return Ok(SpawnedSession { id, workspace: String::new(), queued: Some(message), warnings: Vec::new() });
        }
        crate::session_queue::Admit::Admitted => {}
    }
    let result = launch_orchestrator(store, config, plan).await;
    crate::session_queue::release(store, &id);
    result
}

/// The launch half of [`spawn_orchestrator_common_opts`]; also how a queued
/// orchestrator spawn is started later.
pub(crate) async fn launch_orchestrator(
    store:  &Store,
    config: &AppConfig,
    plan:   crate::session_queue::OrchestratorLaunch,
) -> anyhow::Result<SpawnedSession> {
    let skeleton = plan.skeleton();
    let permission_mode = fresh_permission_mode(config, LaunchKind::Interactive, &skeleton);
    let brain = plan.brain_set();
    let crate::session_queue::OrchestratorLaunch {
        id, name, prompt, agent, spawner, started_at: ts, ..
    } = plan;
    let name = name.as_str();
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
    crate::spawn_util::seed_permissions_for_type_name(&ws, &agent.harness, skeleton.worker_type.as_deref(), config)?;
    let ws_str = ws.to_string_lossy().to_string();
    let claude_session_id = ninox_core::harness::new_claude_session_id();

    store.upsert_orchestrator(&ninox_core::types::Orchestrator {
        id:         id.clone(),
        name:       name.to_string(),
        created_at: ts,
    })?;
    let session = Session {
        workspace_path: Some(ws_str.clone()),
        claude_session_id: Some(claude_session_id.clone()),
        ..skeleton
    };
    store.upsert_session(&session)?;
    store.set_permission_mode(&id, permission_mode)?;

    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    let cmd = ninox_core::hooks::with_bin_dir_on_path(
        &config.registry().interactive_cmd(&agent, permission_mode, &claude_session_id),
    );
    let (brain_write, brain_reads) = (brain.write_str(), brain.reads_env_value());
    let env = orchestrator_env_vars(
        &ninox_bin, &config_path, &brain_write, &brain_reads, &id, &sessions_dir_str,
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
    ninox_core::lifecycle::pane_pid::record_pane_pid(store, &id).await;

    let mut warnings: Vec<String> = config.registry().cross_session_warning(&agent).into_iter().collect();
    if let Some(brief) = prompt {
        let message = orchestrator_initial_brief(&id, &brief, spawner.as_deref());
        note_initial_prompt(deliver_initial_prompt(store, config, &id, &message, spawner.as_deref()).await, &mut warnings);
    }

    Ok(SpawnedSession { id, workspace: ws_str, queued: None, warnings })
}

/// The TUI's/GUI's Standalone-kind spawn with a worker type (MLOPS-5350
/// item 1) — a human/TUI-initiated interactive session, not an orchestrator-
/// dispatched one-shot worker. Reuses `run_spawn`'s own worker-type
/// resolution (hidden/`Orchestrator` rejected, unknown name rejected,
/// success-criteria defaults) but skips its `can_spawn`/orchestrator-claim
/// checks entirely — there is no managing orchestrator here, the human
/// typing into the Spawn dialog is the caller — and launches via the
/// interactive command (attachable later with `a`/Connect, same as this
/// module's Orchestrator path) rather than `run_spawn`'s one-shot worker
/// command.
///
/// Deliberately diverges from `run_spawn --type` on one point: the dialog
/// exposes Agent/Model as their own editable fields alongside Worker Type,
/// so the caller's explicit `agent` always wins over the type's own
/// `resolved_agent()` — a worker type here only contributes the
/// `worker_type` stamp and its success-criteria defaults.
#[allow(clippy::too_many_arguments)] // mirrors spawn_orchestrator_common's own field-per-form-field shape
pub async fn spawn_standalone_worker(
    store: &Store,
    config: &AppConfig,
    name: String,
    workspace_input: String,
    worker_type: Option<String>,
    agent: ninox_core::config::AgentConfig,
    brain: ninox_core::brain_set::BrainSet,
    prompt: String,
) -> anyhow::Result<SpawnedSession> {
    spawn_standalone_worker_opts(store, config, name, workspace_input, worker_type, agent, brain, prompt, false).await
}

/// [`spawn_standalone_worker`], optionally cutting the session its own
/// worktree (as the desktop Spawn dialog does) instead of working in the
/// given directory.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_standalone_worker_opts(
    store: &Store,
    config: &AppConfig,
    name: String,
    workspace_input: String,
    worker_type: Option<String>,
    agent: ninox_core::config::AgentConfig,
    brain: ninox_core::brain_set::BrainSet,
    prompt: String,
    isolate_worktree: bool,
) -> anyhow::Result<SpawnedSession> {
    let id = slugify(&name);
    if id.is_empty() {
        anyhow::bail!("--name must contain at least one alphanumeric character");
    }
    if ninox_core::messaging::is_reserved_session_id(&id) {
        anyhow::bail!("'{id}' is reserved for ninox itself — pick another name");
    }
    if store.get_session(&id)?.is_some() {
        anyhow::bail!("a session named {id} already exists — pick another name");
    }

    let type_registry = config.worker_type_registry();
    let requested_type_name = worker_type.unwrap_or_else(|| "Implementor".to_string());
    let requested_spec = resolve_requested_worker_type(&type_registry, &requested_type_name).map_err(|e| match e {
        InvalidWorkerType::Unknown => anyhow::anyhow!(
            "unknown worker type '{requested_type_name}' — valid types: {}",
            type_registry.user_facing_names().join(", "),
        ),
        InvalidWorkerType::Hidden | InvalidWorkerType::IsOrchestrator => anyhow::anyhow!(
            "worker type '{requested_type_name}' can't be used for a Standalone spawn — pick another type",
        ),
    })?;
    let task_brief = prompt.clone();
    let (resolved_criteria, prompt) = standalone_prompt_for(&requested_spec, config, &id, prompt);

    let plan = crate::session_queue::StandaloneLaunch {
        id: id.clone(),
        name,
        workspace_input,
        worker_type: requested_spec.name.clone(),
        agent,
        catalogue_path: brain.write_str(),
        brain: Some(brain),
        prompt,
        task_brief,
        success_criteria: resolved_criteria,
        started_at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64,
        isolate_worktree,
    };
    let payload = crate::session_queue::QueuedLaunch::Standalone(plan.clone());
    match crate::session_queue::admit(store, ninox_core::store::QueueKind::Spawn, &plan.skeleton(), &payload, false)? {
        crate::session_queue::Admit::Queued { message, .. } => {
            return Ok(SpawnedSession { id, workspace: String::new(), queued: Some(message), warnings: Vec::new() });
        }
        crate::session_queue::Admit::Admitted => {}
    }
    let result = launch_standalone_worker(store, config, plan).await;
    crate::session_queue::release(store, &id);
    result
}

/// The launch half of [`spawn_standalone_worker`]; also how a queued
/// standalone spawn is started later.
pub(crate) async fn launch_standalone_worker(
    store: &Store,
    config: &AppConfig,
    plan: crate::session_queue::StandaloneLaunch,
) -> anyhow::Result<SpawnedSession> {
    let skeleton = plan.skeleton();
    let permission_mode = fresh_permission_mode(config, LaunchKind::Interactive, &skeleton);
    let brain = plan.brain_set();
    let crate::session_queue::StandaloneLaunch {
        id, workspace_input, agent, prompt, task_brief, isolate_worktree, ..
    } = plan;

    // Every validation above runs before this, the first side effect —
    // mirrors run_spawn's and spawn_orchestrator_common's own ordering.
    // On a blocking thread: resolve_standalone_workspace does filesystem
    // checks and (on a missing path) a `git worktree add` subprocess, and
    // repo_from_workspace shells out to `git remote get-url` — both
    // synchronous, same reasoning as create_worker_worktree's own
    // spawn_blocking a few functions up.
    let inbox_enabled = config.send_mechanism() == SendMechanism::Inbox;
    let (workspace_input_bg, config_bg) = (workspace_input.clone(), config.clone());
    let (workspace, repo) = tokio::task::spawn_blocking(move || {
        let workspace = resolve_standalone_workspace(&workspace_input_bg, inbox_enabled, &config_bg)?;
        let repo = repo_from_workspace(&workspace).unwrap_or_default();
        Ok::<_, String>((workspace, repo))
    })
    .await
    .map_err(|e| anyhow::anyhow!("workspace resolution task panicked: {e}"))?
    .map_err(|e| anyhow::anyhow!(e))?;
    // The desktop Spawn dialog cuts each standalone session its own
    // worktree; a queued one from there must get the same when it starts.
    let mut warnings = Vec::new();
    let workspace = if isolate_worktree {
        match create_worker_worktree(&workspace, &id, inbox_enabled, config).await {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!("worktree creation failed for {id}, using shared workspace: {e}");
                warnings.push(format!("couldn't create a worktree for {id} ({e}); it is working in {workspace} directly"));
                workspace
            }
        }
    } else {
        workspace
    };
    seed_worker_skills(&workspace, config, &agent.harness).await;
    crate::spawn_util::seed_permissions_for_type_name(
        std::path::Path::new(&workspace), &agent.harness, skeleton.worker_type.as_deref(), config,
    )?;

    let claude_session_id = ninox_core::harness::new_claude_session_id();

    let session = Session {
        repo,
        workspace_path: Some(workspace.clone()),
        claude_session_id: Some(claude_session_id.clone()),
        ..skeleton
    };
    store.upsert_session(&session)?;
    store.set_permission_mode(&id, permission_mode)?;
    // The task brief and branch are what a fleet restore re-briefs this
    // session with if its conversation can't be resumed — same as
    // run_spawn's own call.
    let branch = ninox_core::fleet::probe::current_branch(std::path::Path::new(&workspace));
    if let Err(e) = store.record_spawn_facts(&id, &task_brief, branch.as_deref()) {
        tracing::warn!("record spawn facts for {id}: {e}");
    }

    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    std::fs::create_dir_all(&sessions_dir).ok();
    let sessions_dir_str = sessions_dir.to_string_lossy().to_string();

    let ninox_bin = ninox_core::hooks::canonical_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ninox".to_string());
    let config_path = AppConfig::config_path().to_string_lossy().to_string();
    let cmd = ninox_core::hooks::with_bin_dir_on_path(
        &config.registry().interactive_cmd(&agent, permission_mode, &claude_session_id),
    );
    let (brain_write, brain_reads) = (brain.write_str(), brain.reads_env_value());
    let env = interactive_env_vars(&ninox_bin, &config_path, &brain_write, &brain_reads, &id, &sessions_dir_str, &[]);

    if let Err(e) = ninox_core::runtime::create_session(config.runtime.backend, &id, &workspace, &cmd, &env).await {
        // No Orchestrator row to roll back (unlike spawn_orchestrator_common)
        // — just the session, and a Terminated record (not a delete) is
        // worth keeping here since, unlike an orchestrator, a standalone
        // session's retired row is swept by the normal retention path.
        if let Ok(Some(mut s)) = store.get_session(&id) {
            s.status = SessionStatus::Terminated;
            let _ = store.upsert_session(&s);
        }
        return Err(e);
    }

    warnings.extend(config.registry().cross_session_warning(&agent));
    if let Some(prompt) = prompt {
        note_initial_prompt(deliver_initial_prompt(store, config, &id, &prompt, None).await, &mut warnings);
    }

    Ok(SpawnedSession { id, workspace, queued: None, warnings })
}

/// The tmux env for a CLI-spawned orchestrator. Mirrors
/// `spawn_util::interactive_env_vars` plus the two vars that make a session
/// an *orchestrator*: its own id (so workers it spawns report back to it)
/// and the caller type (which gates the subagent blocker and `ninox reap`).
fn orchestrator_env_vars<'a>(
    ninox_bin:      &'a str,
    ninox_config:   &'a str,
    brain_write:    &'a str,
    brain_reads:    &'a str,
    session_id:     &'a str,
    sessions_dir:   &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("NINOX_BIN",             ninox_bin),
        ("NINOX_CONFIG",          ninox_config),
        ("NINOX_BRAIN",           brain_write),
        (ninox_core::brain_set::READS_ENV, brain_reads),
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

/// An orchestrator's initial brief as delivered: the human's text plus the
/// orchestrator context footer.
pub fn orchestrator_initial_brief(id: &str, brief: &str, spawner: Option<&str>) -> String {
    format!("{brief}{}", orchestrator_context_footer(id, spawner))
}

/// A Standalone spawn's success criteria (the worker type's defaults) and
/// the first message to send it, if any: the Definition-of-done block goes
/// out whenever there's a prompt or criteria the session would otherwise
/// never be told about. No orchestrator footer — a human-initiated standalone
/// session reports to no one. `None` for an unknown or non-standalone type.
pub fn standalone_initial_prompt(
    config: &AppConfig,
    id: &str,
    worker_type: &str,
    prompt: String,
) -> Option<(Vec<ninox_core::worker_type::SuccessCriterion>, Option<String>)> {
    let spec = resolve_requested_worker_type(&config.worker_type_registry(), worker_type).ok()?;
    Some(standalone_prompt_for(&spec, config, id, prompt))
}

fn standalone_prompt_for(
    spec: &ninox_core::worker_type::WorkerTypeSpec,
    config: &AppConfig,
    id: &str,
    prompt: String,
) -> (Vec<ninox_core::worker_type::SuccessCriterion>, Option<String>) {
    let criteria = resolve_success_criteria(&spec.success, &[], false);
    let message = (!prompt.trim().is_empty() || !criteria.is_empty())
        .then(|| compose_effective_prompt(prompt, id, "", config.pr_watch.enabled, &criteria));
    (criteria, message)
}

/// Delivers a freshly spawned session's first message once its harness has
/// drawn an input box (typing before that is swallowed). Attributed to
/// `spawner`, or unverified like a bare `ninox send` — never to ninox
/// itself, since the text is the human's, not ninox's. `Ok(Some(warning))`
/// when it had to send before the session looked ready; never printed here,
/// since a TUI caller's stderr is its screen.
pub async fn deliver_initial_prompt(
    store: &Store,
    config: &AppConfig,
    id: &str,
    message: &str,
    spawner: Option<&str>,
) -> Result<Option<String>, String> {
    // An orchestrator's first message is its "brief"; anything else's is its "prompt".
    let what = if store.list_orchestrators().map(|os| os.iter().any(|o| o.id == id)).unwrap_or(false) { "brief" } else { "prompt" };
    let ready = ninox_core::runtime::wait_for_input_prompt(id, std::time::Duration::from_secs(90)).await;
    let warning = (!ready).then(|| format!("{id} is still starting up — sent the {what} anyway"));
    let sessions_dir = ninox_core::config::AppConfig::sessions_dir();
    let sender_pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
    ninox_core::messaging::deliver_message(
        store, &sessions_dir, id, message, config.send_mechanism(),
        spawner, sender_pane.as_ref(), ninox_core::types::MessageKind::Freeform,
    )
    .await
    .map(|receipt| {
        let queued = (!receipt.is_delivered()).then(|| format!("the initial {what} for {id} is {receipt}"));
        match (warning, queued) {
            (Some(w), Some(q)) => Some(format!("{w}; {q}")),
            (w, q) => w.or(q),
        }
    })
    .map_err(|e| format!("could not deliver the initial {what} to {id}: {e}\nretry with: ninox send {id} \"<the {what}>\""))
}

/// Folds a [`deliver_initial_prompt`] outcome into a spawn's warnings, and
/// logs it for callers (the session queue) that discard them.
fn note_initial_prompt(outcome: Result<Option<String>, String>, warnings: &mut Vec<String>) {
    let warning = match outcome {
        Ok(warning) => warning,
        Err(e) => Some(e),
    };
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
        warnings.push(warning);
    }
}

/// The CLI's view of a spawn's [`SpawnedSession::warnings`].
fn print_spawn_warnings(spawned: &SpawnedSession) {
    for warning in &spawned.warnings {
        eprintln!("warning: {warning}");
    }
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
    let sessions_dir = AppConfig::sessions_dir();
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

/// What [`poll_until_resolved`] found at its deadline.
enum PollOutcome {
    Answered(String),
    /// Dismissed by a human (only offered once the asking session looked
    /// terminal) — no answer will ever come, so waiting longer is pointless.
    Cancelled,
    StillPending,
}

const DISMISSED_MESSAGE: &str = "was dismissed (the session it was asked from no longer looked alive) \
     — no answer is coming; re-ask with `ninox user-request ask` if you still need one";

fn resolve_user_request_caller(store: &Store, verb: &str) -> anyhow::Result<String> {
    let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
    let cwd = std::env::current_dir().ok();
    ninox_core::worker_status::resolve_session_id(store, env_session.as_deref(), cwd.as_deref())?
        .ok_or_else(|| anyhow::anyhow!(
            "could not resolve this session's identity — `ninox user-request {verb}` only \
             works inside a Ninox-managed session"
        ))
}

/// Block the calling thread, polling once a second, until `id`'s row is
/// answered or `timeout` elapses. Shared by `ninox user-request ask`
/// (sync mode) and `ninox user-request wait` — the latter is how a worker
/// re-attaches to a request after a restart interrupted an `ask --async`
/// or an earlier `wait`. `Err` only for a genuine failure to read the row
/// (including "no such id"); a timeout is `Ok(StillPending)`, not an
/// error — it's the caller's job to decide that running out the clock
/// means the request itself failed.
///
/// On the success path, stamps `answer_delivered_at` before returning:
/// generalizes that column's meaning from "the async poller delivered it"
/// to "delivered to the requester by any channel", so a process that dies
/// after being answered but before reading it here still shows up in a
/// later fleet-recovery briefing (`Store::unresolved_user_requests_for_briefing`).
/// `mark_user_request_answer_delivered` only stamps a NULL column, so this
/// is a no-op for a row an async nudge already delivered.
fn poll_until_resolved(store: &Store, id: &str, timeout: Duration) -> anyhow::Result<PollOutcome> {
    const POLL_INTERVAL: Duration = Duration::from_secs(1);
    let deadline = Instant::now() + timeout;
    loop {
        let row = store
            .get_user_request(id)?
            .ok_or_else(|| anyhow::anyhow!("user request {id} not found"))?;
        if row.status == ninox_core::types::UserRequestStatus::Cancelled {
            return Ok(PollOutcome::Cancelled);
        }
        if row.status == ninox_core::types::UserRequestStatus::Answered {
            if let Some(answer) = row.answer {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
                if let Err(e) = store.mark_user_request_answer_delivered(id, now) {
                    tracing::warn!("mark user-request answer delivered for {id}: {e}");
                }
                return Ok(PollOutcome::Answered(answer));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(PollOutcome::StillPending);
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

/// Handler for `ninox user-request ask` — `requester_session_id` is always
/// resolved server-side by the caller (`dispatch()`, via
/// `worker_status::resolve_session_id`), never accepted as a CLI argument,
/// the same posture `ninox send` takes for its own sender: a caller cannot
/// claim to be a session it merely names.
fn run_user_request_ask(
    store: &Store,
    requester_session_id: &str,
    prompt: &str,
    async_mode: bool,
    timeout: Duration,
) -> anyhow::Result<String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // pid + per-process sequence make the id unique even when two workers
    // (or one worker twice) ask in the same millisecond — same idiom as
    // `hooks::append_work_request`'s `wr-` ids.
    let id = format!("ur-{created_at}-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));

    store.insert_user_request(&ninox_core::store::UserRequestRow {
        id: id.clone(),
        requester_session_id: requester_session_id.to_string(),
        prompt: prompt.to_string(),
        mode: if async_mode { ninox_core::types::UserRequestMode::Async } else { ninox_core::types::UserRequestMode::Sync },
        status: ninox_core::types::UserRequestStatus::Pending,
        answer: None,
        answered_at: None,
        answer_delivered_at: None,
        created_at,
    })?;

    if async_mode {
        return Ok(id);
    }

    match poll_until_resolved(store, &id, timeout)? {
        PollOutcome::Answered(answer) => Ok(format!("{id}: answered: {answer}")),
        PollOutcome::Cancelled => anyhow::bail!("user request {id} {DISMISSED_MESSAGE}"),
        PollOutcome::StillPending => anyhow::bail!(
            "user request {id} timed out after {}s waiting for an answer \
             (use `ninox user-request wait {id}` to keep waiting)",
            timeout.as_secs(),
        ),
    }
}

/// Handler for `ninox user-request wait` — re-attaches to a request `ask`
/// already created (the restart-recovery path after an `ask --async` or an
/// earlier `wait` was interrupted). Only the session that asked may wait:
/// a successful wait stamps the row delivered, which would otherwise let
/// any other session silently consume the asker's answer (suppressing the
/// poller's delivery and the asker's own recovery briefing).
fn run_user_request_wait(store: &Store, caller_session_id: &str, id: &str, timeout: Duration) -> anyhow::Result<String> {
    let row = store.get_user_request(id)?.ok_or_else(|| anyhow::anyhow!("user request {id} not found"))?;
    anyhow::ensure!(
        row.requester_session_id == caller_session_id,
        "user request {id} belongs to a different session ({}) — only the session that asked it can wait on it; \
         use `ninox user-request status {id}` to read it without consuming the answer",
        row.requester_session_id,
    );
    match poll_until_resolved(store, id, timeout)? {
        PollOutcome::Answered(answer) => Ok(format!("{id}: answered: {answer}")),
        PollOutcome::Cancelled => anyhow::bail!("user request {id} {DISMISSED_MESSAGE}"),
        PollOutcome::StillPending => anyhow::bail!(
            "user request {id} timed out after {}s waiting for an answer", timeout.as_secs(),
        ),
    }
}

/// Handler for `ninox user-request status` — a single non-blocking read.
fn run_user_request_status(store: &Store, id: &str) -> anyhow::Result<String> {
    let row = store
        .get_user_request(id)?
        .ok_or_else(|| anyhow::anyhow!("user request {id} not found"))?;
    Ok(match row.status {
        ninox_core::types::UserRequestStatus::Pending => format!("{id}: pending"),
        ninox_core::types::UserRequestStatus::Answered => {
            format!("{id}: answered: {}", row.answer.unwrap_or_default())
        }
        ninox_core::types::UserRequestStatus::Cancelled => format!("{id}: cancelled — this request {DISMISSED_MESSAGE}"),
    })
}

/// `ninox prompt ask/clear/status` — thin dispatch onto
/// `ninox_ui_shared::prompt_agent`'s core functions, the same ones the
/// GUI/TUI prompt bar calls directly (never this CLI binary).
async fn run_prompt(store: Arc<Store>, config: AppConfig, action: PromptAction) -> anyhow::Result<()> {
    match action {
        PromptAction::Ask { text } => {
            let id = crate::prompt_agent::ask(store, &config, &text).await?;
            println!("delivered to {id}");
            Ok(())
        }
        PromptAction::Clear => {
            let busy = store.get_session(ninox_core::worker_type::PROMPT_SESSION_ID)?
                .is_some_and(|s| crate::prompt_agent::turn_in_flight(&s));
            if busy {
                println!("clearing after the current answer…");
            }
            crate::prompt_agent::clear(store, &config).await?;
            println!("prompt agent conversation cleared");
            Ok(())
        }
        PromptAction::Status { json } => {
            let status = crate::prompt_agent::status(&store).await?;
            if json {
                println!("{}", serde_json::json!({
                    "session_id": status.session_id,
                    "exists": status.exists,
                    "running": status.running,
                    "status": status.status,
                }));
            } else if !status.exists {
                println!("prompt agent: not started yet");
            } else {
                println!(
                    "prompt agent: {} ({})",
                    if status.running { "running" } else { "not running" },
                    status.session_id,
                );
            }
            Ok(())
        }
    }
}

/// `ninox complete` — resolves `$NINOX_SESSION`, builds a real `GithubApi`,
/// and delegates to `run_complete_with_github` (split out so tests can
/// supply a fake `GithubApi` instead of hitting the network for `PrMerged`
/// verification — same `*_from`-style seam `reactions.rs` uses).
async fn run_complete(
    store:    Arc<Store>,
    config:   AppConfig,
    port:     u16,
    summary:  String,
    evidence: Vec<String>,
    force:    bool,
) -> anyhow::Result<()> {
    let token = resolve_token(config.github_token.clone()).unwrap_or_default();
    let github = ninox_core::github::GitHubClient::new(token)?;
    run_complete_with_github(store, config, port, summary, evidence, force, &github).await
}

async fn run_complete_with_github(
    store:    Arc<Store>,
    config:   AppConfig,
    port:     u16,
    summary:  String,
    evidence: Vec<String>,
    force:    bool,
    github:   &dyn ninox_core::github::GithubApi,
) -> anyhow::Result<()> {
    use ninox_core::types::CompletionOutcome;
    use ninox_core::worker_type::{CompletionReview, SuccessCriterion};

    // The target is this CALLING process's own verified identity, never a
    // bare env var: `NINOX_SESSION` alone is forgeable by anything running
    // in the same shell, which would let a caller complete (and get
    // reaped) a session that isn't its own. `resolve_live_caller_session_id`
    // trusts the live pane identity first, falling back to a reverse
    // process-ancestry scan against every live session's recorded root pid
    // — the same verification `ninox spawn`'s `can_spawn` enforcement uses.
    // `NINOX_SESSION` is still checked, but only as a courtesy consistency
    // guard: if it's set and disagrees with the verified identity, that's a
    // forged-or-stale claim and this refuses outright rather than silently
    // trusting whichever side looks more convenient.
    let pane = ninox_core::runtime::current_private_pane_identity().await.ok().flatten();
    let pane_session_id = pane.as_ref().map(|p| p.physical_tmux_name.clone());
    let session_id = ninox_core::orchestrator_auth::resolve_live_caller_session_id(&store, pane_session_id.as_deref())
        .ok_or_else(|| anyhow::anyhow!(
            "could not verify the calling session's identity — `ninox complete` only works inside a Ninox worker session"
        ))?;
    if let Some(claimed) = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty()) {
        if claimed != session_id {
            anyhow::bail!(
                "NINOX_SESSION claims '{claimed}' but this process's verified identity is \
                 '{session_id}' — refusing (a worker can only complete itself)"
            );
        }
    }

    let session = store.get_session(&session_id)?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} is not known to this store"))?;

    // A worker completes only itself — there is no id argument to this
    // command — and an orchestrator is never finished by this mechanism.
    if store.list_orchestrators()?.iter().any(|o| o.id == session_id) {
        anyhow::bail!(
            "'{session_id}' is an orchestrator — orchestrators are never self-reaped by \
             `ninox complete`"
        );
    }

    let completion_review = session.worker_type.as_deref()
        .and_then(|name| config.worker_type_registry().get(name).cloned())
        .map(|spec| spec.completion_review)
        .unwrap_or_default();

    if let Some(ws) = session.workspace_path.as_deref() {
        if let Some(reason) = ninox_core::lifecycle::completion::worktree_unsafe_reason(Path::new(ws)).await? {
            if !force {
                anyhow::bail!("refusing to complete: {reason} — pass --force to override");
            }
        }
    }

    let criteria = if session.success_criteria.is_empty() {
        vec![SuccessCriterion::ReportSent]
    } else {
        session.success_criteria.clone()
    };
    let results = ninox_core::lifecycle::completion::verify_criteria(&session, &criteria, github).await;
    let failures: Vec<String> = results.iter()
        .filter_map(|(c, r)| r.as_ref().err().map(|e| format!("{c:?}: {e}")))
        .collect();
    if !failures.is_empty() {
        anyhow::bail!("cannot complete — unmet criteria:\n{}", failures.join("\n"));
    }

    // Notify the parent BEFORE the status write, using the pre-completion
    // session — attributed to this session itself (it is, by construction,
    // the only session `ninox complete` can ever act on). Routed via
    // `resolve_live_parent`, not `session.orchestrator_id` directly, so a
    // worker whose direct spawner already terminated still reaches a live
    // ancestor orchestrator instead of notifying nobody.
    if let Some(orch) = ninox_core::lifecycle::completion::resolve_live_parent(&store, &session) {
        let sessions_dir = AppConfig::sessions_dir();
        let acp = ninox_core::acp_client::HttpAcpRouter { port };
        let msg = ninox_core::lifecycle::reactions::format_completion_reaction(&session, &summary, &evidence);
        if let Err(e) = ninox_core::messaging::deliver_message_with_acp(
            &store, &sessions_dir, &orch, &msg, config.send_mechanism(),
            Some(&session_id), pane.as_ref(), ninox_core::types::MessageKind::Completion, &acp,
        ).await {
            // A worker finishing is more important than the notification
            // reaching its parent on the first try — the outcome is still
            // stored below, so the orchestrator can see it on its next look.
            tracing::warn!("ninox complete: notify parent {orch}: {e}");
        }
    } else if session.orchestrator_id.is_some() {
        // Unlike a plain delivery failure above, this has no `e` to log —
        // `resolve_live_parent` found no live session anywhere in the
        // ancestry chain (missing rows, every ancestor terminal, or the hop
        // limit). Worth a line so an operator scanning for "notify parent"
        // warnings isn't left wondering why a worker's completion never
        // shows up anywhere.
        tracing::warn!(
            "ninox complete: no live ancestor found for {session_id} — completion not delivered to any orchestrator"
        );
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    let outcome = CompletionOutcome { summary: summary.clone(), evidence: evidence.clone(), completed_at: now };

    // Re-read before writing — this session's own last act, but another
    // actor (the poller, an orchestrator's `ninox reap --force`) could have
    // written to this row in the time `verify_criteria`'s network call took.
    let mut row = store.get_session(&session_id)?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} was removed while completing"))?;
    row.completion_outcome = Some(outcome);
    let awaiting_review = completion_review == CompletionReview::Orchestrator;
    if awaiting_review {
        row.status = SessionStatus::AwaitingReview;
    } else {
        row.status = SessionStatus::Done;
        row.self_completed_at = Some(now);
    }
    store.upsert_session(&row)?;

    if awaiting_review {
        println!("completed — awaiting orchestrator review: {summary}");
    } else {
        println!("completed: {summary}");
        println!("this session will be reaped automatically.");
    }
    Ok(())
}

#[cfg(test)]
mod run_complete_tests {
    use super::{run_complete_with_github, test_fixtures, Arc, Store};
    use ninox_core::config::AppConfig;
    use ninox_core::types::{Session, SessionStatus};
    use ninox_core::worker_type::{CompletionReview, SuccessCriterion, WorkerTypeSpec};
    use std::future::Future;

    /// Sets `NINOX_SESSION=id` for the duration of `f`, serialized against
    /// every other test mutating process-global env vars (same guard
    /// `test_fixtures::with_env_override` uses) — that helper's closure is
    /// sync, so this is its async equivalent for `run_complete`'s tests.
    // Deliberately holds a `std::sync::Mutex` guard across `f().await`: the
    // whole point is serializing this env mutation against every OTHER
    // test's for the full duration of `f`'s async body, not just the
    // synchronous set/unset around it. Safe here — this guard is never
    // awaited while held by a task on a SHARED single-threaded executor
    // another task could be blocked behind; each `#[tokio::test]` gets its
    // own runtime, so cross-test contention just blocks that OS thread, the
    // intended effect.
    #[allow(clippy::await_holding_lock)]
    async fn with_ninox_session<F, Fut, T>(id: &str, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let _guard = test_fixtures::ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_SESSION").ok();
        std::env::set_var("NINOX_SESSION", id);
        let result = f().await;
        match prior {
            Some(v) => std::env::set_var("NINOX_SESSION", v),
            None    => std::env::remove_var("NINOX_SESSION"),
        }
        result
    }

    #[derive(Default)]
    struct FakeGithub {
        merged: std::sync::Mutex<Option<bool>>,
    }

    #[async_trait::async_trait]
    impl ninox_core::github::GithubApi for FakeGithub {
        async fn get_pr_status(&self, _owner: &str, _repo: &str, _pr_number: u64) -> anyhow::Result<ninox_core::github::PrStatus> {
            match *self.merged.lock().unwrap() {
                Some(merged) => Ok(ninox_core::github::PrStatus {
                    merged, state: "open".into(), mergeable: None, title: "t".into(),
                    number: 1, head_sha: String::new(), head_ref: String::new(), base_ref: String::new(),
                }),
                None => anyhow::bail!("404"),
            }
        }
        async fn get_ci_checks(&self, _o: &str, _r: &str, _s: &str) -> anyhow::Result<Vec<ninox_core::github::CheckRun>> { Ok(vec![]) }
        async fn get_review_threads(&self, _o: &str, _r: &str, _p: u64) -> anyhow::Result<Vec<ninox_core::github::ReviewThread>> { Ok(vec![]) }
        async fn get_issue_comments(&self, _o: &str, _r: &str, _p: u64) -> anyhow::Result<Vec<ninox_core::types::Comment>> { Ok(vec![]) }
        async fn find_open_pr_for_branch(&self, _o: &str, _r: &str, _b: &str) -> anyhow::Result<Option<ninox_core::github::PrRef>> { Ok(None) }
    }

    fn store() -> Arc<Store> {
        Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap())
    }

    /// `pid: Some(std::process::id())` so `resolve_live_caller_session_id`'s
    /// ancestry-scan fallback verifies this test process AS this session
    /// (the calling pid trivially "descends from" itself) — these tests run
    /// with no real tmux/ACP pane identity, so without this every
    /// `run_complete_with_github` call would refuse with an unverifiable
    /// caller.
    fn bare_session(id: &str) -> Session {
        let mut s = test_fixtures::session(id, None, SessionStatus::Working);
        s.pid = Some(std::process::id());
        s
    }

    fn run(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git").args(["-C", dir.to_str().unwrap()]).args(args).output().unwrap()
    }

    /// A git repo with one commit and no remote — enough to test the dirty
    /// check; `worktree_unsafe_reason`'s own tests cover the upstream/
    /// no-upstream distinction in detail.
    fn clean_git_repo() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap().keep();
        run(&dir, &["init", "-q"]);
        run(&dir, &["config", "user.email", "t@example.com"]);
        run(&dir, &["config", "user.name", "T"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        dir
    }

    // See `with_ninox_session`'s doc comment on why holding this guard
    // across the `.await` below is deliberate and safe in test code.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn complete_requires_a_verifiable_caller_identity() {
        // No sessions at all in the store (so no pid this test process could
        // match) and no NINOX_SESSION — the calling process verifies as
        // nobody's worker.
        let _guard = test_fixtures::ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_SESSION").ok();
        std::env::remove_var("NINOX_SESSION");
        let result = run_complete_with_github(
            store(), AppConfig::default(), 0, "done".into(), vec![], false, &FakeGithub::default(),
        ).await;
        if let Some(v) = prior { std::env::set_var("NINOX_SESSION", v); }
        let err = result.expect_err("must refuse with no verifiable identity").to_string();
        assert!(err.contains("verify"), "{err}");
    }

    #[tokio::test]
    async fn complete_refuses_a_mismatched_ninox_session_claim() {
        // The calling process verifies as "w1" (its own pid is recorded as
        // w1's root pid), but NINOX_SESSION claims a different session —
        // exactly the forgeable-env-var attack this check closes.
        let store = store();
        let mut s = bare_session("w1");
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("some-other-worker", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done".into(), vec![], false, &FakeGithub::default()).await
        }).await;

        let err = result.expect_err("a mismatched NINOX_SESSION claim must be refused").to_string();
        assert!(err.contains("some-other-worker") && err.contains("w1"), "{err}");

        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Working, "no side effects on the real session");
        assert!(after.completion_outcome.is_none());
    }

    #[tokio::test]
    async fn complete_refuses_for_an_orchestrator_session() {
        let store = store();
        store.upsert_session(&bare_session("orch-1")).unwrap();
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: "orch-1".into(), name: "orch-1".into(), created_at: 0,
        }).unwrap();
        let result = with_ninox_session("orch-1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done".into(), vec![], false, &FakeGithub::default()).await
        }).await;
        let err = result.expect_err("must refuse for an orchestrator").to_string();
        assert!(err.to_lowercase().contains("orchestrator"));
    }

    #[tokio::test]
    async fn complete_refuses_on_a_dirty_worktree_without_force() {
        let store = store();
        let ws = clean_git_repo();
        std::fs::write(ws.join("f.txt"), "changed").unwrap();
        let mut s = bare_session("w1");
        s.workspace_path = Some(ws.to_str().unwrap().to_string());
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done".into(), vec![], false, &FakeGithub::default()).await
        }).await;
        assert!(result.is_err());

        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Working, "status must be unchanged");
        assert!(after.completion_outcome.is_none(), "no outcome must be stored");
    }

    #[tokio::test]
    async fn complete_succeeds_on_a_dirty_worktree_with_force() {
        let store = store();
        let ws = clean_git_repo();
        std::fs::write(ws.join("f.txt"), "changed").unwrap();
        let mut s = bare_session("w1");
        s.workspace_path = Some(ws.to_str().unwrap().to_string());
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done".into(), vec![], true, &FakeGithub::default()).await
        }).await;
        assert!(result.is_ok(), "{result:?}");
        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Done);
    }

    #[tokio::test]
    async fn complete_refuses_when_a_verifiable_criterion_fails() {
        let store = store();
        let mut s = bare_session("w1");
        s.pr_number = Some(7);
        s.repo = "o/r".into();
        s.success_criteria = vec![SuccessCriterion::PrMerged];
        store.upsert_session(&s).unwrap();
        let gh = FakeGithub { merged: std::sync::Mutex::new(Some(false)) };

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done".into(), vec![], false, &gh).await
        }).await;
        let err = result.expect_err("an unmet PrMerged criterion must refuse").to_string();
        assert!(err.contains("PrMerged"));

        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Working);
        assert!(after.completion_outcome.is_none());
    }

    #[tokio::test]
    async fn complete_success_stores_outcome_notifies_parent_marks_done_and_flags_self_completed() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        // Unique per run — this tmux socket is shared with every other
        // concurrently-running test (and, on this machine, other Ninox
        // worker sessions' own test runs), so a fixed name risks a
        // "duplicate session" collision.
        let orch_id = format!(
            "orch-notify-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_id = orch_id.as_str();
        let orch_ws = tempfile::tempdir().unwrap().keep();
        ninox_core::tmux::create_session(orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();
        // `pid: None` — this fixture only needs to exist as a message
        // recipient, never as the caller; without this it would share the
        // test process's pid with the worker fixture below and could win
        // the ancestry-scan's caller-identity match instead of "w1".
        let mut orch_row = bare_session(orch_id);
        orch_row.pid = None;
        store.upsert_session(&orch_row).unwrap();
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: orch_id.into(), name: orch_id.into(), created_at: 0,
        }).unwrap();

        let mut s = bare_session("w1");
        s.orchestrator_id = Some(orch_id.into());
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(
                store.clone(), AppConfig::default(), 0,
                "all done here".into(), vec!["https://example.com/evidence".into()], false,
                &FakeGithub::default(),
            ).await
        }).await;
        assert!(result.is_ok(), "{result:?}");

        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Done);
        assert!(after.self_completed_at.is_some());
        let outcome = after.completion_outcome.expect("outcome must be stored");
        assert_eq!(outcome.summary, "all done here");
        assert_eq!(outcome.evidence, vec!["https://example.com/evidence".to_string()]);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = ninox_core::tmux::capture_screen(orch_id, 0, false).await.unwrap_or_default();
        assert!(screen.contains("all done here"), "orchestrator pane did not receive the completion message: {screen:?}");

        ninox_core::tmux::kill_session(orch_id).await.ok();
    }

    #[tokio::test]
    async fn complete_sends_exactly_one_completion_message_to_the_live_parent() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let orch_id = format!(
            "orch-completion-tag-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let orch_id = orch_id.as_str();
        let orch_ws = tempfile::tempdir().unwrap().keep();
        ninox_core::tmux::create_session(orch_id, orch_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();
        let mut orch_row = bare_session(orch_id);
        orch_row.pid = None;
        store.upsert_session(&orch_row).unwrap();
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: orch_id.into(), name: orch_id.into(), created_at: 0,
        }).unwrap();

        let mut s = bare_session("w1");
        s.orchestrator_id = Some(orch_id.into());
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done here".into(), vec![], false, &FakeGithub::default()).await
        }).await;
        assert!(result.is_ok(), "{result:?}");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = ninox_core::tmux::capture_screen(orch_id, 0, false).await.unwrap_or_default();
        assert_eq!(
            screen.matches("[completion]").count(), 1,
            "exactly one completion-tagged message must reach the parent: {screen:?}"
        );

        ninox_core::tmux::kill_session(orch_id).await.ok();
    }

    #[tokio::test]
    async fn complete_routes_to_the_nearest_live_ancestor_when_the_direct_parent_is_gone() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let grandparent_id = format!(
            "orch-grandparent-test-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        );
        let grandparent_id = grandparent_id.as_str();
        let gp_ws = tempfile::tempdir().unwrap().keep();
        ninox_core::tmux::create_session(grandparent_id, gp_ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();
        let mut gp_row = bare_session(grandparent_id);
        gp_row.pid = None;
        store.upsert_session(&gp_row).unwrap();
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: grandparent_id.into(), name: grandparent_id.into(), created_at: 0,
        }).unwrap();

        // The direct parent is gone (Terminated) — no tmux session for it,
        // so a message sent there would never be observable, proving the
        // test only passes if `resolve_live_parent` actually climbed past it.
        let mut dead_parent = bare_session("dead-parent");
        dead_parent.pid = None;
        dead_parent.orchestrator_id = Some(grandparent_id.into());
        dead_parent.status = SessionStatus::Terminated;
        store.upsert_session(&dead_parent).unwrap();

        let mut s = bare_session("w1");
        s.orchestrator_id = Some("dead-parent".into());
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "climbed to grandparent".into(), vec![], false, &FakeGithub::default()).await
        }).await;
        assert!(result.is_ok(), "{result:?}");

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let screen = ninox_core::tmux::capture_screen(grandparent_id, 0, false).await.unwrap_or_default();
        assert!(
            screen.contains("climbed to grandparent"),
            "the nearest LIVE ancestor must receive the message when the direct parent is gone: {screen:?}"
        );

        ninox_core::tmux::kill_session(grandparent_id).await.ok();
    }

    #[tokio::test]
    async fn complete_in_review_mode_sets_awaiting_review_instead_of_done() {
        let store = store();
        let config = AppConfig {
            worker_types: vec![WorkerTypeSpec {
                name: "ReviewedType".into(),
                completion_review: CompletionReview::Orchestrator,
                ..WorkerTypeSpec::default()
            }],
            ..AppConfig::default()
        };
        let mut s = bare_session("w1");
        s.worker_type = Some("ReviewedType".into());
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), config.clone(), 0, "done".into(), vec![], false, &FakeGithub::default()).await
        }).await;
        assert!(result.is_ok(), "{result:?}");
        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::AwaitingReview);
        assert!(after.self_completed_at.is_none(), "review mode must not self-reap");
    }

    #[tokio::test]
    async fn complete_prints_its_result_before_any_reap_happens() {
        let store = store();
        let mut s = bare_session("w1");
        s.success_criteria = vec![SuccessCriterion::ReportSent];
        store.upsert_session(&s).unwrap();

        let result = with_ninox_session("w1", || async {
            run_complete_with_github(store.clone(), AppConfig::default(), 0, "done".into(), vec![], false, &FakeGithub::default()).await
        }).await;
        assert!(result.is_ok());
        // No cleanup call happens inside run_complete — the session row
        // still has its workspace_path and the tmux session (none created
        // here) is untouched; physical reap is the poller's job (Task 9).
        let after = store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.status, SessionStatus::Done);
        assert_eq!(after.self_completed_at, after.self_completed_at, "flagged for the poller, not reaped here");
    }
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

/// `ninox prompt-answer`: writes `reply_to`'s `PromptReply` under
/// `sessions_dir` for the TUI's tick to pick up (see
/// `ninox_core::prompt_reply`). Rejects a reply with neither `answer` nor
/// `select` set — nothing useful for the prompt area to show, so it's a
/// usage error rather than a silent no-op write. `sessions_dir` is a
/// parameter (rather than resolved internally, like `inbox_sessions_dir()`
/// does for its caller above) so tests can point it at a tempdir instead
/// of mutating the process's real `NINOX_DATA_DIR`.
fn run_prompt_answer(sessions_dir: &Path, reply_to: &str, answer: Option<String>, select: Vec<String>) -> anyhow::Result<()> {
    let reply = ninox_core::prompt_reply::PromptReply { answer, select };
    anyhow::ensure!(!reply.is_empty(), "prompt-answer needs answer text and/or --select <node-id>");
    ninox_core::prompt_reply::write_reply(sessions_dir, reply_to, &reply)
}

#[cfg(test)]
mod run_prompt_answer_tests {
    use super::run_prompt_answer;

    const ID: &str = "11111111-1111-1111-1111-111111111111";

    #[test]
    fn errors_when_neither_answer_nor_select_is_given() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_prompt_answer(dir.path(), ID, None, vec![]).is_err());
    }

    #[test]
    fn a_successful_call_is_readable_back_via_take_reply() {
        let dir = tempfile::tempdir().unwrap();
        run_prompt_answer(dir.path(), ID, Some("blocked on review".to_string()), vec!["w1".to_string()]).unwrap();

        let reply = ninox_core::prompt_reply::take_reply(dir.path(), ID).unwrap().unwrap();
        assert_eq!(reply.answer.as_deref(), Some("blocked on review"));
        assert_eq!(reply.select, vec!["w1".to_string()]);
    }

    #[test]
    fn errors_when_reply_to_is_not_a_uuid() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_prompt_answer(dir.path(), "../escape", Some("x".to_string()), vec![]).is_err());
    }
}

/// Handler for `ninox whoami` / `ninox whoami --json` — resolves the
/// caller's own identity fresh from the store (never cached) and renders it.
/// `caller_type` is the caller's own `NINOX_CALLER_TYPE` — see
/// `ninox_core::identity::resolve_for_caller` for why it's needed (closing
/// the gap where a worker could claim its orchestrator's session id).
async fn run_whoami(
    store: &Store,
    config: &AppConfig,
    env_session: Option<&str>,
    cwd: Option<&Path>,
    caller_type: Option<&str>,
    json: bool,
) -> anyhow::Result<String> {
    let resolution = ninox_core::identity::resolve_for_caller(store, config, env_session, cwd, caller_type).await?;
    Ok(if json {
        serde_json::to_string_pretty(&ninox_core::identity::render_json(&resolution))?
    } else {
        ninox_core::identity::render_text(&resolution)
    })
}

/// Handler for `ninox identity-hook prompt`/`session-start` — the
/// `UserPromptSubmit`/`SessionStart` hooks installed only when
/// `[role_rehydration]` is enabled (see `ensure_statusline_settings` and
/// `orchestrator_root::setup_orchestrator_root`). Unlike [`run_inbox`],
/// this never prints nothing: an unmanaged/terminated session, or any
/// resolution failure, still yields an explicit reminder line rather than
/// silence (spec's "never omit the identity block" requirement).
async fn run_identity_hook(action: IdentityHookAction, db_path: PathBuf) {
    use std::io::Read;
    // Required by the Claude Code hook contract even though the reminder
    // doesn't need any of the payload's fields — see `run_inbox`'s identical
    // rationale.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let event = match action {
        IdentityHookAction::Prompt       => "UserPromptSubmit",
        IdentityHookAction::SessionStart => "SessionStart",
    };
    let env_session = std::env::var("NINOX_SESSION").ok().filter(|s| !s.is_empty());
    let caller_type = std::env::var("NINOX_CALLER_TYPE").ok().filter(|s| !s.is_empty());
    let cwd = std::env::current_dir().ok();
    let reminder = match Store::open(&db_path) {
        Ok(store) => {
            let config = AppConfig::load_or_default();
            match ninox_core::identity::resolve_for_caller(
                &store, &config, env_session.as_deref(), cwd.as_deref(), caller_type.as_deref(),
            ).await {
                Ok(resolution) => ninox_core::identity::render_reminder(&resolution),
                Err(e) => format!("[ninox] identity unavailable: {e}"),
            }
        }
        Err(e) => format!("[ninox] identity unavailable: {e}"),
    };
    println!("{}", serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": reminder,
        }
    }));
}

/// Which hook invoked `run_worker_status_hook` — the `ActivityEvent` is
/// built inside the handler because Stop's meaning depends on per-session
/// state (pending inbox messages), and NeedsInput/Permission's depend on the
/// payload's `tool_name`/`tool_input`.
enum WorkerStatusHookKind {
    Prompt,
    Stop,
    /// `PreToolUse`, registered with matcher `AskUserQuestion|ExitPlanMode`
    /// only — see `ensure_statusline_settings` (worker worktrees) and
    /// `orchestrator_root::setup_orchestrator_root` (orchestrator roots),
    /// which install the identical entry. Both also install `Stop`/
    /// `UserPromptSubmit`, which is what clears the `Blocked` this sets.
    NeedsInput,
    /// `PermissionRequest`, registered with matcher
    /// `ninox_core::worker_status::PERMISSION_REQUEST_MATCHER` (the tools
    /// that can actually need permission — not every tool call). Same two
    /// install sites as `NeedsInput`.
    Permission,
}

/// Handler for `ninox worker-status hook-prompt`/`hook-stop`/
/// `hook-needs-input`/`hook-permission` — the UserPromptSubmit/Stop/
/// PreToolUse/PermissionRequest hooks installed in a worker's worktree
/// settings (`ensure_statusline_settings`). Same contract as `run_inbox`:
/// never returns an error and never panics, because a broken status write
/// must never wedge the human's Claude Code session — any failure degrades
/// to doing nothing, with the error on stderr. Prints nothing: these hooks
/// have no output contract, and must never emit a `permissionDecision` —
/// they only observe, they never allow/deny the tool call they're reporting
/// on.
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
        WorkerStatusHookKind::NeedsInput => {
            // Missing tool_name means a payload shape we don't recognize —
            // no-op rather than guess, same as an unidentifiable session.
            let Some(tool_name) = ninox_core::worker_status::hook_payload_tool_name(&input) else {
                return;
            };
            ninox_core::worker_status::ActivityEvent::HookAwaitingInput { tool_name }
        }
        WorkerStatusHookKind::Permission => {
            let Some(tool_name) = ninox_core::worker_status::hook_payload_tool_name(&input) else {
                return;
            };
            let tool_input = ninox_core::worker_status::hook_payload_tool_input(&input)
                .unwrap_or(serde_json::Value::Null);
            ninox_core::worker_status::ActivityEvent::HookAwaitingPermission { tool_name, tool_input }
        }
    };
    if let Err(e) = ninox_core::worker_status::apply_activity(&store, &session_id, event, now) {
        eprintln!("ninox worker-status: {e}");
    }
}

fn inbox_messaging_enabled() -> bool {
    AppConfig::load_or_default().send_mechanism()
        == ninox_core::config::SendMechanism::Inbox
}

fn inbox_sessions_dir() -> PathBuf {
    AppConfig::sessions_dir()
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

pub fn group_sessions(
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

pub(crate) fn render_session_board(
    groups: &[(Option<ninox_core::types::Orchestrator>, Vec<Session>)],
    queue_positions: &std::collections::HashMap<String, usize>,
    pr_summaries: &std::collections::HashMap<String, String>,
) -> String {
    if groups.iter().all(|(_, m)| m.is_empty()) {
        return "no sessions — start one with `ninox orchestrate <name>`".to_string();
    }
    let pr_label = |s: &Session| {
        pr_summaries.get(&s.id).cloned().or_else(|| s.pr_number.map(|n| format!("PR #{n}"))).unwrap_or_default()
    };
    let pr_w = groups.iter().flat_map(|(_, m)| m).map(|s| pr_label(s).chars().count()).max().unwrap_or(0).max(8);
    let mut out = String::new();
    for (orch, members) in groups {
        match orch {
            Some(o) => out.push_str(&format!("{} ({})\n", o.name, o.id)),
            None    => out.push_str("(no orchestrator)\n"),
        }
        for s in members {
            let pr = pr_label(s);
            let status = match queue_positions.get(&s.id) {
                Some(n) if s.status == SessionStatus::Queued => format!("queued #{n}"),
                _ => status_slug(&s.status).to_string(),
            };
            out.push_str(&format!(
                "  {:<24} {:<10} {:<20} {:<pr_w$} ${:.2}\n",
                s.id, status, s.repo, pr, s.cost_usd,
            ));
        }
    }
    out
}

pub fn status_slug(s: &SessionStatus) -> &'static str {
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
        SessionStatus::AwaitingReview => "awaiting_review",
        SessionStatus::Queued        => "queued",
    }
}

pub(crate) fn run_list_sessions(store: &ninox_core::store::Store, json: bool) -> anyhow::Result<String> {
    use ninox_core::session_limits;
    let config = AppConfig::load_or_default();
    let queue = store.list_queue()?;
    let crate::session_queue::QueueView { sessions, orchestrators, positions, limits } =
        crate::session_queue::queue_view(store, &config)?;
    if json {
        // `sessions`/`orchestrators` stay raw store rows, so a consumer built
        // before `queued` existed still parses every status; waiting
        // sessions are listed only under `queue`.
        let queue_json: Vec<_> = queue
            .iter()
            .filter_map(|e| positions.get(&e.id).map(|n| serde_json::json!({
                "id": e.id,
                "name": e.session.name,
                "orchestrator_id": e.session.orchestrator_id,
                "worker_type": e.session.worker_type,
                "kind": if e.kind == ninox_core::store::QueueKind::Relaunch { "restore" } else { "spawn" },
                "position": n,
                "location": session_limits::location_label(&config, e.location.as_deref()),
            })))
            .collect();
        let limits_json: Vec<_> = limits
            .iter()
            .map(|l| serde_json::json!({
                "location": l.label,
                "running": l.running,
                "orchestrators": l.orchestrators,
                "limit": l.limit,
                "queued": l.queued,
            }))
            .collect();
        let sessions_json: Vec<serde_json::Value> = store.list_sessions()?
            .into_iter()
            .map(|s| {
                let set = ninox_core::brain_set::BrainSet::from_session(&s, &config);
                let mut v = serde_json::to_value(&s).unwrap_or_default();
                v["brain"] = serde_json::json!({
                    "reads":  set.names(&config),
                    "writes": set.write_member(&config).catalogue.name,
                });
                v
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "orchestrators": store.list_orchestrators()?,
            "sessions": sessions_json,
            "queue": queue_json,
            "limits": limits_json,
        }))?);
    }
    let headline = crate::session_queue::limits_headline(&limits);
    let pr_summaries: std::collections::HashMap<String, String> =
        ninox_core::session_prs::for_sessions(store, sessions.iter())
            .into_iter()
            .filter_map(|(id, prs)| ninox_core::session_prs::summary(&prs).map(|s| (id, s)))
            .collect();
    Ok(format!("{headline}\n{}", render_session_board(&group_sessions(sessions, orchestrators), &positions, &pr_summaries)))
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
    let config = AppConfig::load_or_default();
    let pane_session_id = ninox_core::runtime::current_private_pane_identity().await.ok().flatten()
        .map(|p| p.physical_tmux_name);
    let (set, caller_session) = ninox_core::brain_set::resolve_for_caller(&store, pane_session_id.as_deref(), &config)?;
    let brain_path = set.write.clone();
    // First open of a config-declared remote catalogue materializes its
    // .sync.toml (a local-fs write only; the sync itself is lazy).
    for path in &set.reads {
        if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, path) {
            tracing::warn!("brain: failed to materialize .sync.toml for {}: {e}", path.display());
        }
    }

    match action {
        BrainAction::Index { all } => {
            for member in brain_targets(&set, &config, all) {
                let path = &member.catalogue.path;
                if member.writable {
                    run_remote_sync_if_configured(path).await;
                } else {
                    run_remote_pull_if_configured(path).await;
                }
                let brain = BrainIndex::open(path)?;
                let embedder = try_build_embedder();
                let stats = brain.rebuild(embedder.as_deref())?;
                println!(
                    "{}indexed {} entries ({} embedded, {} cached)",
                    member_prefix(&set, &member), stats.indexed, stats.embedded, stats.cached
                );
            }
        }
        BrainAction::Sync { all } => {
            let mut any_remote = false;
            for member in brain_targets(&set, &config, all) {
                let path = &member.catalogue.path;
                match ninox_core::brain_sync::BrainSync::for_brain(path).await {
                    Ok(None) => continue,
                    Ok(Some(sync)) => {
                        any_remote = true;
                        let report = if member.writable { sync.sync().await? } else { sync.pull_if_stale().await? };
                        print!("{}", member_prefix(&set, &member));
                        print_sync_report(&report);
                        if report.changed_local() {
                            let brain = BrainIndex::open(path)?;
                            let embedder = try_build_embedder();
                            brain.rebuild(embedder.as_deref())?;
                        }
                    }
                    Err(e) => anyhow::bail!("brain remote unavailable for {}: {e}", member.name()),
                }
            }
            if !any_remote {
                eprintln!("this brain has no remote — configure one with `ninox brain remote set s3://bucket/prefix`");
                std::process::exit(1);
            }
        }
        BrainAction::Query { text, entry_type, tag, catalogue } => {
            let embedder = if text.trim().is_empty() { None } else { try_build_embedder() };
            let reader = open_brain_reader(&set, &config, catalogue.as_deref(), embedder.as_deref()).await?;
            let filters = QueryFilters { entry_type, tag };
            for entry in reader.query(&text, embedder.as_deref(), filters)? {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Show { path, catalogue } => {
            let reader = open_brain_reader(&set, &config, catalogue.as_deref(), None).await?;
            match reader.get(&path)? {
                Some(entry) => println!("{}", serde_json::to_string_pretty(&entry)?),
                None => {
                    eprintln!("entry not found: {path}");
                    std::process::exit(1);
                }
            }
        }
        BrainAction::Related { id, limit, catalogue } => {
            let reader = open_brain_reader(&set, &config, catalogue.as_deref(), None).await?;
            for entry in reader.related(&id, limit)? {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Backlinks { id, catalogue } => {
            let reader = open_brain_reader(&set, &config, catalogue.as_deref(), None).await?;
            for entry in reader.backlinks(&id)? {
                println!("{} ({}) — {}", entry.name, entry.entry_type, entry.id);
            }
        }
        BrainAction::Add { path, content, catalogue } => {
            let content = match content {
                Some(c) => c,
                None => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    buf
                }
            };
            let (member, stats) = run_brain_add_in_set(&set, &config, catalogue.as_deref(), &path, &content)?;
            let to = if set.reads.len() > 1 { format!(" to {}", member.name()) } else { String::new() };
            println!(
                "wrote {path}{to} and indexed {} entries ({} embedded, {} cached)",
                stats.indexed, stats.embedded, stats.cached
            );
        }
        BrainAction::Export { output } => {
            let stats = ninox_core::brain_archive::export(&brain_path, &output)?;
            println!("exported {} entries to {}", stats.files, output.display());
        }
        BrainAction::Import { input, into, force } => {
            let target = match (into, &caller_session) {
                (None, _) => set.authorize_write(&config, None)?.catalogue.path,
                (Some(into), Some(_)) => set.authorize_write(&config, Some(&into.to_string_lossy()))?.catalogue.path,
                (Some(into), None) => into,
            };
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
            // Inside a session every discovered repo goes to its own write
            // catalogue; only the user fans out to each session's catalogue.
            let target = set.authorize_write(&config, None)?.catalogue.path;
            run_discover_repos(&target, &store, paths, caller_session.is_none().then_some(&config))?;
        }
        BrainAction::Remote { action } => match action {
            RemoteAction::Set { url, endpoint, region, ttl } => {
                set.authorize_write(&config, None)?;
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
                set.authorize_write(&config, None)?;
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

/// The read-only counterpart of [`run_remote_sync_if_configured`]: a
/// read-only source is pulled, never pushed.
async fn run_remote_pull_if_configured(brain_path: &std::path::Path) {
    match ninox_core::brain_sync::BrainSync::for_brain(brain_path).await {
        Ok(None) => {}
        Ok(Some(sync)) => match sync.pull_if_stale().await {
            Ok(report) => print_sync_report(&report),
            Err(e) => eprintln!("brain pull failed (continuing with local index): {e}"),
        },
        Err(e) => eprintln!("brain remote unavailable (continuing local-only): {e}"),
    }
}

/// `index`/`sync` targets: the write catalogue, or with `all` every read.
fn brain_targets(
    set: &ninox_core::brain_set::BrainSet,
    config: &AppConfig,
    all: bool,
) -> Vec<ninox_core::brain_set::Member> {
    if all { set.members(config) } else { vec![set.write_member(config)] }
}

/// `"name: "` when the set has several members, so per-catalogue output
/// lines are attributable; empty for a single-brain set (output unchanged).
fn member_prefix(set: &ninox_core::brain_set::BrainSet, member: &ninox_core::brain_set::Member) -> String {
    if set.reads.len() > 1 { format!("{}: ", member.name()) } else { String::new() }
}

/// A single-brain set reads its one `BrainIndex` directly, so its ids and
/// output are exactly what they were before brain sets; anything wider
/// goes through `MergedBrainIndex` with `source:id` ids.
enum BrainReader {
    Single(BrainIndex),
    Merged(ninox_core::MergedBrainIndex),
}

impl BrainReader {
    fn query(&self, text: &str, embedder: Option<&dyn ninox_core::embeddings::Embedder>, filters: QueryFilters) -> anyhow::Result<Vec<ninox_core::BrainEntry>> {
        match self {
            Self::Single(b) => b.query(text, embedder, filters),
            Self::Merged(m) => m.query(text, embedder, filters),
        }
    }

    /// A merged lookup accepts a bare id too: the first brain that has it.
    fn qualify(&self, id: &str) -> anyhow::Result<String> {
        let Self::Merged(m) = self else { return Ok(id.to_string()) };
        if let Some((source, _)) = ninox_core::MergedBrainIndex::split_qualified(id) {
            if m.sources().iter().any(|s| s.name == source) {
                return Ok(id.to_string());
            }
        }
        for src in m.sources() {
            if src.index.get(id)?.is_some() {
                return Ok(ninox_core::MergedBrainIndex::qualify(&src.name, id));
            }
        }
        Ok(id.to_string())
    }

    fn get(&self, id: &str) -> anyhow::Result<Option<ninox_core::BrainEntry>> {
        match self {
            Self::Single(b) => b.get(id),
            Self::Merged(m) => m.get(&self.qualify(id)?),
        }
    }

    fn related(&self, id: &str, limit: usize) -> anyhow::Result<Vec<ninox_core::BrainEntry>> {
        match self {
            Self::Single(b) => b.related(id, limit),
            Self::Merged(m) => m.related(&self.qualify(id)?, limit),
        }
    }

    fn backlinks(&self, id: &str) -> anyhow::Result<Vec<ninox_core::BrainEntry>> {
        match self {
            Self::Single(b) => b.backlinks(id),
            Self::Merged(m) => m.backlinks(&self.qualify(id)?),
        }
    }
}

/// Opens the brains a read should search (`catalogue` narrows to one
/// member of the set), each freshened from its remote first. A brain that
/// fails to open is skipped with a warning rather than failing the read.
async fn open_brain_reader(
    set: &ninox_core::brain_set::BrainSet,
    config: &AppConfig,
    catalogue: Option<&str>,
    embedder: Option<&dyn ninox_core::embeddings::Embedder>,
) -> anyhow::Result<BrainReader> {
    let members = set.authorize_read(config, catalogue)?;
    if set.reads.len() == 1 {
        return Ok(BrainReader::Single(ninox_core::brain_sync::open_synced(&members[0].catalogue.path, embedder).await?));
    }
    let mut sources = Vec::with_capacity(members.len());
    for m in members {
        match ninox_core::brain_sync::open_synced(&m.catalogue.path, embedder).await {
            Ok(index) => sources.push(ninox_core::MergedSource { name: m.catalogue.name.clone(), index: Arc::new(index) }),
            Err(e) => eprintln!("warning: skipping brain '{}': {e}", m.name()),
        }
    }
    Ok(BrainReader::Merged(ninox_core::MergedBrainIndex::from_sources(sources)))
}

/// `ninox brain add` against a brain set: `catalogue` (or the set's write
/// catalogue) must be a writable member — see `BrainSet::authorize_write`.
fn run_brain_add_in_set(
    set: &ninox_core::brain_set::BrainSet,
    config: &AppConfig,
    catalogue: Option<&str>,
    path: &str,
    content: &str,
) -> anyhow::Result<(ninox_core::brain_set::Member, ninox_core::brain::RebuildStats)> {
    let member = set.authorize_write(config, catalogue)?;
    let stats = run_brain_add(&member.catalogue.path, path, content)?;
    Ok((member, stats))
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
/// own recorded write catalogue (its `NINOX_BRAIN` at spawn time — see
/// `Session::brain_write`) takes precedence over `default_brain_path` —
/// the same rule `Poller::trigger_brain_harvest` follows, and for the same
/// reason: a worker spawned against a non-default catalogue must have its
/// facts land in that catalogue, not silently in whichever brain happens to
/// be default for this CLI invocation.
fn run_discover_repos(
    default_brain_path: &std::path::Path,
    store: &Store,
    paths: Vec<PathBuf>,
    // `Some` (the user, outside any session): route each session's
    // workspace to that session's own writable write catalogue. `None`:
    // everything lands in `default_brain_path`.
    per_session: Option<&AppConfig>,
) -> anyhow::Result<()> {
    let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    if paths.is_empty() {
        for session in store.list_sessions()? {
            let catalogue = match per_session {
                Some(config) if session.brain_write.is_some() => {
                    match ninox_core::brain_set::BrainSet::from_session(&session, config).authorize_write(config, None) {
                        Ok(m) => m.catalogue.path,
                        Err(e) => {
                            eprintln!("skipping {}: {e}", session.id);
                            continue;
                        }
                    }
                }
                _ => default_brain_path.to_path_buf(),
            };
            let Some(workspace) = session.workspace_path else { continue };
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
pub fn write_brain_entry(brain_path: &std::path::Path, id: &str, content: &str) -> anyhow::Result<()> {
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
            brain_reads: Vec::new(),
            brain_write: catalogue_path.map(|p| p.to_string_lossy().to_string()),
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
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

        let _iso = ninox_core::test_isolation::isolate();
        run_discover_repos(&default_brain, &store, Vec::new(), Some(&ninox_core::config::AppConfig::default())).unwrap();

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

    /// Inside a session (no per-session routing), discovery writes only to
    /// the caller's own write catalogue — never to other sessions' brains.
    #[test]
    fn discover_repos_from_a_session_only_writes_its_own_catalogue() {
        let tmp = tempfile::tempdir().unwrap();
        let mine = tmp.path().join("mine");
        let theirs = tmp.path().join("theirs");
        let repo = tmp.path().join("repo-theirs");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo, "git@github.com:acme/repo-theirs.git");
        let store = Store::open(tmp.path().join("store.db")).unwrap();
        store.upsert_session(&session("s-theirs", &repo, Some(&theirs))).unwrap();

        run_discover_repos(&mine, &store, Vec::new(), None).unwrap();

        assert!(mine.join("repos/repo-theirs.md").exists());
        assert!(!theirs.join("repos").exists(), "another session's brain must not be written");
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
pub fn build_embedder(show_download_progress: bool) -> Option<Arc<dyn ninox_core::embeddings::Embedder>> {
    match ninox_core::embeddings::FastEmbedEmbedder::try_new_with_progress(show_download_progress) {
        Ok(embedder) => Some(Arc::new(embedder)),
        Err(err) => {
            tracing::warn!("brain: embedding model unavailable, falling back to keyword-only search: {err}");
            None
        }
    }
}

/// True when this process was started through the `nx` alias.
/// Everything an interactive UI needs to finish launching, once
/// [`bootstrap`] has set up the in-process engine/poller/server.
pub struct UiBootstrap {
    pub engine:             Arc<Engine>,
    pub orchestrator_root:  PathBuf,
    pub brain:              Arc<BrainIndex>,
    pub db_path:            PathBuf,
    pub token:              CancellationToken,
    /// The effective port (`--port`, else the config's).
    pub port:               u16,
}

pub enum DaemonOutcome {
    /// Already acted as a background daemon (headless, or no display) and
    /// blocked until shutdown — the caller has nothing left to do.
    ActedAsDaemon,
    /// Ready for an interactive UI to take over.
    Ready(UiBootstrap),
}

/// Shared bootstrap for both an in-process GUI (which always hosts its own
/// poller/server — see the comment below) and a standalone headless daemon:
/// resolves config, sets up the orchestrator root, opens the brain index +
/// embedder, builds the engine, and starts the poller/server. Used
/// identically by `ninox-gui` (its `--headless`/no-display path and its
/// normal window-opening path) and by `ninox-tui` (its own `--headless`
/// path only — the interactive TUI runs as a thin client against an
/// out-of-process daemon instead, via `tui::run`).
pub async fn bootstrap(
    store:    Arc<Store>,
    port_arg: Option<u16>,
    headless: bool,
    db_path:  PathBuf,
) -> anyhow::Result<DaemonOutcome> {
    let config = AppConfig::load_or_default();
    let port = port_arg.unwrap_or(config.port);
    let orchestrator_root = config.resolved_orchestrator_root();
    let config_path = AppConfig::config_path().to_string_lossy().to_string();
    let brain_path = config.resolved_brain_path();

    let ninox_bin = ninox_core::hooks::canonical_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "ninox".to_string());

    let act_as_daemon = headless || !has_display();
    // Registered before any slow startup step, so a stop signal that lands
    // during it is still named in the log rather than killing the process
    // silently.
    let mut stop_signals = None;
    // Releases the marker when startup fails with an error or a panic unwinds.
    let mut _marker_guard = None;
    if act_as_daemon {
        ninox_core::daemon_log::install_panic_hook();
        tracing::info!(
            "ninox daemon starting: pid {} ppid {} version {} build {} port {port} db {} args {:?}",
            std::process::id(),
            std::os::unix::process::parent_id(),
            env!("CARGO_PKG_VERSION"),
            ninox_core::server_control::BUILD_HASH,
            db_path.display(),
            std::env::args().collect::<Vec<_>>(),
        );
        let mut signals = ninox_core::daemon_log::StopSignals::install()?;
        let named = Arc::new(std::sync::OnceLock::new());
        let reached_loop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = CancellationToken::new();
        tokio::spawn({
            let (named, reached_loop, stopped) = (Arc::clone(&named), Arc::clone(&reached_loop), stopped.clone());
            let db_path = db_path.clone();
            async move {
                let name = signals.stop().await;
                let _ = named.set(name);
                if reached_loop.load(std::sync::atomic::Ordering::SeqCst) {
                    stopped.cancel();
                } else {
                    // Still starting up (embedder, brain): nothing is
                    // mid-write yet, so stop now instead of ignoring it.
                    tracing::warn!("daemon exiting: received {name} during startup");
                    ninox_core::daemon_log::release_marker(&db_path);
                    ninox_core::daemon_log::flush();
                    std::process::exit(0);
                }
            }
        });
        stop_signals = Some((named, reached_loop, stopped));
    }
    if daemon_should_yield(act_as_daemon, port).await {
        // Exit 0: launchd's KeepAlive{SuccessfulExit=false} and systemd's
        // Restart=on-failure only restart failures, so a login-item daemon
        // finding the GUI (or a TUI-spawned daemon) on the port stops here
        // instead of crash-looping, each loop re-seeding and polling.
        tracing::warn!("daemon exiting: another ninox already serves :{port}");
        ninox_core::daemon_log::flush();
        return Ok(DaemonOutcome::ActedAsDaemon);
    }
    if act_as_daemon {
        match ninox_core::daemon_log::claim_marker(&db_path) {
            ninox_core::daemon_log::PriorRun::Vanished(pid) => tracing::warn!(
                "previous daemon (pid {pid}) ended without logging an exit: killed by SIGKILL or the OS (memory pressure), or crashed hard"
            ),
            ninox_core::daemon_log::PriorRun::Clean | ninox_core::daemon_log::PriorRun::Live(_) => {}
        }
        _marker_guard = Some(MarkerGuard(db_path.clone()));
    }

    if let Err(e) = ninox_core::orchestrator_root::setup_orchestrator_root(
        &orchestrator_root, &config, &ninox_bin, &config_path,
    ).await
    {
        tracing::warn!("orchestrator root setup failed: {e}");
    }

    let brain = Arc::new(BrainIndex::open(&brain_path)?);
    let engine = match resolve_token(config.github_token.clone()) {
        Some(token) => Engine::new_with_github(Arc::clone(&store), token),
        None        => Engine::new(Arc::clone(&store)),
    };
    let token = CancellationToken::new();

    // The embedding model (a cold load is seconds, dominated by
    // tokenizer/ONNX-session init), the already-running port probe, and the
    // in-process poller/server are all startup side-effects that nothing
    // the interactive GUI renders depends on — `UiBootstrap` below never
    // carries any of their results. The headless daemon path (`ninox-tui
    // --headless`, and the no-display fallback above) *is* this work, so
    // it still awaits `setup` directly and keeps its exact prior ordering;
    // only the GUI path defers it, so none of its cost sits between
    // process launch and the first visible window.
    let setup = {
        let store = Arc::clone(&store);
        let info_db_path = db_path.clone();
        let engine = engine.clone();
        let brain = brain.clone();
        let token = token.clone();
        async move {
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

            // A deliberate --headless (and the no-display daemon fallback
            // above, which behaves the same way) must bind the port or die —
            // otherwise a bind failure just gets traced and the process
            // parks on ctrl_c forever, leaving a permanent duplicate poller.
            // The GUI path instead skips hosting entirely when something is
            // already listening, since the GUI reads the store directly and
            // doesn't need its own poller/server.
            let already_running = !act_as_daemon && ninox_core::daemon::port_in_use(port).await;

            // The GUI hosts its own poller for liveness and its live
            // cost/context updates (in-process Engine events). GitHub is the
            // exception: when another process already serves the port, that
            // daemon polls GitHub and this poller stays off it (and the GUI
            // re-reads the store's PR rows on its own timer), so two
            // pollers never spend one token. If the daemon later goes away
            // this poller starts polling GitHub itself.
            let mut poller = Poller::new(engine.clone())
                .with_fleet_restorer(fleet::auto_restorer(Arc::clone(&store)))
                .with_queue_launcher(crate::session_queue::queue_launcher(Arc::clone(&store)))
                .with_plan_advancer(crate::ghost_accept::plan_advancer(Arc::clone(&store)));
            if already_running {
                poller = poller.with_external_github_daemon(port);
            }
            let poller_task = tokio::spawn(async move { poller.start(token).await });

            let server_task = if already_running {
                tracing::info!("port :{port} already in use — not binding the server (poller runs in-process)");
                None
            } else {
                let task = tokio::spawn({
                    let e = engine.clone();
                    let b = brain.clone();
                    let emb = embedder.clone();
                    let info = ninox_core::server_control::ServerInfo::current(port, &info_db_path, !act_as_daemon);
                    async move {
                        // Traced here, inside the task, rather than only at
                        // the headless select-arm below — the GUI path
                        // never awaits this handle, so that's the only
                        // place a bind/start failure would otherwise be
                        // visible at all.
                        let res = ninox_server::start(e, b, emb, port, info).await;
                        if let Err(err) = &res {
                            tracing::error!("server: {err}");
                        }
                        res
                    }
                });
                tracing::info!("ninox ready on :{port}");
                Some(task)
            };
            (poller_task, server_task)
        }
    };

    if act_as_daemon {
        // already_running is forced false inside `setup` when act_as_daemon,
        // so the server was always spawned.
        let (mut poller_task, task) = setup.await;
        let task = task.expect("act_as_daemon implies the server was spawned");
        let (signal_name, reached_loop, signal_stopped) = stop_signals.expect("act_as_daemon registers the stop signals");
        reached_loop.store(true, std::sync::atomic::Ordering::SeqCst);
        // A signal that arrived while `setup` ran is already cancelled-or-
        // exited above; one that lands in this gap sets the name and, as
        // `reached_loop` is now true, cancels `signal_stopped`.

        // A shutdown accepted on the control socket, or a SIGTERM from a
        // restart's fallback, launchd or systemd, stops the poller and
        // server through `token` like ctrl_c does, rather than dying
        // mid-write.
        let control_stop = CancellationToken::new();
        tokio::spawn({
            let path = ninox_core::server_control::control_socket_path(port, &db_path);
            let store = Arc::clone(&store);
            let control_stop = control_stop.clone();
            async move {
                if let Err(e) = ninox_core::server_control::serve_control(path, store, control_stop).await {
                    tracing::warn!("control socket unavailable, `ninox server restart` will fall back to SIGTERM: {e}");
                }
            }
        });
        let stop = tokio::select! {
            () = signal_stopped.cancelled() => DaemonStop::Signal(signal_name.get().copied().unwrap_or("a stop signal")),
            () = control_stop.cancelled() => DaemonStop::ControlStop,
            res = task => match res {
                Ok(Ok(())) => DaemonStop::ServerReturned,
                Ok(Err(e)) => DaemonStop::ServerFailed(e),
                Err(join_err) => DaemonStop::ServerFailed(anyhow::anyhow!("server task panicked: {join_err}")),
            },
            res = &mut poller_task => DaemonStop::PollerEnded(match res {
                Ok(()) => "returned before shutdown".to_string(),
                Err(join_err) => format!("panicked: {join_err}"),
            }),
        };
        // One line saying why, before anything else happens.
        tracing::warn!("daemon exiting: {}", stop.describe());
        token.cancel();
        control_stop.cancel();
        if !matches!(stop, DaemonStop::PollerEnded(_)) {
            // Let the poller finish the tick it's in, so a read→apply→write
            // store update isn't cut off between its read and its write.
            // Arbitrary bound; a tick that runs longer is abandoned.
            if tokio::time::timeout(std::time::Duration::from_secs(10), poller_task).await.is_err() {
                tracing::warn!("poller still busy 10s after shutdown — exiting anyway");
            }
        }
        // `process::exit` skips destructors, so an in-flight auto-restore's
        // lease is released by hand first.
        let die = |code: i32| {
            fleet::release_own_restore_lease(&store);
            ninox_core::daemon_log::release_marker(&db_path);
            ninox_core::daemon_log::flush();
            std::process::exit(code);
        };
        match &stop {
            // Lost a bind race to another ninox since the probe above.
            DaemonStop::ServerFailed(_) if ninox_core::daemon::port_in_use(port).await => {
                tracing::warn!("another ninox already serves :{port} — this daemon was a duplicate");
                // Only to another daemon: an in-process GUI never releases it.
                if let Some(info) = server_restart::fetch_info(port).await.filter(|i| !i.in_process) {
                    ninox_core::daemon_log::hand_over_marker(&db_path, info.pid);
                }
            }
            // A daemon that can't bind/run its port is a duplicate poller
            // waiting to happen — die loudly instead of parking forever
            // (daemon.log is where `ensure_daemon`'s startup wait points
            // users). Nonzero so launchd/systemd restart it.
            DaemonStop::ServerFailed(_) | DaemonStop::ServerReturned | DaemonStop::PollerEnded(_) => {
                tracing::error!("daemon exited abnormally: {}", stop.describe());
                die(1);
            }
            DaemonStop::Signal(_) | DaemonStop::ControlStop => {}
        }
        ninox_core::daemon_log::release_marker(&db_path);
        ninox_core::daemon_log::flush();
        return Ok(DaemonOutcome::ActedAsDaemon);
    }

    tokio::spawn(setup);

    if config.runtime.backend == ninox_core::runtime::Backend::Tmux {
        if let Err(e) = tmux::require_version().await {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }

    Ok(DaemonOutcome::Ready(UiBootstrap {
        engine, orchestrator_root, brain, db_path, token, port,
    }))
}

/// Clears `daemon.pid` on drop, so an early `?` return (a logged error)
/// isn't later reported as a vanished daemon.
struct MarkerGuard(PathBuf);

impl Drop for MarkerGuard {
    fn drop(&mut self) {
        ninox_core::daemon_log::release_marker(&self.0);
        ninox_core::daemon_log::flush();
    }
}

/// Why the headless daemon's main loop ended.
enum DaemonStop {
    Signal(&'static str),
    ControlStop,
    /// The HTTP server returned `Ok`: it only does that if its listener
    /// closed, which nothing asked for.
    ServerReturned,
    ServerFailed(anyhow::Error),
    PollerEnded(String),
}

impl DaemonStop {
    fn describe(&self) -> String {
        match self {
            Self::Signal(name) => format!("received {name}"),
            Self::ControlStop => "stop accepted on the control socket (requester logged above)".to_string(),
            Self::ServerReturned => "the server task returned Ok without being asked to stop".to_string(),
            Self::ServerFailed(e) => format!("the server task failed: {e}"),
            Self::PollerEnded(how) => format!("the poller task ended: {how}"),
        }
    }
}

/// Prints a stderr-only hint (stdout stays clean for `--json` consumers)
/// when nothing is listening on the effective port, so `ninox list`/`ninox
/// connect` can warn readers that statuses may be stale without depending
/// on a daemon they didn't start.
async fn warn_if_daemon_down(port_arg: Option<u16>) {
    let port = port_arg.unwrap_or_else(|| AppConfig::load_or_default().port);
    if !ninox_core::daemon::port_in_use(port).await {
        eprintln!(
            "note: background services not running — statuses may be stale \
             (they start automatically with ninox tui or ninox orchestrate)"
        );
    }
}

/// Prints `ninox send`'s receipt. A message held because its target is
/// unreachable exits 75 (EX_TEMPFAIL), so the sender sees it wasn't
/// delivered yet without treating it as lost; every other failure already
/// surfaced as an error.
fn print_send_receipt(receipt: &ninox_core::messaging::DeliveryReceipt, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string(receipt)?);
    } else {
        println!("{receipt}");
    }
    if matches!(receipt.status, ninox_core::messaging::DeliveryStatus::Unreachable { .. }) {
        std::process::exit(75);
    }
    Ok(())
}

/// `30m`/`2h`/`1d`/`45s` back from `now`, or an absolute epoch-ms value.
fn parse_since(since: &str, now: i64) -> anyhow::Result<i64> {
    let since = since.trim();
    if let Ok(ms) = since.parse::<i64>() {
        return Ok(ms);
    }
    let (num, unit) = since.split_at(since.find(|c: char| !c.is_ascii_digit()).unwrap_or(since.len()));
    let n: i64 = num.parse().map_err(|_| anyhow::anyhow!("--since: expected e.g. 30m, 2h, 1d or epoch ms, got {since:?}"))?;
    let unit_ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("--since: unknown unit {unit:?} (use s, m, h or d)"),
    };
    Ok(now - n * unit_ms)
}

fn run_messages(store: &Store, to: &str, since: Option<&str>, json: bool, now: i64) -> anyhow::Result<String> {
    let since_ms = parse_since(since.unwrap_or("1d"), now)?;
    let events = store.message_events_to(to, since_ms)?;
    let held = store.held_messages_for(to)?;
    if json {
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "to": to,
            "messages": events,
            "held": held.iter().map(|(at, text, from)| serde_json::json!({ "created_at": at, "text": text, "from": from })).collect::<Vec<_>>(),
        }))?);
    }
    if events.is_empty() && held.is_empty() {
        return Ok(format!("no messages to {to} in that window"));
    }
    let mut out = String::new();
    for e in &events {
        let age_min = (now - e.delivered_at) / 60_000;
        out.push_str(&format!(
            "#{} {age_min}m ago from {} [{}] {}:\n{}\n\n",
            e.id,
            e.from_id.as_deref().unwrap_or("(unverified)"),
            e.kind.tag().trim_matches(|c| c == '[' || c == ']'),
            e.outcome.as_str(),
            e.text.trim_end(),
        ));
    }
    for (at, text, from) in &held {
        out.push_str(&format!(
            "held {}m ago from {} (not delivered yet):\n{}\n\n",
            (now - at) / 60_000,
            from.as_deref().unwrap_or("(unverified)"),
            text.trim_end(),
        ));
    }
    Ok(out.trim_end().to_string())
}

fn default_db_path() -> PathBuf {
    ninox_core::AppConfig::default_db_path()
}

fn first_words(s: &str, n: usize) -> String {
    s.split_whitespace().take(n).collect::<Vec<_>>().join("-")
}

/// First non-empty line of `s`, trimmed and clipped to `max_chars` (with a
/// trailing "…" if truncated) — derives the fleet card summary from a
/// spawn prompt. `None` if `s` has no non-empty line.
pub(crate) fn first_line(s: &str, max_chars: usize) -> Option<String> {
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

#[cfg(test)]
mod brain_set_cli_tests {
    use super::{open_brain_reader, run_brain_add, run_brain_add_in_set, BrainReader};
    use ninox_core::brain_set::BrainSet;
    use ninox_core::config::{AppConfig, CatalogueRef};
    use ninox_core::QueryFilters;

    struct Brains {
        config: AppConfig,
        dir: tempfile::TempDir,
    }

    impl Brains {
        fn path(&self, n: &str) -> std::path::PathBuf {
            self.dir.path().join(n)
        }
    }

    /// Scratch catalogues: `a` (read-write), `b` (read-only), `c` (read-write).
    fn brains() -> Brains {
        let dir = tempfile::tempdir().unwrap();
        let cat = |name: &str, access: &str| CatalogueRef {
            name: name.into(), path: dir.path().join(name), remote: None, endpoint: None, region: None,
            cache_ttl_secs: None, enabled: true, cadence: "manual".into(), access: access.into(),
        };
        let mut config = AppConfig::default();
        config.brain.path = Some(dir.path().join("default"));
        config.brain.catalogues = vec![cat("a", "read_write"), cat("b", "read_only"), cat("c", "read_write")];
        Brains { config, dir }
    }

    fn entry(name: &str) -> String {
        format!("---\nname: {name}\nmetadata:\n  type: concept\n---\n\nshared widget notes for {name}.")
    }

    #[tokio::test]
    async fn query_fans_out_across_the_read_set_with_source_tags() {
        let _iso = ninox_core::test_isolation::isolate();
        let b = brains();
        run_brain_add(&b.path("a"), "concepts/alpha.md", &entry("alpha")).unwrap();
        run_brain_add(&b.path("b"), "concepts/beta.md", &entry("beta")).unwrap();
        run_brain_add(&b.path("c"), "concepts/gamma.md", &entry("gamma")).unwrap();
        let set = BrainSet::select(&b.config, &["a".into(), "b".into()], Some("a")).unwrap();

        let reader = open_brain_reader(&set, &b.config, None, None).await.unwrap();
        let mut ids: Vec<String> = reader.query("", None, QueryFilters::default()).unwrap().into_iter().map(|e| e.id).collect();
        ids.sort();
        assert_eq!(ids, vec!["a:concepts/alpha.md", "b:concepts/beta.md"], "c is outside the set");
        assert!(reader.get("concepts/beta.md").unwrap().is_some(), "a bare id resolves in whichever brain has it");

        let only_b = open_brain_reader(&set, &b.config, Some("b"), None).await.unwrap();
        let ids: Vec<String> = only_b.query("", None, QueryFilters::default()).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["b:concepts/beta.md"]);
        assert!(open_brain_reader(&set, &b.config, Some("c"), None).await.is_err(), "--catalogue can't reach outside the set");
    }

    #[tokio::test]
    async fn single_brain_set_reads_exactly_as_before() {
        let _iso = ninox_core::test_isolation::isolate();
        let b = brains();
        run_brain_add(&b.path("a"), "concepts/alpha.md", &entry("alpha")).unwrap();
        let reader = open_brain_reader(&BrainSet::single(b.path("a")), &b.config, None, None).await.unwrap();
        assert!(matches!(reader, BrainReader::Single(_)));
        let ids: Vec<String> = reader.query("", None, QueryFilters::default()).unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["concepts/alpha.md"], "unqualified ids, as today");
    }

    #[test]
    fn add_goes_to_the_write_catalogue_and_refuses_read_only_and_out_of_set() {
        let _iso = ninox_core::test_isolation::isolate();
        let b = brains();
        let set = BrainSet::select(&b.config, &["a".into(), "b".into()], Some("a")).unwrap();

        let (member, _) = run_brain_add_in_set(&set, &b.config, None, "concepts/x.md", &entry("x")).unwrap();
        assert_eq!(member.name(), "a");
        assert!(b.path("a").join("concepts/x.md").exists());

        let err = run_brain_add_in_set(&set, &b.config, Some("b"), "concepts/y.md", &entry("y")).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        assert!(!b.path("b").join("concepts/y.md").exists());

        let err = run_brain_add_in_set(&set, &b.config, Some("c"), "concepts/z.md", &entry("z")).unwrap_err();
        assert!(err.to_string().contains("not in this session's brain set"), "{err}");
        assert!(!b.path("c").join("concepts/z.md").exists());
    }
}

#[cfg(test)]
mod permission_mode_agreement_tests {
    use crate::session_queue::{OrchestratorLaunch, StandaloneLaunch, WorkerLaunch};
    use ninox_core::config::{AgentConfig, AppConfig};
    use ninox_core::harness::{LaunchKind, PermissionMode, CLAUDE_SKIP_PERMISSIONS_FLAG as SKIP};
    use ninox_core::store::Store;
    use ninox_core::types::Session;

    fn agent() -> AgentConfig {
        AgentConfig { harness: "claude-code".into(), model: Some("m".into()) }
    }

    fn worker(worker_type: &str) -> WorkerLaunch {
        WorkerLaunch {
            id: "w".into(), workspace: "/ws".into(), display_name: "w".into(), summary: None, prompt: "p".into(),
            orchestrator_id: Some("o".into()), agent: agent(), worker_type: Some(worker_type.into()),
            success_criteria: Vec::new(), started_at: 0, ninox_brain: None, ninox_config: None, brain: None,
        }
    }

    fn standalone(worker_type: &str) -> StandaloneLaunch {
        StandaloneLaunch {
            id: "s".into(), name: "s".into(), workspace_input: "/ws".into(), worker_type: worker_type.into(),
            agent: agent(), catalogue_path: "/b".into(), brain: None, prompt: None, task_brief: String::new(),
            success_criteria: Vec::new(), started_at: 0, isolate_worktree: false,
        }
    }

    fn orchestrator() -> OrchestratorLaunch {
        OrchestratorLaunch {
            id: "o".into(), name: "o".into(), prompt: None, parent_orchestrator_id: None, agent: agent(),
            catalogue_path: "/b".into(), brain: None, spawner: None, started_at: 0,
        }
    }

    /// Mirrors the launch halves (`launch_cli_worker` /
    /// `launch_orchestrator` / `launch_standalone_worker`, which every
    /// queued launch also runs): pick the fresh mode, build the fresh
    /// command, record the mode on the row. Then every relaunch path's plan
    /// (resume, and the re-file a fleet restore falls back to) must agree.
    fn assert_agrees(config: &AppConfig, store: &Store, kind: LaunchKind, skeleton: Session, is_orch: bool) -> PermissionMode {
        let mode = super::fresh_permission_mode(config, kind, &skeleton);
        let registry = config.registry();
        let fresh = match kind {
            LaunchKind::Worker => registry.worker_cmd(&skeleton_agent(&skeleton), mode, "p", "c").unwrap(),
            LaunchKind::Interactive => registry.interactive_cmd(&skeleton_agent(&skeleton), mode, "c"),
        };
        let row = Session { workspace_path: Some("/ws".into()), claude_session_id: Some("c".into()), ..skeleton };
        store.upsert_session(&row).unwrap();
        store.set_permission_mode(&row.id, mode).unwrap();
        let resume = crate::restart::resume_plan(store, &row, is_orch, config).unwrap();
        let refile = crate::restart::refile_plan(store, &row, is_orch, config, "c2").unwrap();
        for plan in [&resume, &refile] {
            assert_eq!(plan.permission_mode, mode, "{:?} {:?}", row.worker_type, kind);
            assert_eq!(plan.base_cmd.contains(SKIP), fresh.contains(SKIP), "fresh `{fresh}` vs relaunch `{}`", plan.base_cmd);
        }
        assert!(resume.base_cmd.contains("--resume"));
        mode
    }

    fn skeleton_agent(s: &Session) -> AgentConfig {
        AgentConfig { harness: s.agent_type.clone(), model: s.model.clone() }
    }

    #[test]
    fn fresh_and_relaunch_flags_agree_for_every_built_in_type_and_launch_kind() {
        let _iso = ninox_core::test_isolation::isolate();
        let mut config = AppConfig::default();
        let mut scoped = ninox_core::worker_type::WorkerTypeSpec { name: "Scoped".into(), ..Default::default() };
        scoped.permissions.enabled = true;
        config.worker_types.push(scoped);
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let types = config.worker_type_registry();
        let names = types.names();
        assert!(names.iter().any(|n| n == "Scoped") && names.len() >= 6, "{names:?}");
        for name in &names {
            let enforced = types.get(name).unwrap().permissions.enabled;
            let w = assert_agrees(&config, &store, LaunchKind::Worker, worker(name).skeleton(), false);
            assert_eq!(w, if enforced { PermissionMode::Standard } else { PermissionMode::Bypass }, "worker {name}");
            let s = assert_agrees(&config, &store, LaunchKind::Interactive, standalone(name).skeleton(), false);
            assert_eq!(s, PermissionMode::Standard, "standalone {name}");
        }
        let o = assert_agrees(&config, &store, LaunchKind::Interactive, orchestrator().skeleton(), true);
        assert_eq!(o, PermissionMode::Standard);
    }
}

#[cfg(test)]
mod brain_set_queue_tests {
    use crate::session_queue::{OrchestratorLaunch, QueuedLaunch, WorkerLaunch};
    use ninox_core::brain_set::BrainSet;

    fn worker(brain: Option<BrainSet>, ninox_brain: Option<&str>) -> WorkerLaunch {
        WorkerLaunch {
            id: "w".into(), workspace: "/ws".into(), display_name: "w".into(), summary: None, prompt: "p".into(),
            orchestrator_id: Some("o".into()), agent: ninox_core::config::AgentConfig { harness: "claude-code".into(), model: None }, worker_type: None,
            success_criteria: Vec::new(), started_at: 0, ninox_brain: ninox_brain.map(str::to_string),
            ninox_config: None, brain,
        }
    }

    #[test]
    fn queued_worker_keeps_its_brain_set_through_the_payload() {
        let set = BrainSet { reads: vec!["/b/a".into(), "/b/wiki".into()], write: "/b/a".into() };
        let payload = serde_json::to_string(&QueuedLaunch::CliWorker(worker(Some(set.clone()), Some("/b/a")))).unwrap();
        let QueuedLaunch::CliWorker(back) = serde_json::from_str(&payload).unwrap() else { panic!("kind") };
        let s = back.skeleton();
        assert_eq!(s.brain_reads, vec!["/b/a", "/b/wiki"]);
        assert_eq!(s.brain_write.as_deref(), Some("/b/a"));
    }

    /// A payload an older binary queued has no `brain`: its `ninox_brain`
    /// becomes a single-catalogue set.
    #[test]
    fn legacy_queued_worker_payload_becomes_a_single_brain_set() {
        let mut v = serde_json::to_value(QueuedLaunch::CliWorker(worker(None, Some("/b/old")))).unwrap();
        v.as_object_mut().unwrap().remove("brain");
        let QueuedLaunch::CliWorker(back) = serde_json::from_value(v).unwrap() else { panic!("kind") };
        assert_eq!(back.brain_set(), Some(BrainSet::single("/b/old")));
    }

    #[test]
    fn queued_orchestrator_dual_carries_catalogue_path_for_older_binaries() {
        let set = BrainSet { reads: vec!["/b/a".into(), "/b/wiki".into()], write: "/b/a".into() };
        let o = OrchestratorLaunch {
            id: "o".into(), name: "o".into(), prompt: None, parent_orchestrator_id: None,
            agent: ninox_core::config::AgentConfig { harness: "claude-code".into(), model: None },
            catalogue_path: set.write_str(), brain: Some(set.clone()),
            spawner: None, started_at: 0,
        };
        let v = serde_json::to_value(QueuedLaunch::Orchestrator(o)).unwrap();
        assert_eq!(v["catalogue_path"], "/b/a");
        let QueuedLaunch::Orchestrator(back) = serde_json::from_value(v).unwrap() else { panic!("kind") };
        assert_eq!(back.brain_set(), set);
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
    use std::time::Duration;

    /// Poll instead of asserting on a single call: `port_in_use`'s connect
    /// has a fixed 500ms timeout, which a `cargo test --workspace` run's
    /// CPU/socket contention can occasionally exceed even though the port's
    /// actual state already matches `expected` — not a port-reuse race.
    async fn wait_until_yields(act_as_daemon: bool, port: u16, expected: bool, what: &str) {
        for attempt in 0..20 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if super::daemon_should_yield(act_as_daemon, port).await == expected {
                return;
            }
        }
        panic!("{what} never reached {expected}");
    }

    #[tokio::test]
    async fn daemon_yields_only_when_the_port_is_held() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        wait_until_yields(true, port, true, "daemon_should_yield(true) while the port is held").await;
        assert!(!super::daemon_should_yield(false, port).await, "the GUI path shares the port");
        drop(listener);
        wait_until_yields(true, port, false, "daemon_should_yield(true) after the port was dropped").await;
    }
}

#[cfg(test)]
mod session_limit_spawn_tests {
    use super::{run_spawn_opts, CallerRuntime};
    use ninox_core::config::AppConfig;
    use ninox_core::store::{QueueKind, QueueState, Store};
    use ninox_core::types::SessionStatus;
    use std::sync::Arc;

    fn full_store(iso: &ninox_core::test_isolation::Isolation) -> Arc<Store> {
        AppConfig::update(|c| c.limits.set_limit(None, Some(1))).unwrap();
        let store = Arc::new(Store::open(iso.root().join("t.db")).unwrap());
        let mut busy = crate::session_queue::base_session("busy", "busy", &AppConfig::default().worker, 1);
        busy.status = SessionStatus::Working;
        store.upsert_session(&busy).unwrap();
        store
    }

    async fn spawn(store: &Arc<Store>, name: &str, workspace: &str, no_queue: bool) -> anyhow::Result<()> {
        run_spawn_opts(
            store.clone(), AppConfig::load_or_default(), "do the task".into(), workspace.into(),
            Some(name.into()), None, CallerRuntime { caller_type: None, pane: None }, None, None,
            vec![], false, no_queue, None, None,
        )
        .await
    }

    #[tokio::test]
    async fn over_the_limit_a_spawn_is_queued_with_no_side_effects() {
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        let repo = tempfile::tempdir().unwrap();
        spawn(&store, "queued-one", repo.path().to_str().unwrap(), false).await.unwrap();

        assert!(store.get_session("queued-one").unwrap().is_none(), "no sessions row until it starts");
        let entry = store.queue_entry("queued-one").unwrap().expect("queued");
        assert_eq!((entry.kind, entry.state), (QueueKind::Spawn, QueueState::Queued));
        let payload: crate::session_queue::QueuedLaunch = serde_json::from_str(&entry.payload).unwrap();
        match payload {
            crate::session_queue::QueuedLaunch::CliWorker(p) => {
                assert_eq!(p.workspace, repo.path().to_str().unwrap());
                assert!(p.prompt.contains("do the task"));
            }
            other => panic!("wrong payload: {other:?}"),
        }
        assert!(!repo.path().join(".claude").exists(), "no worktree is cut for a queued spawn");
        assert!(spawn(&store, "queued-one", repo.path().to_str().unwrap(), false).await.is_err(), "the id is taken while queued");
    }

    /// End to end on the isolated test tmux server, with a stand-in harness
    /// that draws a `❯` prompt and echoes input: a queued spawn starts once
    /// the busy session ends, and a message sent while it waited arrives.
    #[tokio::test]
    async fn a_queued_spawn_starts_when_a_slot_frees_and_gets_its_held_messages() {
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        AppConfig::update(|c| {
            c.harnesses.insert("queue-fake".into(), ninox_core::harness::HarnessSpec {
                enabled: true,
                binary: Some("sh".into()),
                worker_args: Some(vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{prompt}".into()]),
                ..Default::default()
            });
            c.worker.harness = "queue-fake".into();
            c.worker.model = None;
            c.set_send_mechanism(ninox_core::config::SendMechanism::Keystrokes);
        })
        .unwrap();
        let id = format!("queue-e2e-{}", std::process::id());
        let repo = tempfile::tempdir().unwrap();
        spawn(&store, &id, repo.path().to_str().unwrap(), false).await.unwrap();
        assert!(store.is_waiting_in_queue(&id).unwrap());

        ninox_core::messaging::deliver_message(
            &store, &AppConfig::sessions_dir(), &id, "hello-from-the-queue",
            ninox_core::config::SendMechanism::Keystrokes, None, None, ninox_core::types::MessageKind::Freeform,
        )
        .await
        .unwrap();
        assert_eq!(store.held_message_count(&id).unwrap(), 1);

        let mut busy = store.get_session("busy").unwrap().unwrap();
        busy.status = SessionStatus::Done;
        store.upsert_session(&busy).unwrap();
        let launcher = crate::session_queue::queue_launcher(store.clone());
        ninox_core::session_limits::drain(&store, &launcher, 1).await;

        // Arbitrary; a cold `sh` start plus a keystroke delivery is well under it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut screen = String::new();
        while std::time::Instant::now() < deadline {
            if store.queue_entry(&id).unwrap().is_none() && store.held_message_count(&id).unwrap() == 0 {
                screen = ninox_core::runtime::read_screen(&id, None, false).await.unwrap_or_default();
                if screen.contains("hello-from-the-queue") {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let row = store.get_session(&id).unwrap();
        let _ = ninox_core::tmux::kill_session(&id).await;
        assert_eq!(row.map(|s| s.status), Some(SessionStatus::Working), "started");
        assert!(screen.contains("hello-from-the-queue"), "held message delivered on start; screen:\n{screen}");
        assert_eq!(store.permission_mode(&id).unwrap(), Some(ninox_core::harness::PermissionMode::Bypass));
    }

    /// A queued spawn starts against the config of its start time: if its
    /// harness lost worker mode meanwhile, the launch fails cleanly and the
    /// slot comes back instead of panicking with it reserved.
    #[tokio::test]
    async fn a_queued_spawn_whose_harness_lost_worker_mode_gives_its_slot_back() {
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        let repo = tempfile::tempdir().unwrap();
        spawn(&store, "queued-gone", repo.path().to_str().unwrap(), false).await.unwrap();
        AppConfig::update(|c| {
            c.harnesses.insert("claude-code".into(), ninox_core::harness::HarnessSpec {
                enabled: true,
                binary: Some("true".into()),
                worker_args: None,
                ..Default::default()
            });
        })
        .unwrap();
        let mut busy = store.get_session("busy").unwrap().unwrap();
        busy.status = SessionStatus::Done;
        store.upsert_session(&busy).unwrap();
        let claimed = store.claim_queued(None, Some(1), 2).unwrap();
        assert_eq!(claimed.len(), 1);
        (crate::session_queue::queue_launcher(store.clone()))(claimed.into_iter().next().unwrap()).await;
        assert!(store.queue_entry("queued-gone").unwrap().is_none(), "slot released");
        assert!(store.get_session("queued-gone").unwrap().is_none(), "nothing half-started");
    }

    #[tokio::test]
    async fn an_orchestrator_spawn_is_admitted_at_the_limit_never_queued() {
        use crate::session_queue::{admit, Admit, OrchestratorLaunch, QueuedLaunch};
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        let launch = OrchestratorLaunch {
            id: "orch-at-limit".into(),
            name: "orch-at-limit".into(),
            prompt: None,
            parent_orchestrator_id: None,
            agent: AppConfig::default().orchestrator,
            catalogue_path: "/brain".into(),
            brain: None,
            spawner: None,
            started_at: 1,
        };
        let skeleton = launch.skeleton();
        let admitted = admit(&store, ninox_core::store::QueueKind::Spawn, &skeleton, &QueuedLaunch::Orchestrator(launch), true).unwrap();
        assert_eq!(admitted, Admit::Admitted, "the worker limit never blocks an orchestrator");
        assert_eq!(store.running_session_count(None).unwrap(), 1, "it reserves no worker slot");
    }

    fn interrupted(store: &Store, id: &str, harness: &str, ws: &str) -> ninox_core::types::Session {
        let mut s = crate::session_queue::base_session(id, id, &ninox_core::config::AgentConfig { harness: harness.into(), model: None }, 1);
        s.status = SessionStatus::Interrupted;
        s.workspace_path = Some(ws.into());
        s.claude_session_id = Some(format!("uuid-{id}"));
        store.upsert_session(&s).unwrap();
        s
    }

    /// Resuming a session that isn't running adds one and is gated;
    /// restarting a running one replaces itself and never is.
    #[test]
    fn resume_is_gated_only_for_a_session_that_is_not_running() {
        use crate::session_queue::{admit_resume, ResumeGate};
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        let ws = tempfile::tempdir().unwrap();
        let live = store.get_session("busy").unwrap().unwrap();
        assert_eq!(admit_resume(&store, &live, false, false).unwrap(), ResumeGate::NotGated);
        assert!(store.queue_entry("busy").unwrap().is_none(), "a restart in place reserves nothing");

        let down = interrupted(&store, "down", "claude-code", ws.path().to_str().unwrap());
        let err = admit_resume(&store, &down, false, true).unwrap_err().to_string();
        assert!(err.contains("--no-queue"), "{err}");
        assert!(store.queue_entry("down").unwrap().is_none());

        assert!(matches!(admit_resume(&store, &down, false, false).unwrap(), ResumeGate::Queued { .. }));
        let entry = store.queue_entry("down").unwrap().unwrap();
        assert_eq!((entry.kind, entry.state), (QueueKind::Relaunch, QueueState::Queued));
        assert!(matches!(
            serde_json::from_str::<crate::session_queue::QueuedLaunch>(&entry.payload).unwrap(),
            crate::session_queue::QueuedLaunch::Resume { is_orchestrator: false }
        ));
        assert_eq!(store.get_session("down").unwrap().unwrap().status, SessionStatus::Interrupted, "row untouched while queued");

        let mut busy = store.get_session("busy").unwrap().unwrap();
        busy.status = SessionStatus::Done;
        store.upsert_session(&busy).unwrap();
        let other = interrupted(&store, "other", "claude-code", ws.path().to_str().unwrap());
        assert!(matches!(admit_resume(&store, &other, false, false).unwrap(), ResumeGate::Queued { .. }), "behind the waiting one");
    }

    /// End to end on the isolated test tmux server: a queued Resume starts
    /// with `--resume` once a slot frees.
    #[tokio::test]
    async fn a_queued_resume_starts_when_a_slot_frees() {
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        AppConfig::update(|c| {
            c.harnesses.insert("resume-fake".into(), ninox_core::harness::HarnessSpec {
                enabled: true,
                binary: Some("sh".into()),
                resume_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into(), "x".into(), "{session_id}".into()],
                ..Default::default()
            });
        })
        .unwrap();
        let ws = tempfile::tempdir().unwrap();
        let id = format!("resume-e2e-{}", std::process::id());
        let down = interrupted(&store, &id, "resume-fake", ws.path().to_str().unwrap());
        let engine = ninox_core::events::Engine::new(store.clone());
        let outcome = crate::restart::resume_gated(engine, down, false, &AppConfig::load_or_default(), false, false).await.unwrap();
        assert!(matches!(outcome, crate::restart::ResumeOutcome::Queued(_)), "{outcome:?}");

        let mut busy = store.get_session("busy").unwrap().unwrap();
        busy.status = SessionStatus::Done;
        store.upsert_session(&busy).unwrap();
        let claimed = store.claim_queued(None, Some(1), 2).unwrap();
        assert_eq!(claimed.len(), 1);
        (crate::session_queue::queue_launcher(store.clone()))(claimed.into_iter().next().unwrap()).await;

        let live = ninox_core::runtime::has_session(&id).await;
        let row = store.get_session(&id).unwrap().unwrap();
        let _ = ninox_core::tmux::kill_session(&id).await;
        assert!(live, "resumed pane is up");
        assert_eq!(row.status, SessionStatus::Working);
        assert!(store.queue_entry(&id).unwrap().is_none(), "slot handed over to the running row");
    }

    #[tokio::test]
    async fn no_queue_fails_fast_and_leaves_nothing_behind() {
        let iso = ninox_core::test_isolation::isolate();
        let store = full_store(&iso);
        let repo = tempfile::tempdir().unwrap();
        let err = spawn(&store, "scripted", repo.path().to_str().unwrap(), true).await.unwrap_err().to_string();
        assert!(err.contains("--no-queue") && err.contains("1/1"), "{err}");
        assert!(store.queue_entry("scripted").unwrap().is_none());
        assert!(store.get_session("scripted").unwrap().is_none());
    }
}

#[cfg(test)]
mod worker_env_tests {
    use super::{CallerRuntime, first_line, first_words, run_spawn, worker_context_footer, worker_env_vars};

    #[tokio::test]
    async fn run_spawn_marks_the_session_terminated_when_tmux_create_fails() {
        let _iso = ninox_core::test_isolation::isolate();
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
            Some("ghost-spawn-test".into()),
            None,
            CallerRuntime { caller_type: None, pane: None },
            None,
            None,
            vec![],
            false,
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
    async fn run_spawn_with_known_type_resolves_harness_model_and_records_worker_type() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;
        use ninox_core::worker_type::WorkerTypeSpec;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        // `config.worker`'s own harness/model deliberately differ from the
        // named type's, so a correct resolution can only come from the
        // type, not from falling through to `config.worker`.
        let mut config = ninox_core::config::AppConfig::default();
        config.worker = ninox_core::config::AgentConfig {
            harness: "claude-code".into(),
            model: Some("claude-sonnet-5-5".into()),
        };
        config.worker_types = vec![WorkerTypeSpec {
            name: "Researcher".into(),
            brief_template: Some("Investigate: {prompt}".into()),
            harness: "codex".into(),
            model: Some("gpt-5".into()),
            ..WorkerTypeSpec::default()
        }];

        let result = run_spawn(
            store.clone(),
            config,
            "the flaky test".into(),
            "/definitely/not/a/real/dir".into(),
            Some("typed-spawn-test".into()),
            None,
            CallerRuntime { caller_type: None, pane: None },
            Some("Researcher".into()),
            None,
            vec![],
            false,
        )
        .await;
        // codex has no resume_args/worker_args issue here — the missing
        // workspace still fails deterministically at tmux creation, same as
        // the untyped ghost-spawn test above.
        assert!(result.is_err(), "spawn into a missing workspace must still fail");

        let session = store.get_session("typed-spawn-test").unwrap()
            .expect("the session row must be recorded before the unrelated tmux failure");
        assert_eq!(session.agent_type, "codex", "harness must come from the named type");
        assert_eq!(session.model.as_deref(), Some("gpt-5"), "model must come from the named type");
        assert_eq!(session.worker_type.as_deref(), Some("Researcher"));

        let facts = store.fleet_record("typed-spawn-test").unwrap().unwrap();
        assert_eq!(
            facts.task_brief.as_deref(), Some("Investigate: the flaky test"),
            "the type's brief template must compose with --prompt",
        );
    }

    #[tokio::test]
    async fn run_spawn_with_type_and_no_name_derives_the_session_name_from_the_raw_prompt() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;
        use ninox_core::worker_type::WorkerTypeSpec;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let config = ninox_core::config::AppConfig {
            worker_types: vec![WorkerTypeSpec {
                name: "Evaluator".into(),
                brief_template: Some("Evaluate and report PASS/FAIL: {prompt}".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let result = run_spawn(
            store.clone(),
            config,
            "fix the login bug".into(),
            "/definitely/not/a/real/dir".into(),
            None, // no --name: the session name must come from the raw prompt
            None,
            CallerRuntime { caller_type: None, pane: None },
            Some("Evaluator".into()),
            None,
            vec![],
            false,
        )
        .await;
        assert!(result.is_err(), "spawn into a missing workspace must still fail");

        // Every unnamed Evaluator spawn must NOT collapse to the same
        // template-boilerplate name ("Evaluate-and-report-PASS/FAIL:") —
        // the name and summary must reflect the actual task, not the type's
        // brief template.
        let sessions = store.list_sessions().unwrap();
        let session = sessions.iter().find(|s| s.worker_type.as_deref() == Some("Evaluator"))
            .expect("the typed session row must be recorded before the unrelated tmux failure");
        assert_eq!(first_words("fix the login bug", 4), session.name);
        assert_eq!(session.summary.as_deref(), Some("fix the login bug"));
    }

    #[tokio::test]
    async fn run_spawn_rejects_an_unknown_worker_type_before_any_side_effect() {
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "do the task".into(),
            "/some/dir".into(),
            Some("unknown-type-spawn-test".into()),
            None,
            CallerRuntime { caller_type: None, pane: None },
            Some("DoesNotExist".into()),
            None,
            vec![],
            false,
        )
        .await;

        let err = result.expect_err("an unrecognized --type name must be a clear error").to_string();
        assert!(err.contains("DoesNotExist"), "{err}");
        assert!(
            store.get_session("unknown-type-spawn-test").unwrap().is_none(),
            "an unrecognized --type must leave no session row behind — no silent fallback to config.worker",
        );
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
            brain_reads: Vec::new(), brain_write: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None,
            merged_at: Some(1_000),
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        store.upsert_session(&kept_alive).unwrap();

        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "follow-up task".into(),
            "/some/other/dir".into(),
            Some("ath-123".into()),
            None,
            CallerRuntime { caller_type: None, pane: None },
            None,
            None,
            vec![],
            false,
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

    #[tokio::test]
    async fn run_spawn_refuses_a_reserved_name() {
        use std::sync::Arc;
        // `messaging::HUMAN_SENDER`: a worker with that id could pass its own
        // messages off as typed by the user.
        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "do the task".into(),
            "/some/dir".into(),
            Some("ninox-user".into()),
            None,
            CallerRuntime { caller_type: None, pane: None },
            None,
            None,
            vec![],
            false,
        )
        .await;

        let err = result.expect_err("must refuse the reserved name").to_string();
        assert!(err.contains("reserved"), "{err}");
        assert!(store.get_session("ninox-user").unwrap().is_none());
    }

    /// The gap this change closes: a forged `--orchestrator-id` with no
    /// genuine runtime behind it (no live pane, no `NINOX_CALLER_TYPE`
    /// claim) must be rejected BEFORE any side effect — no worktree, no
    /// session row — same as the other pre-side-effect refusals above.
    #[tokio::test]
    async fn run_spawn_rejects_an_unowned_orchestrator_id_claim() {
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "do the task".into(),
            "/some/dir".into(),
            Some("forged-spawn-test".into()),
            Some("some-real-orchestrator".into()),
            CallerRuntime { caller_type: None, pane: None },
            None,
            None,
            vec![],
            false,
        )
        .await;

        let err = result.expect_err("an unverified orchestrator-id claim must be refused").to_string();
        assert!(err.contains("some-real-orchestrator"), "{err}");
        assert!(
            store.get_session("forged-spawn-test").unwrap().is_none(),
            "a rejected claim must leave no session row behind",
        );
    }

    /// The legitimate case this change must not regress: a real orchestrator
    /// — persisted in the store, with a genuine tmux-pane identity that
    /// self-registers on first use (the same migration path
    /// `authorize_orchestrator` documents) — can still spawn its own
    /// worker. `pane_pid` is the test process's own pid: `caller_descends_from`
    /// walks the calling process's OS ancestry, and a process always
    /// descends from itself, so this is a genuine (not mocked) pass through
    /// that check. The workspace is still missing, so the spawn fails for
    /// the same unrelated reason as `run_spawn_marks_the_session_terminated_
    /// when_tmux_create_fails` — proving the failure is NOT the orchestrator
    /// check (the error text differs, and the row is recorded with the
    /// claimed owner) while staying deterministic (no real tmux session).
    #[tokio::test]
    async fn run_spawn_accepts_a_genuinely_owned_orchestrator_id_claim() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: "real-orch".into(),
            name: "real-orch".into(),
            created_at: 0,
        }).unwrap();
        // `register_migrated_orchestrator_runtime`'s self-registration
        // (first-use bootstrap) requires the orchestrator's own session row
        // to already record `pid` as this exact pane's root pid — normally
        // set when the orchestrator's own session was spawned.
        store.upsert_session(&ninox_core::types::Session {
            id: "real-orch".into(), orchestrator_id: None, name: "real-orch".into(),
            repo: String::new(), status: ninox_core::SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None,
            pid: Some(std::process::id()), model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();
        let runtime = ninox_core::runtime::PaneIdentity {
            physical_tmux_name: "real-orch".into(),
            pane_id: "%1".into(),
            pane_pid: std::process::id(),
            pane_created_at: 0,
            server_epoch: "test-epoch".into(),
        };

        let result = run_spawn(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "do the task".into(),
            "/definitely/not/a/real/dir".into(),
            Some("legit-spawn-test".into()),
            Some("real-orch".into()),
            CallerRuntime { caller_type: Some("orchestrator".into()), pane: Some(runtime) },
            None,
            None,
            vec![],
            false,
        )
        .await;

        let err = result.expect_err("spawn into a missing workspace must still fail").to_string();
        assert!(
            !err.contains("could not be verified as that orchestrator"),
            "a genuinely-owned claim must not be rejected by the orchestrator check: {err}",
        );

        let session = store.get_session("legit-spawn-test").unwrap()
            .expect("a verified claim must still record the session before the unrelated tmux failure");
        assert_eq!(session.orchestrator_id.as_deref(), Some("real-orch"));
        assert!(matches!(session.status, ninox_core::SessionStatus::Terminated));
    }

    #[tokio::test]
    async fn run_spawn_with_no_type_records_implementor_but_keeps_config_worker_resolution() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let mut config = ninox_core::config::AppConfig::default();
        config.worker = ninox_core::config::AgentConfig { harness: "codex".into(), model: Some("o3".into()) };

        let _ = run_spawn(
            store.clone(), config, "do the task".into(), "/definitely/not/a/real/dir".into(),
            Some("untyped-spawn-test".into()), None, CallerRuntime { caller_type: None, pane: None },
            None, None, vec![], false,
        ).await;

        let session = store.get_session("untyped-spawn-test").unwrap().unwrap();
        assert_eq!(session.worker_type.as_deref(), Some("Implementor"), "an untyped spawn still records Implementor");
        assert_eq!(session.agent_type, "codex", "but harness/model must still come from config.worker, unaffected");
        assert_eq!(session.model.as_deref(), Some("o3"));
        let facts = store.fleet_record("untyped-spawn-test").unwrap().unwrap();
        assert_eq!(facts.task_brief.as_deref(), Some("do the task"), "no brief template applied to an untyped spawn");
    }

    #[tokio::test]
    async fn run_spawn_rejects_orchestrator_as_a_requested_type() {
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let result = run_spawn(
            store.clone(), ninox_core::config::AppConfig::default(), "do the task".into(), "/some/dir".into(),
            Some("orch-type-spawn-test".into()), None, CallerRuntime { caller_type: None, pane: None },
            Some("Orchestrator".into()), None, vec![], false,
        ).await;

        let err = result.expect_err("spawning Orchestrator through `ninox spawn` must be refused").to_string();
        assert!(err.contains("spawn-orchestrator"), "{err}");
        assert!(store.get_session("orch-type-spawn-test").unwrap().is_none());
    }

    #[tokio::test]
    async fn ninox_is_never_spawnable_even_by_a_wildcard_type_or_a_hostile_override() {
        use std::sync::Arc;
        use ninox_core::worker_type::{WorkerTypeSpec, PROMPT_DISPLAY_NAME, PROMPT_WORKER_TYPE};
        let _iso = ninox_core::test_isolation::isolate();
        let store = Arc::new(ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut caller = crate::session_queue::base_session("star-caller", "star-caller", &ninox_core::config::AgentConfig::default(), 1);
        caller.status = ninox_core::types::SessionStatus::Working;
        caller.worker_type = Some("Star".into());
        store.upsert_session(&caller).unwrap();
        let config = ninox_core::config::AppConfig {
            worker_types: vec![
                WorkerTypeSpec { name: PROMPT_WORKER_TYPE.into(), hidden: false, ..Default::default() },
                WorkerTypeSpec { name: "Star".into(), can_spawn: vec!["*".into()], ..Default::default() },
            ],
            ..Default::default()
        };
        for (caller_id, name) in [(None, PROMPT_DISPLAY_NAME), (None, PROMPT_WORKER_TYPE), (Some("star-caller"), PROMPT_DISPLAY_NAME)] {
            let result = run_spawn(
                store.clone(), config.clone(), "t".into(), "/some/dir".into(),
                Some("ninox-spawn-attempt".into()), None, CallerRuntime { caller_type: None, pane: None },
                Some(name.into()), caller_id.map(String::from), vec![], false,
            ).await;
            let err = result.expect_err("the Ninox type must never be spawnable").to_string();
            assert!(err.contains("system type"), "{name}: {err}");
        }
        assert!(store.get_session("ninox-spawn-attempt").unwrap().is_none());
    }

    #[tokio::test]
    async fn run_spawn_rejects_a_hidden_type_regardless_of_caller() {
        use std::sync::Arc;
        use ninox_core::worker_type::WorkerTypeSpec;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let config = ninox_core::config::AppConfig {
            worker_types: vec![WorkerTypeSpec { name: "prompt".into(), hidden: true, ..Default::default() }],
            ..Default::default()
        };
        // No calling session at all — the most permissive caller there is —
        // still must not be able to spawn a hidden/system type.
        let result = run_spawn(
            store.clone(), config, "do the task".into(), "/some/dir".into(),
            Some("hidden-type-spawn-test".into()), None, CallerRuntime { caller_type: None, pane: None },
            Some("prompt".into()), None, vec![], false,
        ).await;

        let err = result.expect_err("a hidden type must never be spawnable via `ninox spawn`").to_string();
        assert!(err.contains("system type"), "{err}");
        assert!(store.get_session("hidden-type-spawn-test").unwrap().is_none());
    }

    #[tokio::test]
    async fn run_spawn_with_no_calling_session_is_unrestricted() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        let result = run_spawn(
            store.clone(), ninox_core::config::AppConfig::default(), "evaluate it".into(), "/definitely/not/a/real/dir".into(),
            Some("bare-human-spawn-test".into()), None, CallerRuntime { caller_type: None, pane: None },
            Some("Evaluator".into()), None, vec![], false,
        ).await;
        assert!(result.is_err(), "spawn into a missing workspace must still fail, but not on a can_spawn check");
        assert!(!result.unwrap_err().to_string().contains("can_spawn"));
        assert_eq!(
            store.get_session("bare-human-spawn-test").unwrap().unwrap().worker_type.as_deref(),
            Some("Evaluator"),
        );
    }

    /// `resolve_caller_worker_type` must key an untyped session's type off
    /// `store.is_orchestrator`, never off the `worker_type` column — every
    /// existing orchestrator predates that column. An orchestrator session
    /// with no `worker_type` set must still resolve to `Orchestrator` (not
    /// the `Implementor` fallback), and so be able to spawn anything.
    #[tokio::test]
    async fn run_spawn_untyped_orchestrator_caller_can_spawn_implementor() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: "untyped-orch".into(), name: "untyped-orch".into(), created_at: 0,
        }).unwrap();
        // Deliberately no `worker_type` on the orchestrator's own session
        // row — mirrors every orchestrator spawned before this feature.

        let result = run_spawn(
            store.clone(), ninox_core::config::AppConfig::default(), "do the task".into(), "/definitely/not/a/real/dir".into(),
            Some("untyped-orch-spawns".into()), None, CallerRuntime { caller_type: None, pane: None },
            None, Some("untyped-orch".into()), vec![], false,
        ).await;
        assert!(result.is_err(), "spawn into a missing workspace must still fail, but not on a can_spawn check");
        assert!(!result.unwrap_err().to_string().contains("can_spawn"));
    }

    /// The brief's core new behavior: a non-orchestrator caller whose type
    /// grants `can_spawn` for the requested type succeeds, and the new
    /// session is parented to the CALLER itself — not to whatever
    /// `NINOX_ORCHESTRATOR_ID` it happened to inherit from its own parent
    /// (simulated here by also passing an unrelated `orchestrator_id`
    /// claim, which must be ignored rather than rejected).
    #[tokio::test]
    async fn implementor_caller_spawns_researcher_parented_to_itself_ignoring_inherited_orchestrator_id() {
        let _iso = ninox_core::test_isolation::isolate();
        use std::sync::Arc;
        use ninox_core::types::{Session, SessionStatus};

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        store.upsert_session(&Session {
            id: "impl-1".into(), orchestrator_id: Some("grandparent-orch".into()), name: "impl-1".into(),
            repo: String::new(), status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: None, model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None, machine_id: None,
            worker_type: Some("Implementor".into()),
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();

        let result = run_spawn(
            store.clone(), ninox_core::config::AppConfig::default(), "look into the flake".into(),
            "/definitely/not/a/real/dir".into(), Some("sub-researcher".into()),
            // Inherited from impl-1's own env — must be ignored, not
            // rejected, since impl-1 never claims to BE grandparent-orch.
            Some("grandparent-orch".into()),
            CallerRuntime { caller_type: None, pane: None },
            Some("Researcher".into()), Some("impl-1".into()), vec![], false,
        ).await;
        assert!(result.is_err(), "spawn into a missing workspace must still fail, but not on authorization");
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("could not be verified as that orchestrator"), "{err}");
        assert!(!err.contains("can_spawn"), "{err}");

        let session = store.get_session("sub-researcher").unwrap()
            .expect("the session row must be recorded before the unrelated tmux failure");
        assert_eq!(
            session.orchestrator_id.as_deref(), Some("impl-1"),
            "a non-orchestrator caller's spawn must be parented to the caller itself",
        );
    }

    #[tokio::test]
    async fn implementor_caller_denied_spawning_evaluator() {
        use std::sync::Arc;
        use ninox_core::types::{Session, SessionStatus};

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        store.upsert_session(&Session {
            id: "impl-2".into(), orchestrator_id: None, name: "impl-2".into(),
            repo: String::new(), status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: None, model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None, machine_id: None,
            worker_type: Some("Implementor".into()),
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();

        let result = run_spawn(
            store.clone(), ninox_core::config::AppConfig::default(), "evaluate this".into(), "/some/dir".into(),
            Some("denied-evaluator-spawn".into()), None, CallerRuntime { caller_type: None, pane: None },
            Some("Evaluator".into()), Some("impl-2".into()), vec![], false,
        ).await;

        let err = result.expect_err("Implementor's can_spawn does not include Evaluator").to_string();
        assert!(err.contains("Implementor"), "{err}");
        assert!(err.contains("Evaluator"), "{err}");
        assert!(store.get_session("denied-evaluator-spawn").unwrap().is_none());
    }

    #[tokio::test]
    async fn researcher_caller_denied_spawning_anything() {
        use std::sync::Arc;
        use ninox_core::types::{Session, SessionStatus};

        let store = Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        );
        store.upsert_session(&Session {
            id: "researcher-1".into(), orchestrator_id: None, name: "researcher-1".into(),
            repo: String::new(), status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: None, model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None, machine_id: None,
            worker_type: Some("Researcher".into()),
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();

        // Even a bare, untyped spawn (which defaults to requesting
        // Implementor) must be denied — a Researcher's can_spawn is empty.
        let result = run_spawn(
            store.clone(), ninox_core::config::AppConfig::default(), "do more research".into(), "/some/dir".into(),
            Some("denied-untyped-spawn".into()), None, CallerRuntime { caller_type: None, pane: None },
            None, Some("researcher-1".into()), vec![], false,
        ).await;

        let err = result.expect_err("Researcher's can_spawn is empty").to_string();
        assert!(err.contains("Researcher"), "{err}");
        assert!(err.contains("none"), "{err}");
        assert!(store.get_session("denied-untyped-spawn").unwrap().is_none());
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
        let footer = worker_context_footer("w1", "orch1", false);
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
    fn worker_footer_omits_pr_watch_instruction_when_disabled() {
        let footer = worker_context_footer("w1", "orch1", false);
        assert!(!footer.contains("ninox open --pr"));
        assert!(!footer.contains("ninox close --pr"));
    }

    #[test]
    fn worker_footer_appends_pr_watch_instruction_when_enabled() {
        let footer = worker_context_footer("w1", "orch1", true);
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
            let footer = worker_context_footer("w1", "orch1", pr_watch);
            assert!(
                footer.contains("ninox capabilities --worker"),
                "worker-scoped capabilities bootstrap line must be present with pr_watch={pr_watch}"
            );
        }
    }

    #[test]
    fn forwards_brain_and_config_when_present() {
        let env = worker_env_vars("w1", "/data", "orch1", Some("/brain.db"), Some(r#"["/brain.db"]"#), Some("/cfg.toml"));
        assert!(env.contains(&("NINOX_ORCHESTRATOR_ID", "orch1")));
        assert!(env.contains(&("NINOX_BRAIN", "/brain.db")));
        assert!(env.contains(&("NINOX_CONFIG", "/cfg.toml")));
        assert!(env.contains(&("NINOX_SESSION", "w1")));
        assert!(env.contains(&("NINOX_DATA_DIR", "/data")));
        // Workers are the usual target of orchestrator messages; without
        // this they come up with no messaging socket and the configured
        // send mechanism silently degrades. See
        // `ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV`.
        assert!(env.contains(&(ninox_core::session_socket::CLAUDE_MESSAGING_GATE_ENV, "1")));
        // The legacy ATHENE_* transition names are gone.
        assert!(!env.iter().any(|(k, _)| k.starts_with("ATHENE_")));
        // No `NINOX_CALLER_TYPE` here is deliberate and load-bearing for
        // `orchestrator_auth::authorize_nested_spawn`, which treats "not
        // `orchestrator`" (including entirely absent) as the worker branch —
        // matching this codebase's existing convention that a worker's env
        // never carries this var at all (see `caller_is_orchestrator`'s doc
        // comment above), rather than inventing a new explicit "worker"
        // value nothing else checks for.
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_CALLER_TYPE"));
    }

    #[test]
    fn omits_brain_config_and_orchestrator_id_when_absent() {
        let env = worker_env_vars("w1", "/data", "", None, None, None);
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_ORCHESTRATOR_ID"));
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_BRAIN"));
        assert!(!env.iter().any(|(k, _)| *k == "NINOX_CONFIG"));
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
mod whoami_cli_tests {
    use super::*;
    use ninox_core::types::{Orchestrator, Session, SessionStatus};

    fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    fn seed(store: &Store, id: &str, orchestrator_id: Option<&str>, status: SessionStatus) {
        store.upsert_session(&Session {
            id: id.into(), orchestrator_id: orchestrator_id.map(String::from), name: id.into(),
            repo: "o/r".into(), status,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None,
            workspace_path: Some(format!("/work/{id}")), pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();
    }

    #[tokio::test]
    async fn text_output_describes_a_known_worker() {
        let (_d, store) = tmp_store();
        seed(&store, "worker-1", Some("orch-1"), SessionStatus::Working);
        let out = run_whoami(&store, &AppConfig::default(), Some("worker-1"), None, None, false).await.unwrap();
        assert!(out.contains("worker-1"));
        assert!(out.contains("orch-1"));
    }

    #[tokio::test]
    async fn json_output_describes_a_known_worker() {
        let (_d, store) = tmp_store();
        seed(&store, "worker-1", Some("orch-1"), SessionStatus::Working);
        let out = run_whoami(&store, &AppConfig::default(), Some("worker-1"), None, None, true).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["managed"], true);
        assert_eq!(parsed["sessionId"], "worker-1");
        assert_eq!(parsed["kind"], "worker");
        assert_eq!(parsed["owner"], "orch-1");
    }

    /// Without a verified orchestrator runtime (there is none in a bare test
    /// process — see `ninox_core::identity::resolve_for_caller`), `ninox
    /// whoami` must not disclose an orchestrator's identity just because the
    /// env var names its row. This is the fix for the review finding that a
    /// worker could set `NINOX_SESSION=$NINOX_ORCHESTRATOR_ID` and read its
    /// orchestrator's full identity.
    #[tokio::test]
    async fn orchestrator_identity_is_not_disclosed_without_a_verified_runtime() {
        let (_d, store) = tmp_store();
        seed(&store, "orch-1", None, SessionStatus::Working);
        store.upsert_orchestrator(&Orchestrator { id: "orch-1".into(), name: "orch-1".into(), created_at: 0 }).unwrap();
        let out = run_whoami(&store, &AppConfig::default(), Some("orch-1"), None, None, true).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["managed"], false, "an unverified caller must never see the orchestrator's identity");
    }

    #[tokio::test]
    async fn unknown_session_reports_not_managed_explicitly() {
        let (_d, store) = tmp_store();
        let out = run_whoami(&store, &AppConfig::default(), Some("ghost"), None, None, false).await.unwrap();
        assert!(out.contains("not managing"));
        assert!(out.contains("ghost"));

        let out_json = run_whoami(&store, &AppConfig::default(), Some("ghost"), None, None, true).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(parsed["managed"], false);
        assert_eq!(parsed["reason"], "unmanaged");
    }

    /// `run_identity_hook` (the `UserPromptSubmit`/`SessionStart` entry
    /// point) reads env/cwd itself and prints straight to stdout, so it
    /// isn't unit-tested directly here — same as `run_inbox`. What's
    /// exercised directly is the reminder it builds from, confirming it's
    /// never empty for a resolvable session.
    #[tokio::test]
    async fn identity_reminder_building_block_is_never_empty_for_a_known_session() {
        let (_d, store) = tmp_store();
        seed(&store, "worker-1", Some("orch-1"), SessionStatus::Working);
        let resolution = ninox_core::identity::resolve_for_caller(
            &store, &AppConfig::default(), Some("worker-1"), None, None,
        ).await.unwrap();
        let reminder = ninox_core::identity::render_reminder(&resolution);
        assert!(!reminder.is_empty());
        assert!(reminder.contains("worker-1"));
    }
}

#[cfg(test)]
mod user_request_cli_tests {
    use super::*;
    use ninox_core::types::UserRequestStatus;
    use std::time::Duration;

    fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn user_request_ask_wait_status_parse() {
        assert!(Args::try_parse_from(["ninox", "user-request", "ask", "proceed?"]).is_ok());
        assert!(Args::try_parse_from(["ninox", "user-request", "wait", "ur-1"]).is_ok());
        assert!(Args::try_parse_from(["ninox", "user-request", "status", "ur-1"]).is_ok());
    }

    #[test]
    fn user_request_has_no_answer_like_subcommand() {
        for bad in ["answer", "respond", "reply", "set-answer"] {
            let argv = ["ninox", "user-request", bad, "ur-1", "yes"];
            assert!(Args::try_parse_from(argv).is_err(), "{bad} must not parse");
        }
    }

    // `ask_resolves_requester_session_server_side_not_from_argv` from the
    // brief is a design constraint, not a runtime test: `UserRequestCommand::Ask`
    // (above) has no `--from`/`--session` field, and `run_user_request_ask`
    // takes the requester's session id as a plain function parameter —
    // resolved server-side in `dispatch()` via `worker_status::resolve_session_id`,
    // never parsed from argv — confirmed by inspection, mirroring `ninox
    // send`'s posture (cli.rs:908-918).

    #[test]
    fn ask_timeout_leaves_the_row_pending() {
        let (_d, store) = tmp_store();
        let err = run_user_request_ask(&store, "worker-1", "proceed?", false, Duration::from_secs(1))
            .expect_err("nobody answers this request — it must time out");
        assert!(err.to_string().contains("timed out"), "{err}");

        let pending = store.open_user_requests(Some("worker-1")).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, UserRequestStatus::Pending, "a timeout must never touch the row");
        assert!(pending[0].answer.is_none());
    }

    #[test]
    fn ask_async_returns_the_id_immediately_without_polling() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();
        assert!(id.starts_with("ur-"), "{id}");
        let row = store.get_user_request(&id).unwrap().unwrap();
        assert_eq!(row.status, UserRequestStatus::Pending);
    }

    #[test]
    fn wait_returns_the_answer_once_present() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();

        ninox_core::human_console_auth::register_human_console(&store, "ninox-gui").unwrap();
        let answering_store = Store::open(_d.path().join("t.db")).unwrap();
        let answer_id = id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            answering_store.answer_user_request(&answer_id, "go ahead").unwrap();
        });

        let out = run_user_request_wait(&store, "worker-1", &id, Duration::from_secs(5)).unwrap();
        assert!(out.contains("go ahead"), "{out}");
    }

    #[test]
    fn unresolved_user_requests_for_briefing_reflects_whether_the_answer_was_ever_read() {
        let (_d, store) = tmp_store();
        ninox_core::human_console_auth::register_human_console(&store, "ninox-gui").unwrap();

        // A sync request answered while its process never came back to
        // read it (the crash-mid-`wait` scenario the spec cares about) —
        // `answer_delivered_at` was never stamped, so it stays unresolved
        // for a fleet-recovery briefing.
        let crashed = ninox_core::store::UserRequestRow {
            id: "ur-crashed".into(), requester_session_id: "worker-1".into(),
            prompt: "proceed?".into(), mode: ninox_core::types::UserRequestMode::Sync,
            status: UserRequestStatus::Pending, answer: None, answered_at: None,
            answer_delivered_at: None, created_at: 1,
        };
        store.insert_user_request(&crashed).unwrap();
        store.answer_user_request("ur-crashed", "go ahead").unwrap();

        // A sync request successfully read back via `wait` — `wait`'s
        // success path stamps `answer_delivered_at`, so it's resolved and
        // must not reappear.
        let read_row = ninox_core::store::UserRequestRow {
            id: "ur-read".into(), requester_session_id: "worker-1".into(),
            prompt: "which env?".into(), mode: ninox_core::types::UserRequestMode::Sync,
            status: UserRequestStatus::Pending, answer: None, answered_at: None,
            answer_delivered_at: None, created_at: 2,
        };
        store.insert_user_request(&read_row).unwrap();
        let answering_store = Store::open(_d.path().join("t.db")).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            answering_store.answer_user_request("ur-read", "staging").unwrap();
        });
        let out = run_user_request_wait(&store, "worker-1", "ur-read", Duration::from_secs(5)).unwrap();
        assert!(out.contains("staging"), "{out}");

        let ids: Vec<_> = store.unresolved_user_requests_for_briefing(Some("worker-1")).unwrap()
            .into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["ur-crashed"]);
    }

    #[test]
    fn wait_times_out_against_an_existing_unanswered_id() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();
        let err = run_user_request_wait(&store, "worker-1", &id, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn status_reports_pending_then_answered() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();

        let pending = run_user_request_status(&store, &id).unwrap();
        assert!(pending.contains("pending"), "{pending}");

        ninox_core::human_console_auth::register_human_console(&store, "ninox-gui").unwrap();
        store.answer_user_request(&id, "go ahead").unwrap();
        let answered = run_user_request_status(&store, &id).unwrap();
        assert!(answered.contains("go ahead"), "{answered}");
    }

    #[test]
    fn wait_and_sync_ask_stop_promptly_on_a_dismissed_request() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();
        store.mark_user_request_cancelled(&id).unwrap();

        let started = std::time::Instant::now();
        let err = run_user_request_wait(&store, "worker-1", &id, Duration::from_secs(30)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "a dismissed row must not wait out the timeout");
        let msg = err.to_string();
        assert!(msg.contains("dismissed") && msg.contains("re-ask"), "{msg}");
        assert!(!msg.contains("timed out"), "{msg}");

        // Sync `ask` shares the poll loop: dismissing it mid-wait ends it too.
        let canceller = Store::open(_d.path().join("t.db")).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            for r in canceller.open_user_requests(Some("worker-2")).unwrap() {
                canceller.mark_user_request_cancelled(&r.id).unwrap();
            }
        });
        let started = std::time::Instant::now();
        let err = run_user_request_ask(&store, "worker-2", "still there?", false, Duration::from_secs(30)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(err.to_string().contains("dismissed"), "{err}");
    }

    #[test]
    fn status_reports_a_dismissed_request_as_cancelled() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();
        store.mark_user_request_cancelled(&id).unwrap();

        let out = run_user_request_status(&store, &id).unwrap();
        assert!(out.starts_with(&format!("{id}: cancelled")), "{out}");
        assert!(out.contains("re-ask"), "{out}");
    }

    #[test]
    fn wait_refuses_a_request_belonging_to_another_session_without_consuming_it() {
        let (_d, store) = tmp_store();
        let id = run_user_request_ask(&store, "worker-1", "proceed?", true, Duration::from_secs(240)).unwrap();
        ninox_core::human_console_auth::register_human_console(&store, "ninox-gui").unwrap();
        store.answer_user_request(&id, "go ahead").unwrap();

        let err = run_user_request_wait(&store, "orchestrator-1", &id, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("belongs to a different session"), "{err}");
        assert!(
            store.get_user_request(&id).unwrap().unwrap().answer_delivered_at.is_none(),
            "a refused wait must leave the answer for its real asker",
        );

        let out = run_user_request_wait(&store, "worker-1", &id, Duration::from_secs(1)).unwrap();
        assert!(out.contains("go ahead"), "{out}");
    }

    #[test]
    fn status_rejects_an_unknown_id() {
        let (_d, store) = tmp_store();
        assert!(run_user_request_status(&store, "ur-ghost").is_err());
    }
}

#[cfg(test)]
mod orchestrator_cli_tests {
    use super::{
        caller_is_orchestrator, orchestrator_context_footer, orchestrator_env_vars,
        reap_report_line, resolve_reap_orchestrator, run_spawn_orchestrator,
        run_spawn_sub_orchestrator, spawn_orchestrator_common, spawn_standalone_worker, CallerRuntime,
    };
    use ninox_core::events::ReapOutcome;
    use std::sync::Arc;

    fn store() -> Arc<ninox_core::store::Store> {
        Arc::new(
            ninox_core::store::Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap(),
        )
    }

    fn pane_for(id: &str) -> ninox_core::runtime::PaneIdentity {
        ninox_core::runtime::PaneIdentity {
            physical_tmux_name: id.to_string(),
            pane_id: format!("%{id}"),
            pane_pid: std::process::id(),
            pane_created_at: 0,
            server_epoch: "test-epoch".to_string(),
        }
    }

    /// A bare session row, as every other field-complete test literal in
    /// this module builds one — factored here since Task 5's tests need
    /// several (an orchestrator chain plus worker rows).
    fn bare_session(id: &str, orchestrator_id: Option<&str>) -> ninox_core::types::Session {
        ninox_core::types::Session {
            id: id.to_string(), orchestrator_id: orchestrator_id.map(str::to_string),
            name: id.to_string(), repo: String::new(), status: ninox_core::SessionStatus::Working,
            agent_type: "claude-code".to_string(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: Some(std::process::id()),
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    /// Persists `id` as a (possibly nested) orchestrator whose live pane
    /// identity will match `pane_for(id)` — same self-registration shape
    /// `authorize_orchestrator`'s own tests rely on.
    fn register_orchestrator_for_depth_test(store: &ninox_core::store::Store, id: &str, parent: Option<&str>) {
        store.upsert_orchestrator(&ninox_core::types::Orchestrator {
            id: id.to_string(), name: id.to_string(), created_at: 0,
        }).unwrap();
        store.upsert_session(&bare_session(id, parent)).unwrap();
    }

    /// An `AppConfig` whose `orchestrator_root` is a fresh tempdir — every
    /// test that spawns an orchestrator must use this (never the real
    /// default `~/ninox/orchestrators`, which is live, shared state).
    fn scratch_config() -> ninox_core::config::AppConfig {
        ninox_core::config::AppConfig {
            orchestrator_root: Some(tempfile::tempdir().unwrap().keep()),
            ..Default::default()
        }
    }

    /// Pre-creates a real tmux session under `id` so `spawn_orchestrator_
    /// common`'s own `tmux::create_session` call fails deterministically
    /// ("duplicate session") instead of either silently succeeding (if tmux
    /// is actually installed and working, as it is in this environment) or
    /// silently failing for an unrelated reason — either of which would let
    /// a test's assertion pass without checking anything. Caller must kill
    /// the session afterward. Session names here are prefixed to make an
    /// accidental collision with a real user session implausible.
    async fn occupy_tmux_session(id: &str) {
        let scratch = tempfile::tempdir().unwrap();
        let ws = scratch.path().join("occupied");
        std::fs::create_dir_all(&ws).unwrap();
        std::mem::forget(scratch); // keep the workspace dir alive for tmux's lifetime
        ninox_core::tmux::create_session(id, ws.to_str().unwrap(), "sleep 30", &[]).await.unwrap();
    }

    // `spawn_orchestrator_common` rolls BOTH rows back on any
    // `tmux::create_session` failure (unlike `run_spawn`'s worker path,
    // which marks the row Terminated instead of deleting it — see the doc
    // comment above the rollback in `spawn_orchestrator_common` itself), so
    // forcing a duplicate-session failure (as the other tests in this file
    // do) would leave nothing to assert on here. These two tests instead
    // rely on a genuine, tmux-enabled success — the same assumption
    // `failed_spawn_orchestrator_rolls_back_both_rows_so_the_name_is_
    // reusable` already makes by calling `tmux::create_session` for real —
    // and kill the real session immediately after asserting.
    #[tokio::test]
    async fn spawn_orchestrator_common_records_the_given_parent() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();

        let result = spawn_orchestrator_common(
            &store, &scratch_config(), "nspawn-test-records-parent", None, Some("parent-orch"), None, None,
        ).await;

        if let Ok(spawned) = &result {
            ninox_core::tmux::kill_session(&spawned.id).await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let session = store.get_session("nspawn-test-records-parent").unwrap()
            .expect("a successful spawn must record a session row");
        assert_eq!(session.orchestrator_id.as_deref(), Some("parent-orch"));
    }

    #[tokio::test]
    async fn spawn_orchestrator_common_still_defaults_to_no_parent() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();

        let result = spawn_orchestrator_common(
            &store, &scratch_config(), "nspawn-test-defaults-no-parent", None, None, None, None,
        ).await;

        if let Ok(spawned) = &result {
            ninox_core::tmux::kill_session(&spawned.id).await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let session = store.get_session("nspawn-test-defaults-no-parent").unwrap()
            .expect("a successful spawn must record a session row");
        assert_eq!(session.orchestrator_id, None);
    }

    #[tokio::test]
    async fn spawn_orchestrator_common_applies_agent_and_catalogue_overrides() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let agent = ninox_core::config::AgentConfig { harness: "codex".to_string(), model: Some("o3".to_string()) };

        let result = spawn_orchestrator_common(
            &store, &scratch_config(), "nspawn-test-overrides", None, None,
            Some(agent), Some(ninox_core::brain_set::BrainSet::single("/tmp/other-brain")),
        ).await;

        if let Ok(spawned) = &result {
            ninox_core::tmux::kill_session(&spawned.id).await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let session = store.get_session("nspawn-test-overrides").unwrap()
            .expect("a successful spawn must record a session row");
        assert_eq!(session.agent_type, "codex");
        assert_eq!(session.model.as_deref(), Some("o3"));
        assert_eq!(session.brain_write.as_deref(), Some("/tmp/other-brain"));
        assert_eq!(session.brain_reads, vec!["/tmp/other-brain"]);
    }

    fn default_agent() -> ninox_core::config::AgentConfig {
        ninox_core::config::AgentConfig { harness: "claude-code".to_string(), model: None }
    }

    fn existing_workspace() -> String {
        tempfile::tempdir().unwrap().keep().to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn spawn_standalone_worker_rejects_hidden_or_orchestrator_type_before_any_side_effect() {
        let store = store();
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-rejects-orch".into(), existing_workspace(),
            Some("Orchestrator".into()), default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        assert!(result.is_err());
        assert!(store.get_session("standalone-rejects-orch").unwrap().is_none());
    }

    #[tokio::test]
    async fn spawn_standalone_worker_rejects_unknown_type() {
        let store = store();
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-unknown-type".into(), existing_workspace(),
            Some("NoSuchType".into()), default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        let err = result.expect_err("an unknown type name must be rejected").to_string();
        assert!(err.contains("unknown worker type"), "unexpected error: {err}");
        assert!(store.get_session("standalone-unknown-type").unwrap().is_none());
    }

    #[tokio::test]
    async fn spawn_standalone_worker_rejects_duplicate_session_name() {
        let store = store();
        store.upsert_session(&bare_session("standalone-dup", None)).unwrap();
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-dup".into(), existing_workspace(),
            None, default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        assert!(result.expect_err("a duplicate session id must be rejected").to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn spawn_standalone_worker_rejects_a_missing_workspace_with_no_repo() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let missing = tempfile::tempdir().unwrap().keep().join("nested").join("does-not-exist");
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-bad-ws".into(), missing.to_string_lossy().to_string(),
            None, default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        assert!(result.expect_err("a missing, repo-less workspace must be rejected").to_string().contains("does not exist"));
        assert!(store.get_session("standalone-bad-ws").unwrap().is_none());
        assert!(store.queue_entry("standalone-bad-ws").unwrap().is_none(), "a failed launch gives its slot back");
    }

    #[tokio::test]
    async fn spawn_standalone_worker_defaults_to_implementor_and_stamps_the_resolved_type() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-defaults-implementor".into(), existing_workspace(),
            None, default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        if result.is_ok() {
            ninox_core::tmux::kill_session("standalone-defaults-implementor").await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let session = store.get_session("standalone-defaults-implementor").unwrap()
            .expect("a successful spawn must record a session row");
        assert_eq!(session.worker_type.as_deref(), Some("Implementor"));
        assert_eq!(session.orchestrator_id, None);
    }

    #[tokio::test]
    async fn spawn_standalone_worker_with_an_explicit_type_stamps_it_and_its_success_criteria() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-researcher".into(), existing_workspace(),
            Some("Researcher".into()), default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        if result.is_ok() {
            ninox_core::tmux::kill_session("standalone-researcher").await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let session = store.get_session("standalone-researcher").unwrap()
            .expect("a successful spawn must record a session row");
        assert_eq!(session.worker_type.as_deref(), Some("Researcher"));
        let builtin_researcher = ninox_core::worker_type::WorkerTypeRegistry::from_config(&[])
            .get("Researcher").cloned().unwrap();
        assert_eq!(session.success_criteria, builtin_researcher.success);
    }

    #[tokio::test]
    async fn spawn_standalone_worker_records_spawn_facts_for_fleet_restore() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        let result = spawn_standalone_worker(
            &store, &scratch_config(), "standalone-facts".into(), existing_workspace(),
            None, default_agent(), ninox_core::brain_set::BrainSet::single("/brain"), "do the thing".into(),
        ).await;
        if result.is_ok() {
            ninox_core::tmux::kill_session("standalone-facts").await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let facts = store.fleet_record("standalone-facts").unwrap()
            .expect("record_spawn_facts must have recorded a row for fleet-restore re-briefing");
        assert_eq!(facts.task_brief.as_deref(), Some("do the thing"));
    }

    #[tokio::test]
    async fn spawn_standalone_worker_explicit_agent_wins_over_the_types_resolved_agent() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        // Whatever Researcher's builtin harness/model default is, the
        // caller's explicit agent argument must be what lands on the
        // session — the dialog's Agent/Model fields, not the type.
        let agent = ninox_core::config::AgentConfig { harness: "codex".to_string(), model: Some("o3".to_string()) };
        let mut config = scratch_config();
        config.harnesses.insert("codex".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            interactive_args: vec!["-c".into(), "'printf \"❯ \\n\"; exec cat'".into()],
            ..Default::default()
        });
        let result = spawn_standalone_worker(
            &store, &config, "standalone-explicit-agent".into(), existing_workspace(),
            Some("Researcher".into()), agent, ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await;
        if result.is_ok() {
            ninox_core::tmux::kill_session("standalone-explicit-agent").await.ok();
        }
        result.expect("spawn must succeed in this tmux-enabled test environment");
        let session = store.get_session("standalone-explicit-agent").unwrap()
            .expect("a successful spawn must record a session row");
        assert_eq!(session.agent_type, "codex");
        assert_eq!(session.model.as_deref(), Some("o3"));
    }

    #[tokio::test]
    async fn spawn_orchestrator_refuses_without_the_user_requested_flag() {
        let store = store();
        let result = run_spawn_orchestrator(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "unrequested".into(),
            None,
            false, false,
            ninox_core::brain_set::BrainSet::single("/brain"), None,
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
            true, false,
            ninox_core::brain_set::BrainSet::single("/brain"), None,
        )
        .await;
        assert!(result.is_err(), "a nameless orchestrator has no addressable session id");
    }

    #[tokio::test]
    async fn spawn_orchestrator_refuses_a_reserved_name() {
        // `messaging::HUMAN_SENDER`: an orchestrator with that id could pass
        // its own messages off as typed by the user.
        let store = store();
        let result = run_spawn_orchestrator(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "ninox-user".into(),
            None,
            true, false,
            ninox_core::brain_set::BrainSet::single("/brain"), None,
        )
        .await;

        let err = result.expect_err("must refuse the reserved name").to_string();
        assert!(err.contains("reserved"), "{err}");
        assert!(store.get_session("ninox-user").unwrap().is_none());
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
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();

        let result = run_spawn_orchestrator(
            store.clone(),
            ninox_core::config::AppConfig::default(),
            "Taken".into(),
            None,
            true, false,
            ninox_core::brain_set::BrainSet::single("/brain"), None,
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
        let _iso = ninox_core::test_isolation::isolate();
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
            store.clone(), config, "ghost orch".into(), None, true, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
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
        let env = orchestrator_env_vars("/bin/ninox", "/cfg.toml", "/brain", r#"["/brain"]"#, "orch-2", "/data");
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

    #[test]
    fn standalone_initial_prompt_appends_the_definition_of_done() {
        let _iso = ninox_core::test_isolation::isolate();
        let config = ninox_core::config::AppConfig::default();
        let (criteria, message) = crate::cli::standalone_initial_prompt(&config, "solo", "Implementor", "do x".into()).unwrap();
        let message = message.expect("a prompt is always sent");
        assert!(message.starts_with("do x") && message.contains("Definition of done"), "{message}");
        assert!(!message.contains("ninox send"), "a standalone session reports to no one: {message}");

        let (_, empty) = crate::cli::standalone_initial_prompt(&config, "solo", "Implementor", "  ".into()).unwrap();
        assert_eq!(empty.is_some(), !criteria.is_empty(), "with no prompt, only criteria are worth sending");
        assert!(crate::cli::standalone_initial_prompt(&config, "solo", "Orchestrator", "x".into()).is_none());
        assert!(crate::cli::standalone_initial_prompt(&config, "solo", "no-such-type", "x".into()).is_none());
    }

    #[test]
    fn orchestrator_initial_brief_keeps_the_brief_first() {
        let brief = crate::cli::orchestrator_initial_brief("solo", "ship it", None);
        assert!(brief.starts_with("ship it") && brief.contains("`solo`"), "{brief}");
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

    #[tokio::test]
    async fn run_spawn_sub_orchestrator_rejects_an_unowned_orchestrator_id_claim() {
        let store = store();
        let result = run_spawn_sub_orchestrator(
            store.clone(), ninox_core::config::AppConfig::default(),
            "forged-sub".into(), None, Some("some-real-orchestrator".into()), None,
            CallerRuntime { caller_type: Some("orchestrator".into()), pane: None }, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
        ).await;

        let err = result.expect_err("an unverified orchestrator-id claim must be refused").to_string();
        assert!(err.contains("some-real-orchestrator"), "{err}");
        assert!(
            store.get_session("forged-sub").unwrap().is_none(),
            "a rejected claim must leave no session row behind",
        );
    }

    #[tokio::test]
    async fn run_spawn_sub_orchestrator_rejects_exactly_at_the_depth_cap() {
        let store = store();
        register_orchestrator_for_depth_test(&store, "root", None);
        register_orchestrator_for_depth_test(&store, "sub1", Some("root"));
        register_orchestrator_for_depth_test(&store, "sub2", Some("sub1"));

        let result = run_spawn_sub_orchestrator(
            store.clone(), ninox_core::config::AppConfig::default(),
            "sub3".into(), None, Some("sub2".into()), None,
            CallerRuntime { caller_type: Some("orchestrator".into()), pane: Some(pane_for("sub2")) }, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
        ).await;

        let err = result.expect_err("spawning at exactly the cap must be rejected").to_string();
        assert!(err.to_lowercase().contains("cap"), "{err}");
        assert!(store.get_session("sub3").unwrap().is_none());
    }

    #[tokio::test]
    async fn run_spawn_sub_orchestrator_allows_one_level_under_the_cap() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        register_orchestrator_for_depth_test(&store, "root", None);
        register_orchestrator_for_depth_test(&store, "sub1", Some("root"));

        occupy_tmux_session("nspawn-test-one-level-ok").await;

        let result = run_spawn_sub_orchestrator(
            store.clone(), scratch_config(),
            "nspawn-test-one-level-ok".into(), None, Some("sub1".into()), None,
            CallerRuntime { caller_type: Some("orchestrator".into()), pane: Some(pane_for("sub1")) }, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
        ).await;

        ninox_core::tmux::kill_session("nspawn-test-one-level-ok").await.ok();
        let err = result.expect_err("tmux create must fail for a duplicate session id").to_string();
        assert!(!err.to_lowercase().contains("cap"), "{err}");
        assert!(!err.contains("could not be verified"), "{err}");
    }

    #[tokio::test]
    async fn run_spawn_sub_orchestrator_rejects_a_worker_type_without_permission() {
        let store = store();
        register_orchestrator_for_depth_test(&store, "root", None);
        let mut w = bare_session("worker-a", Some("root"));
        w.worker_type = Some("Implementation".into());
        store.upsert_session(&w).unwrap();

        let result = run_spawn_sub_orchestrator(
            store.clone(), ninox_core::config::AppConfig::default(),
            "sub-by-worker".into(), None, None, Some("worker-a".into()),
            CallerRuntime { caller_type: Some("worker".into()), pane: Some(pane_for("worker-a")) }, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
        ).await;

        let err = result.expect_err("a worker type with no nested-spawn grant must be refused").to_string();
        assert!(err.contains("does not grant"), "{err}");
        assert!(store.get_session("sub-by-worker").unwrap().is_none());
    }

    /// A worker calling the command carries `NINOX_ORCHESTRATOR_ID` (its own
    /// parent), which `dispatch()` resolves into `orchestrator_id` the same
    /// way it does for an orchestrator caller's own id — so the error
    /// wrapper must not assume "an `orchestrator_id` is present" means "the
    /// orchestrator-identity path ran" and claim a misleading "could not be
    /// verified as that orchestrator" for what is actually a permission
    /// rejection.
    #[tokio::test]
    async fn run_spawn_sub_orchestrator_worker_rejection_does_not_claim_an_orchestrator_identity_failure() {
        let store = store();
        register_orchestrator_for_depth_test(&store, "root", None);
        let mut w = bare_session("worker-a", Some("root"));
        w.worker_type = Some("Implementation".into());
        store.upsert_session(&w).unwrap();

        let result = run_spawn_sub_orchestrator(
            store.clone(), ninox_core::config::AppConfig::default(),
            "sub-by-worker-2".into(), None, Some("root".into()), Some("worker-a".into()),
            CallerRuntime { caller_type: Some("worker".into()), pane: Some(pane_for("worker-a")) }, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
        ).await;

        let err = result.expect_err("a worker type with no nested-spawn grant must be refused").to_string();
        assert!(err.contains("does not grant"), "{err}");
        assert!(
            !err.contains("could not be verified as that orchestrator"),
            "a permission rejection must not be worded as an identity-verification failure: {err}"
        );
    }

    #[tokio::test]
    async fn run_spawn_sub_orchestrator_allows_a_worker_type_with_permission() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store();
        register_orchestrator_for_depth_test(&store, "root", None);
        let mut w = bare_session("worker-a", Some("root"));
        w.worker_type = Some("Delegator".into());
        store.upsert_session(&w).unwrap();
        let config = ninox_core::config::AppConfig {
            worker_types: vec![ninox_core::worker_type::WorkerTypeSpec {
                name: "Delegator".into(),
                allow_nested_spawn: true,
                ..Default::default()
            }],
            ..scratch_config()
        };

        occupy_tmux_session("nspawn-test-permitted-worker-sub").await;

        let result = run_spawn_sub_orchestrator(
            store.clone(), config,
            "nspawn-test-permitted-worker-sub".into(), None, None, Some("worker-a".into()),
            CallerRuntime { caller_type: Some("worker".into()), pane: Some(pane_for("worker-a")) }, false, ninox_core::brain_set::BrainSet::single("/brain"), None,
        ).await;

        ninox_core::tmux::kill_session("nspawn-test-permitted-worker-sub").await.ok();
        // the point is the error must NOT be a permission or depth-cap rejection.
        let err = result.expect_err("tmux create must fail for a duplicate session id").to_string();
        assert!(!err.contains("does not grant"), "{err}");
        assert!(!err.to_lowercase().contains("cap"), "{err}");
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
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
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

/// Gated on the `test-support` feature as well as `cfg(test)`: `cfg(test)`
/// is per-crate, but these fixtures are also used by ninox-gui's and
/// ninox-tui's own test builds, which compile this crate as an ordinary
/// (non-test) dependency — so those crates enable `test-support` only in
/// their own `[dev-dependencies]`, keeping this module out of a plain build
/// (and out of the published crate's API). That also means a plain
/// `cfg(test)` build of this crate never calls these itself, hence
/// `allow(dead_code)`.
#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
pub mod test_fixtures {
    use ninox_core::types::{Session, SessionStatus};

    /// Serializes tests that mutate process-global env vars (`NINOX_CONFIG`)
    /// against each other — `cargo test` runs test fns on parallel threads,
    /// so without this guard one test's env mutation could leak into
    /// another's read.
    pub static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `key=value` for the duration of `f`, restoring the prior value
    /// (or unsetting it) afterward. Serialized via `ENV_TEST_GUARD` since
    /// env vars are process-global state shared across parallel test
    /// threads. Mirrors `ninox_core::config::tests::with_env_override`.
    pub fn with_env_override<T>(
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

    /// Like `with_env_override`, but for several keys set together under
    /// one `ENV_TEST_GUARD` lock — nesting `with_env_override` calls would
    /// deadlock on its non-reentrant mutex.
    pub fn with_env_overrides<T>(
        vars: &[(&str, &std::path::Path)],
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior: Vec<Option<String>> = vars.iter().map(|(k, _)| std::env::var(k).ok()).collect();
        for (k, v) in vars {
            std::env::set_var(k, v);
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        for ((k, _), prior) in vars.iter().zip(prior) {
            match prior {
                Some(v) => std::env::set_var(k, v),
                None    => std::env::remove_var(k),
            }
        }
        result.unwrap()
    }

    pub fn session(id: &str, orch: Option<&str>, status: SessionStatus) -> Session {
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
            brain_reads: Vec::new(), brain_write: None,
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
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
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
        ), &std::collections::HashMap::new(), &std::collections::HashMap::new());
        assert!(out.contains("boss"), "{out}");
        assert!(out.contains("w1"), "{out}");
        assert!(out.contains("PR #42"), "{out}");
        assert!(out.contains("$1.50"), "{out}");
        assert!(out.contains("pr_open"), "{out}");
    }

    #[test]
    fn board_shows_the_pr_summary_for_a_session_with_several_prs() {
        let out = render_session_board(
            &group_sessions(vec![test_fixtures::session("w1", None, SessionStatus::PrOpen)], vec![]),
            &std::collections::HashMap::new(),
            &[("w1".to_string(), "PR bellhop#1466 +3 · 1 failing".to_string())].into(),
        );
        assert!(out.contains("PR bellhop#1466 +3 · 1 failing $1.50"), "{out}");
        assert!(!out.contains("PR #42"), "{out}");
    }

    #[test]
    fn empty_store_prints_hint() {
        let out = render_session_board(&group_sessions(vec![], vec![]), &std::collections::HashMap::new(), &std::collections::HashMap::new());
        assert!(out.contains("no sessions"), "{out}");
        assert!(out.contains("ninox orchestrate"), "{out}");
    }
}

/// One test per spawn path: the model the one rule resolves is the
/// `--model` the launched pane actually runs with. Every path launches a
/// stand-in `claude-code` that prints its argv, on the isolated test tmux
/// server.
#[cfg(test)]
mod model_resolution_spawn_tests {
    use super::{run_spawn_opts, spawn_orchestrator_common, spawn_standalone_worker, CallerRuntime};
    use ninox_core::config::AppConfig;
    use ninox_core::store::Store;
    use std::sync::Arc;

    const ECHO: &str = "'printf \"❯ \\n\"; echo \"argv:$*\"; exec cat'";

    fn echo_config() -> AppConfig {
        let mut c = AppConfig {
            orchestrator_root: Some(tempfile::tempdir().unwrap().keep()),
            ..Default::default()
        };
        c.harnesses.insert("claude-code".into(), ninox_core::harness::HarnessSpec {
            enabled: true,
            binary: Some("sh".into()),
            interactive_args: vec!["-c".into(), ECHO.into(), "x".into(), "--model".into(), "{model}".into()],
            worker_args: Some(vec!["-c".into(), ECHO.into(), "x".into(), "--model".into(), "{model}".into()]),
            ..Default::default()
        });
        c.set_send_mechanism(ninox_core::config::SendMechanism::Keystrokes);
        c
    }

    fn store(iso: &ninox_core::test_isolation::Isolation) -> Arc<Store> {
        Arc::new(Store::open(iso.root().join("t.db")).unwrap())
    }

    async fn argv_line(id: &str) -> String {
        // Arbitrary; a cold `sh` start is well under it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut screen = String::new();
        while std::time::Instant::now() < deadline {
            screen = ninox_core::runtime::read_screen(id, None, false).await.unwrap_or_default();
            if let Some(line) = screen.lines().find(|l| l.contains("argv:")) {
                let line = line.to_string();
                let _ = ninox_core::tmux::kill_session(id).await;
                return line;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let _ = ninox_core::tmux::kill_session(id).await;
        panic!("{id} never printed its argv; screen:\n{screen}");
    }

    async fn cli_spawn(store: &Arc<Store>, config: AppConfig, id: &str, worker_type: Option<&str>, model: Option<&str>) -> String {
        let ws = tempfile::tempdir().unwrap().keep();
        run_spawn_opts(
            store.clone(), config, "do the task".into(), ws.to_string_lossy().into(),
            Some(id.into()), None, CallerRuntime { caller_type: None, pane: None },
            worker_type.map(Into::into), None, vec![], false, true, None, model.map(Into::into),
        )
        .await
        .expect("spawn");
        argv_line(id).await
    }

    #[tokio::test]
    async fn untyped_ninox_spawn_runs_the_worker_setting() {
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.orchestrator.model = Some("orch-m".into());
        c.worker.model = Some("worker-m".into());
        let id = format!("mr-untyped-{}", std::process::id());
        assert!(cli_spawn(&store(&iso), c, &id, None, None).await.contains("--model worker-m"));
    }

    #[tokio::test]
    async fn typed_ninox_spawn_without_a_type_model_inherits_the_worker_setting() {
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.worker.model = Some("worker-m".into());
        let id = format!("mr-typed-inherit-{}", std::process::id());
        assert!(cli_spawn(&store(&iso), c, &id, Some("Researcher"), None).await.contains("--model worker-m"));
    }

    #[tokio::test]
    async fn typed_ninox_spawn_runs_the_type_model() {
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.worker.model = Some("worker-m".into());
        c.worker_types.push(ninox_core::worker_type::WorkerTypeSpec {
            name: "Researcher".into(), model: Some("type-m".into()), ..Default::default()
        });
        let id = format!("mr-typed-{}", std::process::id());
        assert!(cli_spawn(&store(&iso), c, &id, Some("Researcher"), None).await.contains("--model type-m"));
    }

    /// An enforced type's worker launches in `Standard` mode with its rules
    /// already in the workspace; a plain worker bypasses and gets no rules.
    #[tokio::test]
    async fn an_enforced_type_worker_is_seeded_with_its_rules_and_a_plain_one_is_not() {
        use ninox_core::harness::PermissionMode;
        let iso = ninox_core::test_isolation::isolate();
        let st = store(&iso);
        let mut c = echo_config();
        let mut scoped = ninox_core::worker_type::WorkerTypeSpec { name: "Scoped".into(), ..Default::default() };
        scoped.permissions = ninox_core::worker_type::WorkerPermissionsConfig {
            enabled: true,
            allow: vec!["Read".into(), "Bash(cargo test:*)".into()],
            deny: vec!["Bash(gh pr merge:*)".into()],
            ask: vec!["Bash(git push:*)".into()],
        };
        c.worker_types.push(scoped);
        let local = |id: &str| {
            let ws = st.get_session(id).unwrap().unwrap().workspace_path.unwrap();
            std::fs::read_to_string(std::path::Path::new(&ws).join(".claude/settings.local.json"))
                .ok()
                .map(|raw| serde_json::from_str::<serde_json::Value>(&raw).unwrap())
        };

        let enforced = format!("mr-perm-on-{}", std::process::id());
        cli_spawn(&st, c.clone(), &enforced, Some("Scoped"), None).await;
        assert_eq!(st.permission_mode(&enforced).unwrap(), Some(PermissionMode::Standard));
        let mut want_deny = vec!["Bash(gh pr merge:*)".to_string()];
        want_deny.extend(ninox_core::worker_type::settings_self_protection_denies());
        assert_eq!(local(&enforced).expect("seeded")["permissions"], serde_json::json!({
            "allow": ["Read", "Bash(cargo test:*)"], "deny": want_deny, "ask": ["Bash(git push:*)"],
        }));

        let plain = format!("mr-perm-off-{}", std::process::id());
        cli_spawn(&st, c, &plain, Some("Researcher"), None).await;
        assert_eq!(st.permission_mode(&plain).unwrap(), Some(PermissionMode::Bypass));
        assert!(local(&plain).is_none_or(|v| v.get("permissions").is_none()), "{:?}", local(&plain));
    }

    #[tokio::test]
    async fn ninox_spawn_model_flag_wins() {
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.worker.model = Some("worker-m".into());
        let id = format!("mr-explicit-{}", std::process::id());
        assert!(cli_spawn(&store(&iso), c, &id, Some("Researcher"), Some("flag-m")).await.contains("--model flag-m"));
    }

    #[tokio::test]
    async fn nothing_configured_launches_the_builtin_role_default() {
        let iso = ninox_core::test_isolation::isolate();
        let id = format!("mr-none-{}", std::process::id());
        let line = cli_spawn(&store(&iso), echo_config(), &id, None, None).await;
        assert!(line.contains("--model claude-sonnet-5-5"), "{line}");
    }

    #[tokio::test]
    async fn spawn_orchestrator_runs_the_orchestrator_setting() {
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.orchestrator.model = Some("orch-m".into());
        c.worker.model = Some("worker-m".into());
        let store = store(&iso);
        let id = format!("mr-orch-{}", std::process::id());
        spawn_orchestrator_common(&store, &c, &id, None, None, None, None).await.expect("spawn");
        assert!(argv_line(&id).await.contains("--model orch-m"));
    }

    #[tokio::test]
    async fn spawn_orchestrator_model_flag_wins() {
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.orchestrator.model = Some("orch-m".into());
        let store = store(&iso);
        let id = format!("mr-orch-flag-{}", std::process::id());
        let agent = super::resolve_orchestrator_agent(&c, Some("flag-m"));
        spawn_orchestrator_common(&store, &c, &id, None, None, Some(agent), None).await.expect("spawn");
        assert!(argv_line(&id).await.contains("--model flag-m"));
    }

    #[tokio::test]
    async fn dialog_standalone_spawn_runs_the_resolved_type_model() {
        use crate::spawn_form::{resolve_spawn_agent, SpawnKind};
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.worker.model = Some("worker-m".into());
        let store = store(&iso);
        let id = format!("mr-standalone-{}", std::process::id());
        let agent = resolve_spawn_agent(&c, SpawnKind::Standalone, "Researcher", None, None).agent;
        let ws = tempfile::tempdir().unwrap().keep();
        spawn_standalone_worker(
            &store, &c, id.clone(), ws.to_string_lossy().into(), Some("Researcher".into()), agent, ninox_core::brain_set::BrainSet::single("/brain"), String::new(),
        ).await.expect("spawn");
        assert!(argv_line(&id).await.contains("--model worker-m"));
    }

    #[tokio::test]
    async fn dialog_orchestrator_spawn_runs_the_resolved_orchestrator_model() {
        use crate::spawn_form::{resolve_spawn_agent, SpawnKind, DEFAULT_WORKER_TYPE};
        let iso = ninox_core::test_isolation::isolate();
        let mut c = echo_config();
        c.orchestrator.model = Some("orch-m".into());
        let store = store(&iso);
        let id = format!("mr-dialog-orch-{}", std::process::id());
        let agent = resolve_spawn_agent(&c, SpawnKind::Orchestrator, DEFAULT_WORKER_TYPE, None, None).agent;
        spawn_orchestrator_common(&store, &c, &id, None, None, Some(agent), None).await.expect("spawn");
        assert!(argv_line(&id).await.contains("--model orch-m"));
    }

    #[tokio::test]
    async fn queued_spawn_payload_carries_the_resolved_model() {
        let iso = ninox_core::test_isolation::isolate();
        AppConfig::update(|c| {
            c.limits.set_limit(None, Some(1));
            c.worker.model = Some("worker-m".into());
        }).unwrap();
        let store = store(&iso);
        let mut busy = crate::session_queue::base_session("busy", "busy", &AppConfig::default().worker, 1);
        busy.status = ninox_core::types::SessionStatus::Working;
        store.upsert_session(&busy).unwrap();
        let ws = tempfile::tempdir().unwrap();
        run_spawn_opts(
            store.clone(), AppConfig::load_or_default(), "t".into(), ws.path().to_string_lossy().into(),
            Some("mr-queued".into()), None, CallerRuntime { caller_type: None, pane: None },
            None, None, vec![], false, false, None, None,
        ).await.unwrap();
        let entry = store.queue_entry("mr-queued").unwrap().expect("queued");
        match serde_json::from_str(&entry.payload).unwrap() {
            crate::session_queue::QueuedLaunch::CliWorker(p) => assert_eq!(p.agent.model.as_deref(), Some("worker-m")),
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[tokio::test]
    async fn restart_relaunch_keeps_the_session_model_after_a_setting_change() {
        let iso = ninox_core::test_isolation::isolate();
        let st = store(&iso);
        let mut c = echo_config();
        c.worker.model = Some("new-worker-m".into());
        let mut session = crate::session_queue::base_session("mr-refile", "mr-refile", &ninox_core::config::AgentConfig {
            harness: "claude-code".into(), model: Some("started-m".into()),
        }, 1);
        session.workspace_path = Some("/tmp".into());
        let plan = crate::restart::refile_plan(&st, &session, false, &c, "fresh").expect("plan");
        assert!(plan.base_cmd.contains("--model 'started-m'"), "{}", plan.base_cmd);
    }
}

#[cfg(test)]
mod messages_cli_tests {
    use super::{parse_since, run_messages};
    use ninox_core::{store::{HeldMessage, Store}, types::{MessageKind, MessageOutcome}};

    #[test]
    fn since_accepts_durations_and_epoch_ms() {
        assert_eq!(parse_since("30m", 10_000_000).unwrap(), 10_000_000 - 30 * 60_000);
        assert_eq!(parse_since("2h", 10_000_000).unwrap(), 10_000_000 - 2 * 3_600_000);
        assert_eq!(parse_since("1d", 100_000_000).unwrap(), 100_000_000 - 86_400_000);
        assert_eq!(parse_since("1793000000000", 0).unwrap(), 1_793_000_000_000);
        assert!(parse_since("2w", 0).is_err());
        assert!(parse_since("soon", 0).is_err());
    }

    #[test]
    fn messages_lists_every_attempt_and_whatever_is_still_held() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let now = 10 * 3_600_000;
        store.record_message_attempt(Some("w1"), "orch", "read-back", MessageKind::Status, MessageOutcome::Delivered, Some("session_socket"), None, now - 60_000).unwrap();
        store.record_message_attempt(Some("w2"), "orch", "too old", MessageKind::Status, MessageOutcome::Delivered, None, None, now - 3 * 3_600_000).unwrap();
        let held = HeldMessage { tagged: "t".into(), attributed_from: Some("w3".into()), defanged: "pr open".into(), kind: MessageKind::Status };
        store.hold_unreachable("orch", &held, "no pane", now - 120_000, now).unwrap();

        let out = run_messages(&store, "orch", Some("2h"), false, now).unwrap();
        assert!(out.contains("from w1 [status] delivered:\nread-back"), "{out}");
        assert!(!out.contains("too old"), "--since filters: {out}");
        assert!(out.contains("held 2m ago from w3 (not delivered yet):\npr open"), "{out}");

        let json: serde_json::Value = serde_json::from_str(&run_messages(&store, "orch", Some("2h"), true, now).unwrap()).unwrap();
        assert_eq!(json["messages"][0]["outcome"], "delivered");
        assert_eq!(json["held"][0]["from"], "w3");
        assert!(run_messages(&store, "nobody", None, false, now).unwrap().starts_with("no messages to nobody"));
    }
}

#[cfg(test)]
mod worker_type_list_format_tests {
    use super::format_worker_type_list;

    #[test]
    fn list_shows_use_when_model_and_can_spawn_and_names_the_default() {
        let _iso = ninox_core::test_isolation::isolate();
        let config = ninox_core::config::AppConfig::default();
        let entries = ninox_core::worker_type_cli::list().unwrap();
        let text = format_worker_type_list(&entries, &config, false).unwrap();
        assert!(text.contains("Researcher (built-in) — Investigate or answer a question"), "{text}");
        assert!(text.contains("can spawn: Researcher"), "Implementor's can_spawn: {text}");
        assert!(text.contains("claude-sonnet-5-5"), "resolved model shown: {text}");
        assert!(text.trim_end().ends_with("Default when none fits better: Implementor"));

        let json: serde_json::Value = serde_json::from_str(&format_worker_type_list(&entries, &config, true).unwrap()).unwrap();
        let researcher = json.as_array().unwrap().iter().find(|r| r["name"] == "Researcher").unwrap();
        assert!(researcher["use_when"].as_str().unwrap().starts_with("Investigate"));
        assert_eq!(researcher["can_spawn"], serde_json::json!([]));
        assert!(researcher.get("permissions").is_some(), "the full spec is still in the JSON");
        assert_eq!(researcher["resolved_model"], "claude-sonnet-5-5");

        let mut on_codex = ninox_core::config::AppConfig::default();
        on_codex.orchestrator.harness = "codex".into();
        let text = format_worker_type_list(&entries, &on_codex, false).unwrap();
        let orch = text.lines().skip_while(|l| !l.starts_with("Orchestrator")).nth(1).unwrap();
        assert!(orch.trim_start().starts_with("codex"), "the Orchestrator line shows the harness it runs on: {orch}");
    }
}
