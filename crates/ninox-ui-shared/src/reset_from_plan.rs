//! "Reset from plan": give a long-running orchestrator a fresh conversation
//! without losing the thread. Step 1 asks the orchestrator to bring its
//! registered plan doc up to date; once the doc has changed and the
//! orchestrator is idle again (or the user skips the wait), step 2 relaunches
//! the same session id on a new Claude conversation whose first prompt is that
//! plan. The GUI, the TUI and `ninox reset-from-plan` all drive [`ResetFlow`].
//!
//! A flow lives only in memory. There is no persisted "ready" state, so a
//! closed UI or a restarted server leaves nothing that could fire later, and
//! nothing resets until the caller invokes [`ResetFlow::confirm`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use ninox_core::{
    config::AppConfig,
    events::Engine,
    store::Store,
    types::{ActivityState, MessageKind, Session, SessionStatus},
};
use sha2::{Digest, Sha256};

use crate::fleet::now_ms;
use crate::restart::refile_request_with_prompt;
use crate::spawn_util::relaunch_in_place;

/// Arbitrary; an orchestrator with a big fleet can take a while to write its plan.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long after the ask an `Idle` reading counts without having seen the
/// orchestrator busy first (the activity state lags the message by a moment).
pub const DEFAULT_GRACE: Duration = Duration::from_secs(20);
/// Plans above this are referenced by path instead of embedded in the launch
/// prompt, to keep the command line a sane size.
pub const EMBED_LIMIT: usize = 64 * 1024;
/// Arbitrary; a cold harness start (auth, MCP servers) can be slow.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(90);
pub const RETIRE_REASON: &str = "reset-from-plan";

/// The ninox system message asking the orchestrator for its plan. Tune the
/// wording here; nothing else depends on it.
pub fn request_message(plan_path: Option<&str>, workspace: Option<&str>, auto_threshold: Option<u8>) -> String {
    let target = match plan_path {
        Some(p) => format!("Update your registered plan doc, {p}, in place."),
        None => {
            let dir = workspace.unwrap_or("your workspace");
            format!(
                "You have no registered plan doc. Create one (for example {dir}/PLAN.md) and register it with \
                 `ninox plan register <absolute path>`."
            )
        }
    };
    let why = match auto_threshold {
        Some(pct) => format!(
            "your context window reached the {pct}% threshold set by [orchestrator_reset] auto_at_context_pct \
             (this request was triggered automatically)"
        ),
        None => "a reset was requested".to_string(),
    };
    format!(
        "[ninox system] Your conversation is about to be reset to free up context: {why}. {target} Carry forward \
         what still matters and prune finished items, so the plan stays short. Write it so that a fresh session \
         could continue the work from the plan alone. Cover: your goals; decisions made and why; every live worker \
         with its PR and state; pending asks from the user; standing rules and constraints; and next steps. Do not \
         start new work. Reply briefly once the file is saved."
    )
}

/// What the fresh conversation is told first. Always names the plan file, so
/// the file stays the source of truth even when its content is embedded.
pub fn continuation_prompt(name: &str, plan_path: &str, content: &str) -> String {
    let head = format!(
        "You are continuing as orchestrator {name} after a context reset; the plan below is your source of truth; \
         check `ninox list` / worker states before acting. Plan file: {plan_path}"
    );
    if content.len() <= EMBED_LIMIT {
        format!("{head}\n\n--- plan ---\n{content}\n--- end of plan ---")
    } else {
        format!("{head}\n\nThe plan is too long to inline: read {plan_path} first, before anything else.")
    }
}

/// The registered plan doc at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub path: Option<String>,
    /// SHA-256 of the file content; `None` when unregistered or unreadable.
    pub hash: Option<String>,
}

impl PlanSnapshot {
    pub fn take(store: &Store, orchestrator_id: &str) -> Self {
        let path = store.get_orchestrator_plan(orchestrator_id).ok().flatten().map(|p| p.file_path);
        let hash = path.as_deref().and_then(|p| std::fs::read(p).ok()).map(|b| hex(&Sha256::digest(b)));
        Self { path, hash }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The plan counts as updated when a readable doc exists whose content (or
/// registered path) differs from the baseline. Touching the file without
/// changing it does not count.
pub fn plan_updated(baseline: &PlanSnapshot, current: &PlanSnapshot) -> bool {
    current.hash.is_some() && (baseline.path != current.path || baseline.hash != current.hash)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Ready,
    TimedOut,
}

/// Pure phase decision. Ready needs the plan updated and the orchestrator
/// idle again: seen busy since the ask, or past the grace period.
pub fn evaluate(
    updated: bool,
    activity: ActivityState,
    saw_busy: bool,
    elapsed: Duration,
    grace: Duration,
    timeout: Duration,
) -> Phase {
    let idle_again = activity == ActivityState::Idle && (saw_busy || elapsed >= grace);
    if updated && idle_again {
        Phase::Ready
    } else if elapsed >= timeout {
        Phase::TimedOut
    } else {
        Phase::Waiting
    }
}

/// A fresh id per flow, so a late result from a cancelled flow is not
/// applied to the next one opened on the same orchestrator.
pub fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Where a dialog driving a [`ResetFlow`] is, shared by the GUI and the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPhase {
    Sending,
    Waiting,
    Ready,
    TimedOut,
    SendFailed,
    Resetting,
    ResetFailed,
}

impl UiPhase {
    /// The phases the flow is still polled in.
    pub fn pollable(self) -> bool {
        matches!(self, Self::Waiting | Self::Ready | Self::TimedOut | Self::SendFailed)
    }

    /// Whether the confirm buttons may reset: not while the ask is still
    /// being sent, while resetting, or after a failed reset.
    pub fn can_reset(self) -> bool {
        self.pollable()
    }

    /// The phase after a poll: a failed send stays failed until the plan
    /// actually updates.
    pub fn after_poll(self, polled: Phase) -> Self {
        match (polled, self) {
            (Phase::Waiting, Self::SendFailed) => Self::SendFailed,
            (Phase::Waiting, _) => Self::Waiting,
            (Phase::Ready, _) => Self::Ready,
            (Phase::TimedOut, _) => Self::TimedOut,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPreview {
    pub path: String,
    pub content: String,
}

impl std::fmt::Debug for ResetFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResetFlow").field("orchestrator_id", &self.orchestrator_id).finish_non_exhaustive()
    }
}

pub struct ResetFlow {
    store: Arc<Store>,
    orchestrator_id: String,
    baseline: PlanSnapshot,
    started: Instant,
    timeout: Duration,
    grace: Duration,
    saw_busy: bool,
    /// Hash of the plan text last shown by [`Self::preview`]; confirming
    /// refuses if the file changed since.
    previewed: Option<String>,
    /// This flow wrote a marker row that tells the automatic trigger a flow
    /// is running; cleared when the flow ends.
    marked: Option<Arc<std::sync::atomic::AtomicI64>>,
}

impl Drop for ResetFlow {
    fn drop(&mut self) {
        if let Some(at) = &self.marked {
            let _ = self.store.clear_reset_marker_if(&self.orchestrator_id, at.load(std::sync::atomic::Ordering::Relaxed));
        }
    }
}

impl ResetFlow {
    /// Check `orchestrator_id` can be reset and snapshot its plan. Returns
    /// the flow and the plan request still to be sent with [`send_request`].
    /// Nothing is sent or changed yet.
    pub fn prepare(store: Arc<Store>, orchestrator_id: &str) -> anyhow::Result<(Self, String)> {
        let session = require_orchestrator(&store, orchestrator_id)?;
        let baseline = PlanSnapshot::take(&store, orchestrator_id);
        let msg = request_message(baseline.path.as_deref(), session.workspace_path.as_deref(), None);
        let mut flow = Self::with_baseline(store, orchestrator_id, baseline);
        flow.claim();
        Ok((flow, msg))
    }

    /// The Ready dialog for a flow the automatic trigger started: judged
    /// against the baseline it stored, with no second request sent and no
    /// wait for the grace period. Only meaningful while the row is still
    /// [`ResetState::Requested`]; see [`auto_flow_row`].
    pub fn from_auto_row(store: Arc<Store>, row: &ninox_core::store::ResetRow) -> Self {
        let baseline = PlanSnapshot { path: row.baseline_path.clone(), hash: row.baseline_hash.clone() };
        let mut flow = Self::with_baseline(store, &row.orchestrator_id, baseline).with_timing(DEFAULT_TIMEOUT, Duration::ZERO);
        // Claim the flow: while this dialog is open the automatic trigger
        // leaves the reset to it.
        flow.claim();
        flow
    }

    /// Tell the automatic trigger a person has this flow open, until it ends.
    /// Kept fresh by [`Self::heartbeat`].
    pub fn claim(&mut self) {
        let at = now_ms();
        if self.store.set_reset_marker(&self.orchestrator_id, at).is_ok() {
            self.marked = Some(Arc::new(std::sync::atomic::AtomicI64::new(at)));
        }
    }

    /// The user backed out. On a flow the automatic trigger started, that
    /// keeps it from starting another for this conversation.
    pub fn cancel(self) {
        let _ = self.store.suppress_auto_reset(&self.orchestrator_id);
    }

    /// [`Self::prepare`] then [`send_request`].
    pub async fn start(store: Arc<Store>, config: &AppConfig, orchestrator_id: &str) -> anyhow::Result<Self> {
        let (flow, msg) = Self::prepare(store.clone(), orchestrator_id)?;
        send_request(&store, config, orchestrator_id, &msg).await?;
        Ok(flow)
    }

    /// A flow whose request was already sent (or needs none).
    pub fn with_baseline(store: Arc<Store>, orchestrator_id: &str, baseline: PlanSnapshot) -> Self {
        Self {
            store,
            orchestrator_id: orchestrator_id.to_string(),
            baseline,
            started: Instant::now(),
            timeout: DEFAULT_TIMEOUT,
            grace: DEFAULT_GRACE,
            saw_busy: false,
            previewed: None,
            marked: None,
        }
    }

    pub fn with_timing(mut self, timeout: Duration, grace: Duration) -> Self {
        self.timeout = timeout;
        self.grace = grace;
        self
    }

    pub fn orchestrator_id(&self) -> &str {
        &self.orchestrator_id
    }

    pub fn poll(&mut self) -> Phase {
        self.poll_at(self.started.elapsed())
    }

    pub fn poll_at(&mut self, elapsed: Duration) -> Phase {
        let activity = self
            .store
            .get_session(&self.orchestrator_id)
            .ok()
            .flatten()
            .map(|s| s.activity)
            .unwrap_or_default();
        if matches!(activity, ActivityState::Working | ActivityState::Blocked) {
            self.saw_busy = true;
        }
        let updated = plan_updated(&self.baseline, &PlanSnapshot::take(&self.store, &self.orchestrator_id));
        self.heartbeat();
        evaluate(updated, activity, self.saw_busy, elapsed, self.grace, self.timeout)
    }

    /// Keep the marker fresh while this flow is open, so the automatic
    /// trigger can tell an open flow from one that died without clearing it.
    /// Called on every poll, by the CLI while it waits at its prompt, and
    /// during the reset itself.
    pub fn heartbeat(&self) {
        if let Some(at) = &self.marked {
            touch_marker(&self.store, &self.orchestrator_id, at);
        }
    }

    /// The plan as it reads now, for the confirm dialog.
    pub fn preview(&mut self) -> Option<PlanPreview> {
        let path = self.store.get_orchestrator_plan(&self.orchestrator_id).ok().flatten()?.file_path;
        let content = std::fs::read_to_string(&path).ok()?;
        self.previewed = Some(hex(&Sha256::digest(content.as_bytes())));
        Some(PlanPreview { path, content })
    }

    /// Reset now, whatever [`Self::poll`] says: "Reset now" once ready, and
    /// "Reset with the plan as it is" while waiting or after a timeout.
    pub async fn confirm(self, config: &AppConfig) -> anyhow::Result<ResetReport> {
        let engine = Engine::new(self.store.clone());
        self.confirm_with(engine, config, false).await
    }

    /// [`Self::confirm`] on `engine`; `stream_pty` is for a caller that
    /// attaches a terminal to the new pane from `ResetReport::attach`.
    /// Refuses, before touching the session, when the plan changed after
    /// the preview the user approved.
    pub async fn confirm_with(self, engine: Arc<Engine>, config: &AppConfig, stream_pty: bool) -> anyhow::Result<ResetReport> {
        if let Some(seen) = &self.previewed {
            let now = PlanSnapshot::take(&self.store, &self.orchestrator_id).hash;
            anyhow::ensure!(
                now.as_ref() == Some(seen),
                "the plan changed after the preview you reviewed; cancel and start again"
            );
        }
        // The reset itself can take a minute or two; keep the flow's marker
        // fresh meanwhile so the trigger never reads it as dead.
        let beat = self.marked.clone().map(|at| {
            let (store, id) = (self.store.clone(), self.orchestrator_id.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(HEARTBEAT_EVERY).await;
                    touch_marker(&store, &id, &at);
                }
            })
        });
        let result = reset_now(engine, config, &self.orchestrator_id, stream_pty).await;
        if let Some(beat) = beat {
            // Wait for it to stop, so it cannot write after this flow's marker
            // is cleared on drop.
            beat.abort();
            let _ = beat.await;
        }
        result
    }
}

/// How often a flow that is busy but not polling refreshes its marker; well
/// inside the trigger's 2 minute staleness window.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(15);

fn touch_marker(store: &Store, id: &str, at: &std::sync::atomic::AtomicI64) {
    use std::sync::atomic::Ordering::Relaxed;
    let now = now_ms().max(at.load(Relaxed));
    if store.refresh_reset_marker(id, now).unwrap_or(false) {
        at.store(now, Relaxed);
    }
}

/// The automatic trigger's row for `orchestrator_id`, but only while its flow
/// is still waiting on the user: a notice that outlived its flow (the
/// orchestrator was reset, the user cancelled, it timed out) finds nothing.
pub fn auto_flow_row(store: &Store, orchestrator_id: &str) -> Option<ninox_core::store::ResetRow> {
    let row = store.reset_row(orchestrator_id).ok().flatten()?;
    let live = store.get_session(orchestrator_id).ok().flatten().is_some_and(|s| {
        !s.status.is_terminal() && s.claude_session_id.as_deref() == Some(row.conversation_id.as_str())
    });
    (row.state == ninox_core::store::ResetState::Requested && live).then_some(row)
}

/// Deliver the plan request to the orchestrator as a ninox system message.
pub async fn send_request(store: &Store, config: &AppConfig, orchestrator_id: &str, message: &str) -> anyhow::Result<()> {
    ninox_core::messaging::deliver_system_message(
        store, &AppConfig::sessions_dir(), orchestrator_id, message, config.send_mechanism(), MessageKind::System,
    )
    .await
    .map(|_| ())
    .map_err(|e| anyhow::anyhow!("could not send the plan request to {orchestrator_id}: {e:#}"))
}

fn require_orchestrator(store: &Store, id: &str) -> anyhow::Result<Session> {
    let session = store.get_session(id)?.ok_or_else(|| anyhow::anyhow!("no such session: {id}"))?;
    anyhow::ensure!(store.is_orchestrator(id)?, "{id} is not an orchestrator; reset-from-plan only applies to orchestrators");
    anyhow::ensure!(!session.status.is_terminal(), "{id} is not live");
    anyhow::ensure!(
        std::env::var("NINOX_SESSION").ok().as_deref() != Some(id),
        "an orchestrator cannot reset the session it is running in; run this from another terminal"
    );
    Ok(session)
}

/// Everything step 2 needs, built from one read of the plan file.
pub struct PreparedReset {
    pub request: crate::spawn_util::RelaunchRequest,
    pub prompt: String,
    /// `true` when the prompt is part of the launch command; otherwise it
    /// must be sent as the first pane input.
    pub prompt_in_launch: bool,
    pub plan_path: String,
    pub retiring_conversation: Option<String>,
    /// The prompt file the launch command reads (harnesses without hooks);
    /// removed once launched.
    pub prompt_file: std::path::PathBuf,
    /// Under a hook-capable harness: the plan snapshot the session-setup
    /// hook delivers on the new conversation's start, which consumes it.
    /// Removed here only when the launch fails.
    pub snapshot_file: Option<std::path::PathBuf>,
}

/// Snapshot the plan once and build the fresh launch from that snapshot.
/// The plan is never re-read after this, since the old conversation could
/// still be writing it.
pub fn prepare_reset(store: &Store, config: &AppConfig, orchestrator_id: &str, started_at: i64) -> anyhow::Result<PreparedReset> {
    let session = require_orchestrator(store, orchestrator_id)?;
    let plan = store
        .get_orchestrator_plan(orchestrator_id)?
        .ok_or_else(|| anyhow::anyhow!("{orchestrator_id} has no registered plan doc to reset from"))?;
    let content = std::fs::read_to_string(&plan.file_path)
        .map_err(|e| anyhow::anyhow!("cannot read plan doc {}: {e}", plan.file_path))?;
    anyhow::ensure!(!content.contains('\0'), "plan doc {} contains a NUL byte and cannot be passed as a prompt", plan.file_path);
    let prompt = continuation_prompt(&session.name, &plan.file_path, &content);
    let retiring_conversation = session.claude_session_id.clone();
    let prompt_file = AppConfig::sessions_dir().join(format!("{orchestrator_id}.reset-prompt.md"));
    let file_str = prompt_file.to_string_lossy().into_owned();
    let (request, prompt_in_launch) = refile_request_with_prompt(store, session, true, config, started_at, &file_str)
        .ok_or_else(|| anyhow::anyhow!("{orchestrator_id} has no recorded workspace to relaunch into"))?;
    let hooked = ninox_core::session_setup::delivered_by_hook(config, &request.plan.agent.harness, std::path::Path::new(&request.plan.workspace));
    let snapshot_file = hooked.then(|| ninox_core::session_setup::reset_snapshot_path(&AppConfig::sessions_dir(), orchestrator_id));
    match &snapshot_file {
        Some(path) => write_private(path, &prompt)?,
        None if prompt_in_launch => write_private(&prompt_file, &prompt)?,
        None => {}
    }
    Ok(PreparedReset { request, prompt, prompt_in_launch, plan_path: plan.file_path, retiring_conversation, prompt_file, snapshot_file })
}

fn write_private(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(text.as_bytes())?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetReport {
    pub orchestrator_id: String,
    /// The conversation that was replaced; `claude --resume <id>` finds it.
    pub old_conversation: Option<String>,
    pub plan_path: String,
    /// Attach argv of the new pane.
    pub attach: Vec<String>,
}

/// One reset at a time per orchestrator across every process sharing the
/// store (the daemon's trigger, a GUI or TUI dialog, `ninox reset-from-plan`):
/// two concurrent relaunches would kill each other's new pane and retire the
/// conversation the first one just started. A lease in the store's fleet
/// lock table, released on drop; a lease older than [`RESET_LOCK_STALE_MS`]
/// is a reset that died and is taken over.
struct ResetGuard {
    store: Arc<Store>,
    name: String,
    holder: String,
}

/// Longer than a reset takes (it waits up to 90s for the new prompt).
const RESET_LOCK_STALE_MS: i64 = 5 * 60 * 1000;

impl ResetGuard {
    fn take(store: &Arc<Store>, id: &str) -> anyhow::Result<Self> {
        let name = format!("reset:{id}");
        let holder = Store::new_fleet_lock_holder();
        let now = now_ms();
        anyhow::ensure!(
            store.try_acquire_fleet_lock(&name, &holder, now, now - RESET_LOCK_STALE_MS)?,
            "a reset of {id} is already running"
        );
        Ok(Self { store: store.clone(), name, holder })
    }
}

impl Drop for ResetGuard {
    fn drop(&mut self) {
        let _ = self.store.release_fleet_lock(&self.name, &self.holder);
    }
}

/// Step 2. Retire the old conversation id, relaunch fresh with the plan, and
/// deliver anything that was waiting for the orchestrator.
pub async fn reset_now(engine: Arc<Engine>, config: &AppConfig, orchestrator_id: &str, stream_pty: bool) -> anyhow::Result<ResetReport> {
    let _guard = ResetGuard::take(&engine.store, orchestrator_id)?;
    let store = engine.store.clone();
    let started_at = now_ms();
    let prepared = prepare_reset(&store, config, orchestrator_id, started_at)?;
    let PreparedReset { mut request, prompt, prompt_in_launch, plan_path, retiring_conversation, prompt_file, snapshot_file } = prepared;
    // Interrupted rather than the live status, so a failed relaunch does not
    // read as healthy; Resume brings the previous conversation back.
    request.failure_status = SessionStatus::Interrupted;
    let new_conversation = request.claude_session_id.clone();
    let launched = relaunch_in_place(engine, request, config, stream_pty).await;
    let attach = match launched {
        Ok(attach) => attach,
        Err(e) => {
            let _ = std::fs::remove_file(&prompt_file);
            if let Some(snapshot) = &snapshot_file {
                let _ = std::fs::remove_file(snapshot);
            }
            return Err(anyhow::anyhow!("launch failed: {e}"));
        }
    };
    // The reset has happened: re-arm the automatic trigger for the new
    // conversation and drop the old one's readings before anything that can
    // fail, so a bookkeeping error cannot leave the old conversation armed.
    if let Err(e) = store.note_conversation_reset(orchestrator_id, &new_conversation) {
        tracing::warn!("reset-from-plan: re-arming {orchestrator_id}: {e}");
    }
    // Only a launched reset retires the old conversation.
    if let Some(old) = &retiring_conversation {
        if let Err(e) = store.record_retired_conversation(orchestrator_id, old, started_at, RETIRE_REASON) {
            tracing::warn!("reset-from-plan: recording the retired conversation {old}: {e}");
        }
    }

    if !ninox_core::runtime::wait_for_input_prompt(orchestrator_id, PROMPT_TIMEOUT).await {
        tracing::warn!("reset-from-plan: {orchestrator_id} showed no input prompt within {PROMPT_TIMEOUT:?}");
    }
    let _ = std::fs::remove_file(&prompt_file);
    let sessions_dir = AppConfig::sessions_dir();
    if !prompt_in_launch {
        let note = pane_preamble(&plan_path, &prompt);
        if let Err(e) = ninox_core::messaging::deliver_system_message(
            &store, &sessions_dir, orchestrator_id, &note, config.send_mechanism(), MessageKind::System,
        )
        .await
        {
            tracing::warn!("reset-from-plan: delivering the continuation prompt to {orchestrator_id}: {e}");
        }
    }
    if store.held_message_count(orchestrator_id).unwrap_or(0) > 0 {
        if let Err(e) =
            ninox_core::messaging::deliver_held_messages(&store, &sessions_dir, orchestrator_id, config.send_mechanism()).await
        {
            tracing::warn!("reset-from-plan: delivering held messages to {orchestrator_id}: {e}");
        }
    }
    Ok(ResetReport { orchestrator_id: orchestrator_id.to_string(), old_conversation: retiring_conversation, plan_path, attach })
}

/// The first pane input for a harness that cannot take a launch prompt:
/// short, so paste handling and length are not a problem, and it names the
/// file to read.
pub fn pane_preamble(plan_path: &str, full_prompt: &str) -> String {
    let first_line = full_prompt.lines().next().unwrap_or_default();
    format!("{first_line}\nRead {plan_path} first, before anything else.")
}

/// Waits for `handle`, refreshing `flow`'s marker every `every` so a person
/// sitting at a prompt is not mistaken for a flow that died.
async fn wait_with_heartbeat<T: Send + 'static>(flow: &ResetFlow, mut handle: tokio::task::JoinHandle<T>, every: Duration) -> anyhow::Result<T> {
    loop {
        tokio::select! {
            joined = &mut handle => return joined.map_err(|e| anyhow::anyhow!("prompt task failed: {e}")),
            _ = tokio::time::sleep(every) => flow.heartbeat(),
        }
    }
}

/// A terminal confirm for the CLI. `yes` skips the question.
pub async fn confirm_on_terminal(flow: &ResetFlow, preview: Option<&PlanPreview>, yes: bool) -> anyhow::Result<bool> {
    if let Some(p) = preview {
        println!("── plan: {} ──\n{}\n── end of plan ──", p.path, p.content);
    }
    if yes {
        return Ok(true);
    }
    use std::io::Write;
    print!("Reset this orchestrator onto a fresh conversation with this plan? [y/N] ");
    std::io::stdout().flush()?;
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    });
    let line = wait_with_heartbeat(flow, read, Duration::from_secs(15)).await??;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// `ninox reset-from-plan`.
#[derive(clap::Args, Clone, Debug)]
pub struct ResetFromPlanArgs {
    /// The orchestrator's session id
    pub orchestrator_id: String,
    /// Reset without the confirm prompt
    #[arg(long)]
    pub yes: bool,
    /// Do not ask the orchestrator to update its plan first; reset with it as it is
    #[arg(long)]
    pub skip_update: bool,
}

pub async fn run_cli(args: ResetFromPlanArgs, store: Arc<Store>, config: AppConfig) -> anyhow::Result<()> {
    let id = args.orchestrator_id.as_str();
    let mut flow = if args.skip_update {
        require_orchestrator(&store, id)?;
        let mut flow = ResetFlow::with_baseline(store.clone(), id, PlanSnapshot::take(&store, id));
        flow.claim();
        flow
    } else {
        let flow = ResetFlow::start(store.clone(), &config, id).await?;
        println!("asked {id} to update its plan; waiting (Ctrl-C to cancel, nothing is reset until you confirm)");
        flow
    };
    if !args.skip_update {
        loop {
            match flow.poll() {
                Phase::Ready => break,
                Phase::TimedOut => {
                    println!("timed out waiting for the plan update; continuing with the plan as it is");
                    break;
                }
                Phase::Waiting => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }
    let preview = flow.preview();
    if !confirm_on_terminal(&flow, preview.as_ref(), args.yes).await? {
        println!("cancelled; nothing was reset");
        return Ok(());
    }
    let report = flow.confirm(&config).await?;
    println!("{}: reset onto a fresh conversation from {}", report.orchestrator_id, report.plan_path);
    if let Some(old) = report.old_conversation {
        println!("previous conversation (resumable with `claude --resume {old}`): {old}");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests_support {
    use ninox_core::types::{Session, SessionStatus};

    pub(crate) fn session(id: &str, orch: Option<&str>, workspace: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(Into::into), name: id.into(), repo: String::new(),
            status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some(workspace.into()), pid: None,
            model: Some("claude-opus-5-5".into()), observed_model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some("old-conv".into()),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::Orchestrator;

    fn session(id: &str, orch: Option<&str>, workspace: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(Into::into), name: id.into(), repo: String::new(),
            status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some(workspace.into()), pid: None,
            model: Some("claude-opus-5-5".into()), observed_model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some("old-conv".into()),
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

    struct Fixture {
        _iso: ninox_core::test_isolation::Isolation,
        dir: tempfile::TempDir,
        store: Arc<Store>,
        plan: std::path::PathBuf,
    }

    fn fixture(plan_text: &str) -> Fixture {
        let iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        // The orchestrator's workspace carries the session-setup hook, as a seeded one does.
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::fs::write(
            ws.join(".claude/settings.local.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/b/ninox' session-setup"}]}]}}"#,
        )
        .unwrap();
        store.upsert_session(&session("orch", None, ws.to_str().unwrap())).unwrap();
        store.upsert_session(&session("w1", Some("orch"), "/tmp/ws-w1")).unwrap();
        let plan = dir.path().join("PLAN.md");
        std::fs::write(&plan, plan_text).unwrap();
        store.register_orchestrator_plan("orch", plan.to_str().unwrap(), 1).unwrap();
        Fixture { _iso: iso, dir, store, plan }
    }

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn evaluate_needs_update_and_idle() {
        let (grace, timeout) = (20 * S, 600 * S);
        use ActivityState::*;
        assert_eq!(evaluate(false, Idle, true, 5 * S, grace, timeout), Phase::Waiting);
        assert_eq!(evaluate(true, Working, true, 5 * S, grace, timeout), Phase::Waiting);
        assert_eq!(evaluate(true, Blocked, true, 5 * S, grace, timeout), Phase::Waiting);
        assert_eq!(evaluate(true, Unknown, true, 5 * S, grace, timeout), Phase::Waiting);
        assert_eq!(evaluate(true, Idle, true, 5 * S, grace, timeout), Phase::Ready);
    }

    #[test]
    fn a_stale_idle_reading_right_after_the_ask_is_not_ready() {
        let (grace, timeout) = (20 * S, 600 * S);
        assert_eq!(evaluate(true, ActivityState::Idle, false, 2 * S, grace, timeout), Phase::Waiting);
        assert_eq!(evaluate(true, ActivityState::Idle, false, 25 * S, grace, timeout), Phase::Ready);
    }

    #[test]
    fn timeout_wins_only_when_not_ready() {
        let (grace, timeout) = (20 * S, 600 * S);
        assert_eq!(evaluate(false, ActivityState::Idle, true, 601 * S, grace, timeout), Phase::TimedOut);
        assert_eq!(evaluate(true, ActivityState::Idle, true, 601 * S, grace, timeout), Phase::Ready);
    }

    #[test]
    fn detects_content_change_new_registration_and_ignores_touch() {
        let f = fixture("v1");
        let base = PlanSnapshot::take(&f.store, "orch");
        assert!(!plan_updated(&base, &PlanSnapshot::take(&f.store, "orch")));

        std::fs::write(&f.plan, "v1").unwrap(); // same bytes, new mtime
        assert!(!plan_updated(&base, &PlanSnapshot::take(&f.store, "orch")));

        std::fs::write(&f.plan, "v2").unwrap();
        assert!(plan_updated(&base, &PlanSnapshot::take(&f.store, "orch")));

        let other = f.dir.path().join("OTHER.md");
        std::fs::write(&other, "v1").unwrap();
        f.store.register_orchestrator_plan("orch", other.to_str().unwrap(), 2).unwrap();
        assert!(plan_updated(&base, &PlanSnapshot::take(&f.store, "orch")), "newly registered path counts");
    }

    #[test]
    fn unregistered_then_registered_counts_as_updated_and_missing_file_does_not() {
        let f = fixture("v1");
        f.store.unregister_orchestrator_plan("orch").unwrap();
        let base = PlanSnapshot::take(&f.store, "orch");
        assert_eq!(base, PlanSnapshot { path: None, hash: None });
        assert!(!plan_updated(&base, &base));
        f.store.register_orchestrator_plan("orch", "/definitely/missing.md", 2).unwrap();
        assert!(!plan_updated(&base, &PlanSnapshot::take(&f.store, "orch")));
        f.store.register_orchestrator_plan("orch", f.plan.to_str().unwrap(), 3).unwrap();
        assert!(plan_updated(&base, &PlanSnapshot::take(&f.store, "orch")));
    }

    #[test]
    fn flow_goes_ready_after_busy_then_idle_with_a_changed_plan_and_times_out_otherwise() {
        let f = fixture("v1");
        let mut flow = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"))
            .with_timing(100 * S, 20 * S);
        assert_eq!(flow.poll_at(S), Phase::Waiting);

        let set = |a: ActivityState| {
            let mut s = f.store.get_session("orch").unwrap().unwrap();
            s.activity = a;
            f.store.upsert_session(&s).unwrap();
        };
        set(ActivityState::Working);
        assert_eq!(flow.poll_at(2 * S), Phase::Waiting);
        std::fs::write(&f.plan, "v2").unwrap();
        assert_eq!(flow.poll_at(3 * S), Phase::Waiting, "still working");
        set(ActivityState::Idle);
        assert_eq!(flow.poll_at(4 * S), Phase::Ready);

        let mut quiet = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"))
            .with_timing(100 * S, 20 * S);
        assert_eq!(quiet.poll_at(99 * S), Phase::Waiting);
        assert_eq!(quiet.poll_at(100 * S), Phase::TimedOut);
    }

    #[test]
    fn request_message_updates_the_registered_plan_or_asks_for_a_new_one() {
        let with = request_message(Some("/p/PLAN.md"), Some("/ws"), None);
        assert!(with.starts_with("[ninox system]") && with.contains("/p/PLAN.md") && with.contains("prune finished"));
        for topic in ["goals", "decisions", "live worker", "pending asks", "standing rules", "next steps"] {
            assert!(with.contains(topic), "{topic}");
        }
        let auto = request_message(Some("/p/PLAN.md"), Some("/ws"), Some(40));
        assert!(auto.contains("40% threshold") && auto.contains("triggered automatically") && auto.contains("prune finished"));
        let without = request_message(None, Some("/ws"), None);
        assert!(without.contains("/ws/PLAN.md") && without.contains("ninox plan register"));
    }

    #[test]
    fn continuation_prompt_has_preamble_path_and_plan() {
        let p = continuation_prompt("atlas", "/p/PLAN.md", "# Plan\n- ship it");
        assert!(p.contains("continuing as orchestrator atlas after a context reset"));
        assert!(p.contains("source of truth") && p.contains("ninox list"));
        assert!(p.contains("/p/PLAN.md") && p.contains("- ship it"));
        let big = continuation_prompt("atlas", "/p/PLAN.md", &"x".repeat(EMBED_LIMIT + 1));
        assert!(big.contains("read /p/PLAN.md first") && !big.contains("xxxx"));
    }

    #[test]
    fn prepare_launches_fresh_with_a_kickoff_and_the_plan_in_a_private_snapshot_for_the_hook() {
        let plan_text = format!("# Plan\n{}\nit's {{model}} {{session_id}}\n", "- decision line\n".repeat(1300));
        assert!(plan_text.len() > 20_000);
        let f = fixture(&plan_text);
        let prep = prepare_reset(&f.store, &AppConfig::default(), "orch", 5).unwrap();
        let cmd = &prep.request.plan.base_cmd;
        assert!(prep.prompt_in_launch);
        assert!(!cmd.contains("--resume"), "{cmd}");
        assert!(cmd.contains("--session-id") && cmd.contains(&prep.request.claude_session_id));
        assert_ne!(prep.request.claude_session_id, "old-conv");
        assert!(cmd.ends_with(" -- 'Continue from your plan.'"), "a hook adds context but starts no turn, so the launch keeps a kick-off: {cmd}");
        assert!(cmd.len() < 2000, "the plan is not in the command line");
        assert!(!prep.prompt_file.exists(), "no prompt file under a hook-capable harness");
        let snapshot = prep.snapshot_file.clone().expect("the hook's snapshot");
        assert_eq!(snapshot, ninox_core::session_setup::reset_snapshot_path(&AppConfig::sessions_dir(), "orch"));
        let on_disk = std::fs::read_to_string(&snapshot).unwrap();
        assert_eq!(on_disk, prep.prompt);
        assert!(on_disk.len() > 20_000 && on_disk.contains(f.plan.to_str().unwrap()));
        assert!(on_disk.contains("- decision line") && on_disk.contains("{model} {session_id}"), "placeholder text in the plan survives");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(prep.retiring_conversation.as_deref(), Some("old-conv"));
        assert_eq!(prep.request.session.id, "orch");
        assert_eq!(prep.request.session.name, "orch");
        assert!(prep.request.is_orchestrator);
    }

    #[test]
    fn without_the_hook_installed_the_plan_rides_on_the_launch_as_before() {
        let f = fixture("# Plan\n- keep going\n");
        std::fs::remove_file(f.dir.path().join("ws/.claude/settings.local.json")).unwrap();
        let prep = prepare_reset(&f.store, &AppConfig::default(), "orch", 5).unwrap();
        assert!(prep.snapshot_file.is_none());
        assert!(prep.request.plan.base_cmd.contains(&format!(" -- \"$(cat '{}')\"", prep.prompt_file.display())));
        assert!(std::fs::read_to_string(&prep.prompt_file).unwrap().contains("keep going"));
    }

    #[test]
    fn launch_is_built_from_one_snapshot_of_the_plan() {
        let f = fixture("before");
        let prep = prepare_reset(&f.store, &AppConfig::default(), "orch", 5).unwrap();
        std::fs::write(&f.plan, "after: the old conversation kept writing").unwrap();
        let on_disk = std::fs::read_to_string(prep.snapshot_file.unwrap()).unwrap();
        assert!(on_disk.contains("before") && !on_disk.contains("after:"));
    }

    #[test]
    fn a_plan_with_a_nul_byte_is_refused_before_anything_is_touched() {
        let f = fixture("ok");
        std::fs::write(&f.plan, "bad\0plan").unwrap();
        let err = prepare_reset(&f.store, &AppConfig::default(), "orch", 1).err().expect("refused").to_string();
        assert!(err.contains("NUL"), "{err}");
    }

    #[tokio::test]
    async fn confirm_refuses_when_the_plan_changed_after_the_reviewed_preview() {
        let f = fixture("v1");
        let mut flow = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"));
        assert!(flow.preview().is_some());
        std::fs::write(&f.plan, "v2 written after the preview").unwrap();
        let engine = Engine::new(f.store.clone());
        let err = flow.confirm_with(engine, &AppConfig::default(), false).await.expect_err("refused").to_string();
        assert!(err.contains("changed after the preview"), "{err}");
        assert!(f.store.retired_conversations("orch").unwrap().is_empty());
    }

    #[test]
    fn only_one_reset_runs_per_orchestrator_at_a_time_even_across_processes() {
        let f = fixture("v1");
        let first = ResetGuard::take(&f.store, "guard-orch").unwrap();
        assert!(ResetGuard::take(&f.store, "guard-orch").err().expect("refused").to_string().contains("already running"));
        // Another process: its own connection to the same database file.
        let other = Arc::new(Store::open(f.dir.path().join("t.db")).unwrap());
        assert!(ResetGuard::take(&other, "guard-orch").is_err(), "the lease is in the shared store, not in this process");
        assert!(ResetGuard::take(&f.store, "other-orch").is_ok(), "other orchestrators are unaffected");
        drop(first);
        assert!(ResetGuard::take(&other, "guard-orch").is_ok(), "released on drop");
    }

    #[test]
    fn a_dead_resets_lease_is_taken_over_once_stale() {
        let f = fixture("v1");
        let now = now_ms();
        // A lease left by a reset that died six minutes ago.
        assert!(f.store.try_acquire_fleet_lock("reset:dead-orch", "gone", now - 6 * 60 * 1000, 0).unwrap());
        assert!(ResetGuard::take(&f.store, "dead-orch").is_ok());
    }

    #[test]
    fn a_flow_opened_from_the_auto_trigger_judges_against_its_stored_baseline_and_cancel_suppresses() {
        use ninox_core::store::{ResetRow, ResetState};
        let f = fixture("v1");
        let base = PlanSnapshot::take(&f.store, "orch");
        let mut row = ResetRow::fresh("orch", "c1", false);
        row.state = ResetState::Requested;
        row.baseline_path = base.path.clone();
        row.baseline_hash = base.hash.clone();
        f.store.put_reset_row(&row).unwrap();

        let mut flow = ResetFlow::from_auto_row(f.store.clone(), &row);
        let idle = |a| {
            let mut s = f.store.get_session("orch").unwrap().unwrap();
            s.activity = a;
            f.store.upsert_session(&s).unwrap();
        };
        idle(ActivityState::Idle);
        assert_eq!(flow.poll_at(Duration::ZERO), Phase::Waiting, "plan not updated yet");
        std::fs::write(&f.plan, "v2").unwrap();
        assert_eq!(flow.poll_at(Duration::ZERO), Phase::Ready, "no grace wait, no second request");
        assert!(f.store.reset_marker("orch").unwrap().is_some(), "the dialog claims the flow");
        let _ = &mut flow;

        flow.cancel();
        assert!(f.store.reset_marker("orch").unwrap().is_none(), "and releases it");
        assert_eq!(f.store.reset_row("orch").unwrap().unwrap().state, ResetState::Suppressed);
    }

    #[test]
    fn a_notice_that_outlived_its_flow_finds_no_flow_to_open() {
        use ninox_core::store::{ResetRow, ResetState};
        let f = fixture("v1");
        let mut row = ResetRow::fresh("orch", "old-conv", false);
        row.state = ResetState::Requested;
        f.store.put_reset_row(&row).unwrap();
        assert!(auto_flow_row(&f.store, "orch").is_some(), "a live flow");

        let mut gone = row.clone();
        gone.state = ResetState::Suppressed;
        f.store.put_reset_row(&gone).unwrap();
        assert!(auto_flow_row(&f.store, "orch").is_none(), "cancelled");

        f.store.put_reset_row(&row).unwrap();
        let mut s = f.store.get_session("orch").unwrap().unwrap();
        s.claude_session_id = Some("new-conv".into());
        f.store.upsert_session(&s).unwrap();
        assert!(auto_flow_row(&f.store, "orch").is_none(), "the orchestrator was reset onto a new conversation");
        assert!(auto_flow_row(&f.store, "nobody").is_none());
    }

    #[tokio::test]
    async fn a_flow_waiting_at_a_prompt_keeps_its_marker_fresh() {
        let f = fixture("v1");
        let (flow, _) = ResetFlow::prepare(f.store.clone(), "orch").unwrap();
        f.store.set_reset_marker("orch", 1_000).unwrap(); // looks long dead
        let slow_reader = tokio::task::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(300));
            "y"
        });
        let got = wait_with_heartbeat(&flow, slow_reader, Duration::from_millis(50)).await.unwrap();
        assert_eq!(got, "y");
        assert!(f.store.reset_marker("orch").unwrap().unwrap() > 1_000_000, "refreshed while it waited");
    }

    #[test]
    fn a_claimed_flow_holds_and_releases_its_marker_and_heartbeat_is_a_no_op_for_an_unclaimed_one() {
        let f = fixture("v1");
        let unclaimed = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"));
        unclaimed.heartbeat();
        assert!(f.store.reset_marker("orch").unwrap().is_none());
        let mut claimed = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"));
        claimed.claim();
        assert!(f.store.reset_marker("orch").unwrap().is_some());
        f.store.set_reset_marker("orch", 5).unwrap();
        claimed.heartbeat();
        assert!(f.store.reset_marker("orch").unwrap().unwrap() > 1_000_000);
        let at = claimed.marked.clone().unwrap();
        drop(claimed);
        assert!(f.store.reset_marker("orch").unwrap().is_none(), "released with the latest timestamp the heartbeat wrote");
        touch_marker(&f.store, "orch", &at);
        assert!(f.store.reset_marker("orch").unwrap().is_none(), "a late heartbeat does not bring the marker back");
    }

    #[test]
    fn an_open_flow_refreshes_its_marker_on_every_poll() {
        let f = fixture("v1");
        let (mut flow, _) = ResetFlow::prepare(f.store.clone(), "orch").unwrap();
        f.store.set_reset_marker("orch", 1_000).unwrap(); // looks long dead
        flow.poll_at(Duration::ZERO);
        assert!(f.store.reset_marker("orch").unwrap().unwrap() > 1_000_000, "refreshed to now");
        drop(flow);
        assert!(f.store.reset_marker("orch").unwrap().is_none(), "and still cleared by its own timestamp on drop");
    }

    #[test]
    fn a_manual_flows_marker_is_set_and_cleared_by_that_flow_only() {
        let f = fixture("v1");
        let (a, _) = ResetFlow::prepare(f.store.clone(), "orch").unwrap();
        assert!(f.store.reset_marker("orch").unwrap().is_some());
        drop(a);
        assert!(f.store.reset_marker("orch").unwrap().is_none());

        let (a, _) = ResetFlow::prepare(f.store.clone(), "orch").unwrap();
        f.store.set_reset_marker("orch", i64::MAX).unwrap(); // another flow's, newer
        drop(a);
        assert_eq!(f.store.reset_marker("orch").unwrap(), Some(i64::MAX), "not dropped by the older flow");
    }

    #[test]
    fn generations_are_distinct() {
        assert_ne!(next_generation(), next_generation());
    }

    #[test]
    fn prepare_rejects_workers_missing_plans_and_unreadable_plans() {
        let f = fixture("v1");
        let cfg = AppConfig::default();
        assert!(prepare_reset(&f.store, &cfg, "w1", 1).err().expect("should fail").to_string().contains("not an orchestrator"));
        assert!(prepare_reset(&f.store, &cfg, "nope", 1).err().expect("should fail").to_string().contains("no such session"));
        f.store.register_orchestrator_plan("orch", "/definitely/missing.md", 2).unwrap();
        assert!(prepare_reset(&f.store, &cfg, "orch", 1).err().expect("should fail").to_string().contains("cannot read plan"));
        f.store.unregister_orchestrator_plan("orch").unwrap();
        assert!(prepare_reset(&f.store, &cfg, "orch", 1).err().expect("should fail").to_string().contains("no registered plan"));
    }

    #[test]
    fn preparing_a_reset_leaves_workers_attached_to_the_orchestrator() {
        let f = fixture("v1");
        let before = f.store.get_session("w1").unwrap().unwrap();
        let prep = prepare_reset(&f.store, &AppConfig::default(), "orch", 1).unwrap();
        assert_eq!(prep.request.session.id, "orch", "same session id, so children still point at it");
        let after = f.store.get_session("w1").unwrap().unwrap();
        assert_eq!(after.orchestrator_id, before.orchestrator_id);
        assert_eq!(after.orchestrator_id.as_deref(), Some("orch"));
    }

    #[test]
    fn a_dropped_flow_leaves_nothing_that_could_fire() {
        let f = fixture("v1");
        let mut flow = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"))
            .with_timing(100 * S, 20 * S);
        std::fs::write(&f.plan, "v2").unwrap();
        let mut s = f.store.get_session("orch").unwrap().unwrap();
        s.activity = ActivityState::Idle;
        f.store.upsert_session(&s).unwrap();
        assert_eq!(flow.poll_at(30 * S), Phase::Ready);
        drop(flow); // UI closed or server restarted

        assert!(f.store.retired_conversations("orch").unwrap().is_empty());
        assert_eq!(f.store.get_session("orch").unwrap().unwrap().claude_session_id.as_deref(), Some("old-conv"));
        let mut again = ResetFlow::with_baseline(f.store.clone(), "orch", PlanSnapshot::take(&f.store, "orch"))
            .with_timing(100 * S, 20 * S);
        assert_eq!(again.poll_at(S), Phase::Waiting, "a new flow starts waiting, never ready");
    }

    #[test]
    fn pane_preamble_is_short_and_names_the_plan() {
        let full = continuation_prompt("atlas", "/p/PLAN.md", &"- line\n".repeat(4000));
        let note = pane_preamble("/p/PLAN.md", &full);
        assert!(note.len() < 400 && note.contains("Read /p/PLAN.md first"));
    }

    #[test]
    fn retired_conversations_round_trip_in_order() {
        let f = fixture("v1");
        f.store.record_retired_conversation("orch", "c1", 10, RETIRE_REASON).unwrap();
        f.store.record_retired_conversation("orch", "c2", 20, RETIRE_REASON).unwrap();
        let got = f.store.retired_conversations("orch").unwrap();
        assert_eq!(got.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(), ["c1", "c2"]);
        assert!(f.store.retired_conversations("w1").unwrap().is_empty());
    }
}
