//! The automatic half of reset-from-plan (`[orchestrator_reset]`): when an
//! orchestrator's context window passes a threshold, ask it for its plan and
//! then either tell the user (`prompt`) or reset it (`auto`).
//!
//! Runs as one background task in the process that owns the server port, so
//! it works with the GUI closed and never runs twice. The decisions are pure
//! ([`evaluate_trigger`]); [`tick`] applies them through [`Ops`], so the loop
//! is tested without tmux. Progress is kept in the `orchestrator_resets`
//! table, so a restart resumes a running flow and never re-fires for a
//! conversation that already has one.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ninox_core::{
    config::{AppConfig, OrchestratorResetConfig, ResetMode},
    events::{Engine, Event},
    model_resolution::model_context_window,
    store::{ResetRow, ResetState, Store},
    types::{ActivityState, Notification, NotificationKind, Session},
};
use tokio_util::sync::CancellationToken;

use crate::reset_from_plan::{
    self, request_message, Phase, PlanSnapshot, ResetFlow, DEFAULT_GRACE, DEFAULT_TIMEOUT,
};

/// Arbitrary; the trigger is not time-critical.
pub const TICK: Duration = Duration::from_secs(15);
/// After a failed or timed-out flow, how long before the trigger may fire again.
pub const COOLDOWN_MS: i64 = 30 * 60 * 1000;
/// A fresh conversation must grow this many points past its first reading
/// before the next automatic reset.
pub const GROWTH_MARGIN_PCT: f64 = 10.0;
/// A fresh conversation whose first reading is within this many points of the
/// threshold (or above it) gets the one-time "trim the plan" notice.
pub const NEAR_THRESHOLD_PCT: f64 = 5.0;
/// An open flow refreshes its marker every poll (every few seconds); one not
/// refreshed for this long belongs to a flow that died.
const MARKER_STALE_MS: i64 = 2 * 60 * 1000;

/// `(tokens, window)` for a session, where both are known: the statusline's
/// own figures, else the transcript's token count over the model table's
/// window.
pub fn context_tokens_window(s: &Session) -> (Option<u64>, Option<u64>) {
    let tokens = s.context_total_tokens.or(s.context_tokens);
    let window = s
        .context_window_size
        .or_else(|| s.model.as_deref().or(s.observed_model.as_deref()).and_then(model_context_window));
    (tokens, window)
}

/// The share of the context window in use, 0-100. The statusline's own
/// figure when it has reported one; otherwise tokens over the window (the
/// reported size, else the model table). `None` when it cannot be known.
/// The trigger and the context meter both read this.
pub fn context_pct(s: &Session) -> Option<f64> {
    if let Some(p) = s.context_used_pct {
        return Some(p);
    }
    match context_tokens_window(s) {
        (Some(tokens), Some(window)) if window > 0 => Some(tokens as f64 * 100.0 / window as f64),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Off,
    Ended,
    UnknownContext,
    InFlight,
    Suppressed,
    Cooldown,
    ManualFlow,
    NotIdle,
    AwaitingUser,
    Below,
    AwaitingBaseline,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Skip(Skip),
    /// Record the first context reading of a conversation a reset started.
    NoteBaseline(f64),
    /// Send the plan request.
    Fire,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Raise the one-time "plan too large" notice.
    pub too_large: bool,
    pub action: Action,
}

pub struct Inputs<'a> {
    pub cfg: &'a OrchestratorResetConfig,
    pub live: bool,
    pub activity: ActivityState,
    /// Asked the user something and is waiting (a pending user request).
    pub awaiting_user: bool,
    pub pct: Option<f64>,
    /// This conversation's row.
    pub row: &'a ResetRow,
    /// A person-started flow is running for it.
    pub manual_flow: bool,
    pub now_ms: i64,
}

/// The context percentage at which the next automatic reset fires: the
/// threshold, or a margin above where a fresh conversation began, whichever
/// is higher, so a big plan raises the bar instead of disabling resets.
pub fn fire_at(threshold: u8, row: &ResetRow) -> f64 {
    let t = f64::from(threshold);
    match (row.after_reset, row.post_reset_baseline_pct) {
        (true, Some(b)) => t.max(b + GROWTH_MARGIN_PCT),
        _ => t,
    }
}

/// Whether to act on one orchestrator now. Pure; every guard is here.
pub fn evaluate_trigger(i: &Inputs) -> Decision {
    let skip = |s| Decision { too_large: false, action: Action::Skip(s) };
    if !i.cfg.enabled() {
        return skip(Skip::Off);
    }
    if !i.live {
        return skip(Skip::Ended);
    }
    let Some(pct) = i.pct else { return skip(Skip::UnknownContext) };

    if i.row.after_reset && i.row.post_reset_baseline_pct.is_none() {
        // Only once the fresh conversation has gone idle after its first
        // turn, so the reading includes the plan it was started on. The
        // reset cleared the activity state, so Idle here is its own report.
        let action = if i.activity == ActivityState::Idle { Action::NoteBaseline(pct) } else { Action::Skip(Skip::AwaitingBaseline) };
        return Decision { too_large: false, action };
    }
    let too_large = i.row.after_reset
        && !i.row.loop_notified
        && i.row
            .post_reset_baseline_pct
            .is_some_and(|b| b >= f64::from(i.cfg.threshold()) - NEAR_THRESHOLD_PCT);
    let with = |action| Decision { too_large, action };

    match i.row.state {
        ResetState::Requested => return with(Action::Skip(Skip::InFlight)),
        ResetState::Suppressed => return with(Action::Skip(Skip::Suppressed)),
        ResetState::Cooldown if i.now_ms < i.row.cooldown_until => return with(Action::Skip(Skip::Cooldown)),
        ResetState::Cooldown | ResetState::Fresh => {}
    }
    if i.manual_flow {
        return with(Action::Skip(Skip::ManualFlow));
    }
    if i.activity != ActivityState::Idle {
        return with(Action::Skip(Skip::NotIdle));
    }
    if i.awaiting_user {
        return with(Action::Skip(Skip::AwaitingUser));
    }
    if pct < fire_at(i.cfg.threshold(), i.row) {
        return with(Action::Skip(Skip::Below));
    }
    with(Action::Fire)
}

/// Whether a person-started flow is open for `id`: its marker, unless that has
/// not been refreshed lately (a flow that died without clearing it).
fn manual_flow_running(store: &Store, id: &str, now_ms: i64) -> bool {
    store
        .reset_marker(id)
        .ok()
        .flatten()
        .is_some_and(|t| now_ms.saturating_sub(t) < MARKER_STALE_MS)
}

/// What [`tick`] does to the outside world.
pub trait Ops {
    fn send_request(&self, id: &str, text: &str) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn reset(&self, id: &str) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn notify(&self, n: Notification);
}

pub struct RealOps {
    pub engine: Arc<Engine>,
    pub config: AppConfig,
}

impl Ops for RealOps {
    async fn send_request(&self, id: &str, text: &str) -> anyhow::Result<()> {
        reset_from_plan::send_request(&self.engine.store, &self.config, id, text).await
    }

    async fn reset(&self, id: &str) -> anyhow::Result<()> {
        reset_from_plan::reset_now(self.engine.clone(), &self.config, id, false).await.map(|_| ())
    }

    fn notify(&self, n: Notification) {
        self.engine.emit(Event::Notification(n));
    }
}

/// Per-process memory: orchestrators already logged as having no readable
/// context figure, and those whose workspace settings were seeded.
#[derive(Default)]
pub struct Memory {
    unknown_logged: HashSet<String>,
    seeded: HashSet<(String, String)>,
}

/// Give every live orchestrator the statusLine and hooks in its own
/// workspace, once per process per (orchestrator, workspace): orchestrators
/// started before this existed never got them, and without them they never
/// report their context window (the meter and the trigger both read it).
/// Runs whether or not the automatic reset is on.
pub fn seed_running_orchestrators(store: &Store, config: &AppConfig, mem: &mut Memory) {
    let Ok(orchestrators) = store.list_orchestrators() else { return };
    for o in orchestrators {
        let Ok(Some(s)) = store.get_session(&o.id) else { continue };
        let Some(ws) = s.workspace_path.clone().filter(|_| !s.status.is_terminal()) else { continue };
        // `custom_workspace::install` owns a custom workspace's settings, guards included.
        if matches!(store.custom_workspace(&o.id), Ok(Some(_))) {
            continue;
        }
        if mem.seeded.insert((o.id.clone(), ws.clone())) {
            crate::spawn_util::seed_orchestrator_workspace_settings(std::path::Path::new(&ws), config);
        }
    }
}

fn note(kind: NotificationKind, id: &str, conversation: &str, name: &str, title: String, body: String, now_ms: i64) -> Notification {
    Notification {
        id: format!("reset-{kind:?}-{id}-{conversation}-{now_ms}"),
        kind,
        title: format!("{name}: {title}"),
        body,
        session_id: Some(id.to_string()),
        created_at: now_ms,
    }
}

/// One pass over every live orchestrator.
pub async fn tick<O: Ops>(store: &Arc<Store>, cfg: &OrchestratorResetConfig, ops: &O, now_ms: i64, mem: &mut Memory) {
    if !cfg.enabled() {
        return;
    }
    let orchestrators = match store.list_orchestrators() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("auto reset: listing orchestrators: {e}");
            return;
        }
    };
    for o in orchestrators {
        if let Err(e) = tick_one(store, cfg, ops, now_ms, mem, &o.id, &o.name).await {
            tracing::warn!("auto reset {}: {e:#}", o.id);
        }
    }
}

async fn tick_one<O: Ops>(
    store: &Arc<Store>,
    cfg: &OrchestratorResetConfig,
    ops: &O,
    now_ms: i64,
    mem: &mut Memory,
    id: &str,
    name: &str,
) -> anyhow::Result<()> {
    let Some(session) = store.get_session(id)? else { return Ok(()) };
    let live = !session.status.is_terminal();
    let Some(conversation) = session.claude_session_id.clone() else { return Ok(()) };
    if !live {
        // A flow in flight for a session that ended cannot be resumed with
        // its stale baseline and clock; it starts over if the session returns.
        if let Some(mut row) = store.reset_row(id)? {
            if row.state == ResetState::Requested {
                row.state = ResetState::Fresh;
                row.started_at = 0;
                row.ready_notified = false;
                store.put_reset_row(&row)?;
            }
        }
        return Ok(());
    }

    let mut row = match store.reset_row(id)? {
        Some(r) if r.conversation_id == conversation => r,
        previous => {
            // A conversation the row has not seen. A reset started it when the
            // row's conversation was retired by one (even if re-arming failed
            // at the time); a first sight, re-file or restart did not.
            let after_reset = previous.is_some_and(|p| {
                store.retired_conversations(id).unwrap_or_default().iter().any(|(c, _, _)| c == &p.conversation_id)
            });
            let r = ResetRow::fresh(id, &conversation, after_reset);
            store.put_reset_row(&r)?;
            r
        }
    };

    if row.state == ResetState::Requested {
        return tick_requested(store, cfg, ops, now_ms, id, name, row).await;
    }

    let pct = context_pct(&session);
    if pct.is_none() && mem.unknown_logged.insert(id.to_string()) {
        tracing::info!("auto reset: no context reading for orchestrator {id} yet; not triggering until there is one");
    }
    let manual_flow = manual_flow_running(store, id, now_ms);
    let awaiting_user = !store.open_user_requests(Some(id))?.is_empty();
    let decision = evaluate_trigger(&Inputs {
        cfg,
        live,
        activity: session.activity,
        awaiting_user,
        pct,
        row: &row,
        manual_flow,
        now_ms,
    });

    if decision.too_large {
        ops.notify(note(
            NotificationKind::ResetPlanTooLarge,
            id,
            &conversation,
            name,
            "plan too large to reset below the threshold".into(),
            format!(
                "After the last reset the conversation already started at {:.0}% of its context window \
                 (threshold {}%). Trim the plan so a reset can get below it.",
                row.post_reset_baseline_pct.unwrap_or_default(),
                cfg.threshold()
            ),
            now_ms,
        ));
        row.loop_notified = true;
        store.put_reset_row(&row)?;
    }
    match decision.action {
        Action::Skip(_) => {}
        Action::NoteBaseline(p) => {
            row.post_reset_baseline_pct = Some(p);
            store.put_reset_row(&row)?;
        }
        Action::Fire => fire(store, cfg, ops, now_ms, id, name, row).await?,
    }
    Ok(())
}

async fn fire<O: Ops>(
    store: &Arc<Store>,
    cfg: &OrchestratorResetConfig,
    ops: &O,
    now_ms: i64,
    id: &str,
    name: &str,
    mut row: ResetRow,
) -> anyhow::Result<()> {
    let session = store.get_session(id)?.ok_or_else(|| anyhow::anyhow!("session vanished"))?;
    let baseline = PlanSnapshot::take(store, id);
    let text = request_message(baseline.path.as_deref(), session.workspace_path.as_deref(), Some(cfg.threshold()));
    let sent = ops.send_request(id, &text).await;
    // The send awaited a pane; re-read the row so a reset that finished
    // meanwhile (a new conversation, a re-armed row) is not overwritten.
    match store.reset_row(id)? {
        Some(current) if current.conversation_id == row.conversation_id => row = current,
        _ => return Ok(()),
    }
    match sent {
        Ok(()) => {
            row.state = ResetState::Requested;
            row.started_at = now_ms;
            row.baseline_path = baseline.path;
            row.baseline_hash = baseline.hash;
            row.ready_notified = false;
        }
        Err(e) => {
            tracing::warn!("auto reset {id}: plan request failed: {e:#}");
            row.state = ResetState::Cooldown;
            row.cooldown_until = now_ms + COOLDOWN_MS;
            ops.notify(note(
                NotificationKind::ResetPlanTimedOut,
                id,
                &row.conversation_id,
                name,
                "could not ask for the plan".into(),
                format!("Automatic reset at {}% context: {e:#}. Nothing was reset.", cfg.threshold()),
                now_ms,
            ));
        }
    }
    store.put_reset_row(&row)?;
    Ok(())
}

async fn tick_requested<O: Ops>(
    store: &Arc<Store>,
    cfg: &OrchestratorResetConfig,
    ops: &O,
    now_ms: i64,
    id: &str,
    name: &str,
    mut row: ResetRow,
) -> anyhow::Result<()> {
    // A dialog is open: it owns the flow, the clock included. No timeout, no
    // reset under it.
    if manual_flow_running(store, id, now_ms) {
        return Ok(());
    }
    // Once the Ready notice has gone out in prompt mode, the offer stays open
    // until the user resets, cancels, or the conversation changes: no
    // timeout (a later turn would otherwise read as "never updated") and no
    // second request.
    if cfg.mode == ResetMode::Prompt && row.ready_notified {
        return Ok(());
    }
    let baseline = PlanSnapshot { path: row.baseline_path.clone(), hash: row.baseline_hash.clone() };
    let mut flow = ResetFlow::with_baseline(store.clone(), id, baseline).with_timing(DEFAULT_TIMEOUT, DEFAULT_GRACE);
    let elapsed = Duration::from_millis(now_ms.saturating_sub(row.started_at).max(0) as u64);
    match flow.poll_at(elapsed) {
        Phase::Waiting => {}
        Phase::Ready => match cfg.mode {
            // `tick` returns before here while the feature is off.
            ResetMode::Off => {}
            ResetMode::Prompt => {
                if !row.ready_notified {
                    ops.notify(note(
                        NotificationKind::ResetPlanReady,
                        id,
                        &row.conversation_id,
                        name,
                        "plan updated; ready to reset".into(),
                        format!(
                            "Context passed {}%. Review the plan and reset when you are ready; nothing resets until you confirm.",
                            cfg.threshold()
                        ),
                        now_ms,
                    ));
                    row.ready_notified = true;
                    store.put_reset_row(&row)?;
                }
            }
            ResetMode::Auto => {
                // It asked the user something since the request went out: not
                // now (the same guard `evaluate_trigger` applies to a fresh start).
                if !store.open_user_requests(Some(id))?.is_empty() {
                    return Ok(());
                }
                if let Err(e) = ops.reset(id).await {
                    tracing::warn!("auto reset {id}: {e:#}");
                    // reset_now may have re-armed the row; only a failure that
                    // left the same conversation needs the cooldown.
                    if let Some(mut now_row) = store.reset_row(id)? {
                        if now_row.conversation_id == row.conversation_id {
                            now_row.state = ResetState::Cooldown;
                            now_row.cooldown_until = now_ms + COOLDOWN_MS;
                            store.put_reset_row(&now_row)?;
                        }
                    }
                    ops.notify(note(
                        NotificationKind::ResetPlanTimedOut,
                        id,
                        &row.conversation_id,
                        name,
                        "automatic reset failed".into(),
                        format!("{e:#}"),
                        now_ms,
                    ));
                }
            }
        },
        Phase::TimedOut => {
            // Never resets: a plan that was not updated is not a plan to reset from.
            row.state = ResetState::Cooldown;
            row.cooldown_until = now_ms + COOLDOWN_MS;
            store.put_reset_row(&row)?;
            ops.notify(note(
                NotificationKind::ResetPlanTimedOut,
                id,
                &row.conversation_id,
                name,
                "plan not updated; nothing was reset".into(),
                format!(
                    "The orchestrator did not update its plan within {} minutes of the request at {}% context.",
                    DEFAULT_TIMEOUT.as_secs() / 60,
                    cfg.threshold()
                ),
                now_ms,
            ));
        }
    }
    Ok(())
}

/// Spawn the trigger loop. Reloads the config every tick so a settings change
/// applies without a restart.
pub fn spawn(engine: Arc<Engine>, token: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut mem = Memory::default();
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                _ = tokio::time::sleep(TICK) => {}
            }
            let config = AppConfig::load_or_default();
            seed_running_orchestrators(&engine.store, &config, &mut mem);
            let cfg = config.orchestrator_reset.clone();
            let ops = RealOps { engine: engine.clone(), config };
            tick(&engine.store, &cfg, &ops, crate::fleet::now_ms(), &mut mem).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{Orchestrator, SessionStatus};
    use std::sync::Mutex;

    fn cfg(pct: u8, mode: ResetMode) -> OrchestratorResetConfig {
        OrchestratorResetConfig { auto_at_context_pct: pct, mode }
    }

    fn row() -> ResetRow {
        ResetRow::fresh("o", "c1", false)
    }

    fn inputs<'a>(cfg: &'a OrchestratorResetConfig, row: &'a ResetRow) -> Inputs<'a> {
        Inputs {
            cfg, live: true, activity: ActivityState::Idle, awaiting_user: false, pct: Some(45.0), row,
            manual_flow: false, now_ms: 1_000_000,
        }
    }

    fn act(i: &Inputs) -> Action {
        evaluate_trigger(i).action
    }

    // ---- threshold math ----

    fn session_with(f: impl FnOnce(&mut Session)) -> Session {
        let mut s = crate::reset_from_plan::tests_support::session("o", None, "/ws");
        s.context_used_pct = None;
        f(&mut s);
        s
    }

    #[test]
    fn context_pct_prefers_the_statusline_figure() {
        let s = session_with(|s| {
            s.context_used_pct = Some(41.5);
            s.context_total_tokens = Some(1);
        });
        assert_eq!(context_pct(&s), Some(41.5));
    }

    #[test]
    fn context_pct_falls_back_to_tokens_over_the_reported_or_modelled_window() {
        let reported = session_with(|s| {
            s.context_total_tokens = Some(80_000);
            s.context_window_size = Some(200_000);
        });
        assert_eq!(context_pct(&reported), Some(40.0));
        let modelled = session_with(|s| {
            s.context_tokens = Some(80_000);
            s.model = Some("claude-opus-5-5".into());
        });
        assert_eq!(context_pct(&modelled), Some(40.0));
        let one_m = session_with(|s| {
            s.context_tokens = Some(80_000);
            s.model = Some("claude-opus-5-5[1m]".into());
        });
        assert_eq!(context_pct(&one_m), Some(8.0));
    }

    #[test]
    fn context_pct_is_unknown_without_tokens_or_a_window() {
        assert_eq!(context_pct(&session_with(|_| {})), None);
        assert_eq!(context_pct(&session_with(|s| { s.model = None; s.context_tokens = Some(5) })), None, "no window");
        assert_eq!(context_pct(&session_with(|s| s.context_window_size = Some(200_000))), None, "no tokens");
        let unknown_model = session_with(|s| {
            s.context_tokens = Some(5);
            s.model = Some("some-other-model".into());
        });
        assert_eq!(context_pct(&unknown_model), None);
    }

    #[test]
    fn fires_at_the_threshold_not_below() {
        let c = cfg(40, ResetMode::Prompt);
        let r = row();
        for (pct, fires) in [(39.9, false), (40.0, true), (62.0, true)] {
            let mut i = inputs(&c, &r);
            i.pct = Some(pct);
            assert_eq!(act(&i) == Action::Fire, fires, "{pct}");
        }
    }

    // ---- guards ----

    #[test]
    fn each_guard_blocks_a_trigger_that_would_otherwise_fire() {
        let c = cfg(40, ResetMode::Prompt);
        let r = row();
        assert_eq!(act(&inputs(&c, &r)), Action::Fire);

        let off = cfg(40, ResetMode::Off);
        assert_eq!(act(&inputs(&off, &r)), Action::Skip(Skip::Off));

        let mut i = inputs(&c, &r);
        i.live = false;
        assert_eq!(act(&i), Action::Skip(Skip::Ended));

        let mut i = inputs(&c, &r);
        i.pct = None;
        assert_eq!(act(&i), Action::Skip(Skip::UnknownContext));

        for a in [ActivityState::Working, ActivityState::Blocked, ActivityState::Unknown] {
            let mut i = inputs(&c, &r);
            i.activity = a;
            assert_eq!(act(&i), Action::Skip(Skip::NotIdle), "{a:?}");
        }

        let mut i = inputs(&c, &r);
        i.awaiting_user = true;
        assert_eq!(act(&i), Action::Skip(Skip::AwaitingUser));

        let mut i = inputs(&c, &r);
        i.manual_flow = true;
        assert_eq!(act(&i), Action::Skip(Skip::ManualFlow));

        for (state, skip) in [(ResetState::Requested, Skip::InFlight), (ResetState::Suppressed, Skip::Suppressed)] {
            let mut r = row();
            r.state = state;
            assert_eq!(act(&inputs(&c, &r)), Action::Skip(skip), "{state:?}");
        }
    }

    #[test]
    fn a_cooldown_blocks_until_it_passes() {
        let c = cfg(40, ResetMode::Prompt);
        let mut r = row();
        r.state = ResetState::Cooldown;
        r.cooldown_until = 2_000_000;
        assert_eq!(act(&inputs(&c, &r)), Action::Skip(Skip::Cooldown));
        let mut i = inputs(&c, &r);
        i.now_ms = 2_000_000;
        assert_eq!(act(&i), Action::Fire);
    }

    // ---- loop guard ----

    fn after_reset(baseline: Option<f64>) -> ResetRow {
        let mut r = ResetRow::fresh("o", "c2", true);
        r.post_reset_baseline_pct = baseline;
        r
    }

    #[test]
    fn the_first_reading_after_a_reset_is_recorded_not_acted_on() {
        let c = cfg(40, ResetMode::Prompt);
        let r = after_reset(None);
        let mut i = inputs(&c, &r);
        i.pct = Some(90.0);
        assert_eq!(evaluate_trigger(&i), Decision { too_large: false, action: Action::NoteBaseline(90.0) });
    }

    #[test]
    fn a_fresh_conversation_must_grow_past_its_baseline_by_the_margin() {
        let c = cfg(40, ResetMode::Prompt);
        let r = after_reset(Some(38.0));
        for (pct, fires) in [(41.0, false), (47.9, false), (48.0, true), (60.0, true)] {
            let mut i = inputs(&c, &r);
            i.pct = Some(pct);
            assert_eq!(act(&i) == Action::Fire, fires, "{pct}");
        }
        assert_eq!(fire_at(40, &r), 48.0);
        assert_eq!(fire_at(40, &after_reset(Some(10.0))), 40.0, "a small plan leaves the threshold alone");
        assert_eq!(fire_at(40, &row()), 40.0, "no margin for a conversation no reset started");
    }

    #[test]
    fn a_big_plan_raises_the_bar_without_disabling_resets_and_notifies_once() {
        let c = cfg(40, ResetMode::Prompt);
        let r = after_reset(Some(55.0));
        let mut i = inputs(&c, &r);
        i.pct = Some(60.0);
        let d = evaluate_trigger(&i);
        assert!(d.too_large, "baseline above the threshold");
        assert_eq!(d.action, Action::Skip(Skip::Below), "needs 65");
        i.pct = Some(65.0);
        assert_eq!(act(&i), Action::Fire, "still fires, later");

        let near = after_reset(Some(35.0));
        i = inputs(&c, &near);
        i.pct = Some(36.0);
        assert!(evaluate_trigger(&i).too_large, "within 5 points of the threshold");
        let far = after_reset(Some(34.9));
        i = inputs(&c, &far);
        assert!(!evaluate_trigger(&i).too_large);

        let mut notified = after_reset(Some(55.0));
        notified.loop_notified = true;
        i = inputs(&c, &notified);
        assert!(!evaluate_trigger(&i).too_large, "once");
    }

    // ---- the loop ----

    #[derive(Default)]
    struct Fake {
        requests: Mutex<Vec<(String, String)>>,
        resets: Mutex<Vec<String>>,
        notes: Mutex<Vec<Notification>>,
        store: Option<Arc<Store>>,
        fail_reset: bool,
        fail_request: bool,
        conv: Mutex<u32>,
        /// Simulates a reset finishing while the plan request is in flight.
        rearm_during_send: bool,
    }

    impl Ops for Fake {
        async fn send_request(&self, id: &str, text: &str) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail_request, "no pane");
            if self.rearm_during_send {
                let store = self.store.as_ref().unwrap();
                let mut s = store.get_session(id)?.unwrap();
                s.claude_session_id = Some("conv-other".into());
                store.upsert_session(&s)?;
                store.note_conversation_reset(id, "conv-other")?;
            }
            self.requests.lock().unwrap().push((id.into(), text.into()));
            Ok(())
        }

        async fn reset(&self, id: &str) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail_reset, "launch failed");
            self.resets.lock().unwrap().push(id.into());
            // What reset_now does: a new conversation, old readings dropped.
            let store = self.store.as_ref().unwrap();
            let n = {
                let mut c = self.conv.lock().unwrap();
                *c += 1;
                *c
            };
            let mut s = store.get_session(id)?.unwrap();
            s.claude_session_id = Some(format!("conv-{n}"));
            store.upsert_session(&s)?;
            store.note_conversation_reset(id, &format!("conv-{n}"))?;
            Ok(())
        }

        fn notify(&self, n: Notification) {
            self.notes.lock().unwrap().push(n);
        }
    }

    struct World {
        _iso: ninox_core::test_isolation::Isolation,
        _dir: tempfile::TempDir,
        store: Arc<Store>,
        plan: std::path::PathBuf,
        fake: Fake,
    }

    fn world() -> World {
        let iso = ninox_core::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("t.db")).unwrap());
        store.upsert_orchestrator(&Orchestrator { id: "o".into(), name: "o".into(), created_at: 0 }).unwrap();
        let mut s = crate::reset_from_plan::tests_support::session("o", None, "/tmp/ws");
        s.claude_session_id = Some("conv-0".into());
        s.activity = ActivityState::Idle;
        s.context_used_pct = Some(10.0);
        store.upsert_session(&s).unwrap();
        let plan = dir.path().join("PLAN.md");
        std::fs::write(&plan, "# plan v0").unwrap();
        store.register_orchestrator_plan("o", plan.to_str().unwrap(), 1).unwrap();
        let fake = Fake { store: Some(store.clone()), ..Default::default() };
        World { _iso: iso, _dir: dir, store, plan, fake }
    }

    impl World {
        fn set(&self, f: impl FnOnce(&mut Session)) {
            let mut s = self.store.get_session("o").unwrap().unwrap();
            f(&mut s);
            self.store.upsert_session(&s).unwrap();
        }

        async fn tick(&self, cfg: &OrchestratorResetConfig, now: i64, mem: &mut Memory) {
            tick(&self.store, cfg, &self.fake, now, mem).await;
        }

        fn requests(&self) -> usize {
            self.fake.requests.lock().unwrap().len()
        }

        fn kinds(&self) -> Vec<NotificationKind> {
            self.fake.notes.lock().unwrap().iter().map(|n| n.kind.clone()).collect()
        }
    }

    const T0: i64 = 10_000_000;
    const MIN: i64 = 60_000;

    #[tokio::test]
    async fn below_the_threshold_nothing_happens_and_above_it_one_request_goes_out() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.tick(&c, T0, &mut mem).await;
        assert_eq!(w.requests(), 0);
        w.set(|s| s.context_used_pct = Some(41.0));
        w.tick(&c, T0 + 15_000, &mut mem).await;
        w.tick(&c, T0 + 30_000, &mut mem).await;
        assert_eq!(w.requests(), 1, "once per conversation, not once per poll");
        let (_, text) = w.fake.requests.lock().unwrap()[0].clone();
        assert!(text.contains("40% threshold") && text.contains("triggered automatically"), "{text}");
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Requested);
    }

    #[test]
    fn running_orchestrators_are_seeded_once_whether_or_not_the_feature_is_on() {
        let w = world();
        let ws = tempfile::tempdir().unwrap();
        w.set(|s| s.workspace_path = Some(ws.path().to_string_lossy().into_owned()));
        let settings = ws.path().join(".claude/settings.local.json");
        let mut mem = Memory::default();
        assert!(!settings.exists());
        seed_running_orchestrators(&w.store, &AppConfig::default(), &mut mem);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(v["statusLine"]["command"].as_str().unwrap().ends_with("statusline"));

        std::fs::remove_file(&settings).unwrap();
        seed_running_orchestrators(&w.store, &AppConfig::default(), &mut mem);
        assert!(!settings.exists(), "once per process, not on every pass");

        seed_running_orchestrators(&w.store, &AppConfig::default(), &mut Memory::default());
        assert!(settings.exists(), "a restarted server seeds again");
    }

    #[test]
    fn a_custom_workspace_orchestrator_is_left_to_the_custom_workspace_installer() {
        let w = world();
        let ws = tempfile::tempdir().unwrap();
        w.set(|s| s.workspace_path = Some(ws.path().to_string_lossy().into_owned()));
        w.store.set_custom_workspace("o", ws.path().to_str().unwrap()).unwrap();
        seed_running_orchestrators(&w.store, &AppConfig::default(), &mut Memory::default());
        assert!(!ws.path().join(".claude").exists(), "re-seeding would strip the guards the installer wrote");
    }

    #[test]
    fn a_missing_workspace_is_not_created_and_an_ended_orchestrator_is_skipped() {
        let w = world();
        let gone = std::env::temp_dir().join("ninox-no-such-orchestrator-ws");
        w.set(|s| s.workspace_path = Some(gone.to_string_lossy().into_owned()));
        seed_running_orchestrators(&w.store, &AppConfig::default(), &mut Memory::default());
        assert!(!gone.exists());

        let ws = tempfile::tempdir().unwrap();
        w.set(|s| {
            s.workspace_path = Some(ws.path().to_string_lossy().into_owned());
            s.status = SessionStatus::Terminated;
        });
        seed_running_orchestrators(&w.store, &AppConfig::default(), &mut Memory::default());
        assert!(!ws.path().join(".claude").exists());
    }

    #[tokio::test]
    async fn the_default_config_never_triggers_even_at_99_percent() {
        let w = world();
        w.set(|s| s.context_used_pct = Some(99.0));
        let default = AppConfig::default().orchestrator_reset;
        assert!(!default.enabled());
        let mut mem = Memory::default();
        for i in 0..5 {
            w.tick(&default, T0 + i * 20_000, &mut mem).await;
        }
        assert_eq!(w.requests(), 0);
        assert!(w.kinds().is_empty());
        assert!(w.store.reset_row("o").unwrap().is_none(), "not even bookkeeping");
    }

    #[tokio::test]
    async fn context_that_cannot_be_read_never_triggers() {
        let w = world();
        w.set(|s| s.context_used_pct = None);
        let mut mem = Memory::default();
        w.tick(&cfg(40, ResetMode::Auto), T0, &mut mem).await;
        w.tick(&cfg(40, ResetMode::Auto), T0 + 15_000, &mut mem).await;
        assert_eq!(w.requests(), 0);
        assert_eq!(mem.unknown_logged.len(), 1, "logged once");
    }

    #[tokio::test]
    async fn prompt_mode_notifies_once_when_the_plan_is_updated_and_never_resets() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        assert_eq!(w.requests(), 1);

        w.tick(&c, T0 + 5_000, &mut mem).await;
        assert!(w.kinds().is_empty(), "plan not updated yet");
        std::fs::write(&w.plan, "# plan v1, updated").unwrap();
        w.tick(&c, T0 + 30_000, &mut mem).await;
        w.tick(&c, T0 + 45_000, &mut mem).await;
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanReady], "once");
        let note = w.fake.notes.lock().unwrap()[0].clone();
        assert_eq!(note.session_id.as_deref(), Some("o"), "the notification opens this orchestrator's dialog");
        assert!(w.fake.resets.lock().unwrap().is_empty(), "nothing resets until the user clicks");
    }

    #[tokio::test]
    async fn an_open_prompt_offer_never_times_out_or_asks_again() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        std::fs::write(&w.plan, "# plan updated").unwrap();
        w.tick(&c, T0 + 30_000, &mut mem).await;
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanReady]);

        // The user does not click. The orchestrator carries on working, well past
        // the flow's timeout and the cooldown.
        w.set(|s| s.activity = ActivityState::Working);
        for later in [11 * MIN, 12 * MIN, 45 * MIN, 120 * MIN] {
            w.tick(&c, T0 + later, &mut mem).await;
        }
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanReady], "no timed-out notice");
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Requested, "no cooldown, the offer stays open");
        assert_eq!(w.requests(), 1, "no second plan request");
        assert!(w.fake.resets.lock().unwrap().is_empty());

        // It still ends when the user cancels.
        w.store.suppress_auto_reset("o").unwrap();
        w.tick(&c, T0 + 200 * MIN, &mut mem).await;
        assert_eq!(w.requests(), 1);
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Suppressed);
    }

    #[tokio::test]
    async fn auto_mode_resets_when_the_plan_is_updated_and_the_orchestrator_is_idle() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        w.tick(&c, T0 + 30_000, &mut mem).await;
        assert!(w.fake.resets.lock().unwrap().is_empty(), "plan unchanged");
        std::fs::write(&w.plan, "# plan v1, updated").unwrap();
        w.set(|s| s.activity = ActivityState::Working);
        w.tick(&c, T0 + 40_000, &mut mem).await;
        assert!(w.fake.resets.lock().unwrap().is_empty(), "still working");
        w.set(|s| s.activity = ActivityState::Idle);
        w.tick(&c, T0 + 55_000, &mut mem).await;
        assert_eq!(w.fake.resets.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_mode_timeout_never_resets_it_notifies_and_cools_down() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        w.tick(&c, T0 + 11 * MIN, &mut mem).await;
        assert!(w.fake.resets.lock().unwrap().is_empty());
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanTimedOut]);
        let row = w.store.reset_row("o").unwrap().unwrap();
        assert_eq!(row.state, ResetState::Cooldown);

        w.tick(&c, T0 + 12 * MIN, &mut mem).await;
        assert_eq!(w.requests(), 1, "no re-fire on the next poll");
        w.tick(&c, T0 + 11 * MIN + COOLDOWN_MS + 1, &mut mem).await;
        assert_eq!(w.requests(), 2, "armed again once the cooldown has passed");
    }

    #[tokio::test]
    async fn a_failed_request_or_reset_cools_down_and_notifies() {
        let mut w = world();
        w.fake.fail_request = true;
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        w.tick(&c, T0 + 15_000, &mut mem).await;
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanTimedOut], "one notice, no retry storm");
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Cooldown);

        let mut w = world();
        w.fake.fail_reset = true;
        let auto = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&auto, T0, &mut mem).await;
        std::fs::write(&w.plan, "# updated").unwrap();
        w.tick(&auto, T0 + 30_000, &mut mem).await;
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanTimedOut]);
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Cooldown);
    }

    #[tokio::test]
    async fn a_user_cancel_suppresses_the_trigger_for_that_conversation() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        w.store.suppress_auto_reset("o").unwrap();
        std::fs::write(&w.plan, "# updated").unwrap();
        w.tick(&c, T0 + 30_000, &mut mem).await;
        w.tick(&c, T0 + 40 * MIN, &mut mem).await;
        assert_eq!(w.requests(), 1);
        assert!(w.kinds().is_empty(), "a cancelled flow is not announced");
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Suppressed);
    }

    #[tokio::test]
    async fn blocked_idle_awaiting_and_manual_flows_are_left_alone() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));

        w.set(|s| s.activity = ActivityState::Working);
        w.tick(&c, T0, &mut mem).await;
        w.set(|s| s.activity = ActivityState::Blocked);
        w.tick(&c, T0 + 1, &mut mem).await;
        w.set(|s| s.activity = ActivityState::Idle);

        w.store.set_reset_marker("o", T0).unwrap();
        w.tick(&c, T0 + 2, &mut mem).await;
        w.store.clear_reset_marker("o").unwrap();
        assert_eq!(w.requests(), 0);

        w.store
            .insert_user_request(&ninox_core::store::UserRequestRow {
                id: "ur".into(), requester_session_id: "o".into(), prompt: "which?".into(),
                mode: ninox_core::types::UserRequestMode::Async, status: ninox_core::types::UserRequestStatus::Pending,
                answer: None, answered_at: None, answer_delivered_at: None, created_at: 1,
            })
            .unwrap();
        w.tick(&c, T0 + 3, &mut mem).await;
        assert_eq!(w.requests(), 0, "waiting on the user");
    }

    #[tokio::test]
    async fn the_baseline_waits_for_the_fresh_conversation_to_go_idle() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.store.note_conversation_reset("o", "conv-1").unwrap();
        w.set(|s| {
            s.claude_session_id = Some("conv-1".into());
            s.context_used_pct = Some(3.0);
        });
        // Reports a reading before it has processed the plan it started on.
        for a in [ActivityState::Unknown, ActivityState::Working] {
            w.set(|s| s.activity = a);
            w.tick(&c, T0, &mut mem).await;
            assert_eq!(w.store.reset_row("o").unwrap().unwrap().post_reset_baseline_pct, None, "{a:?}");
        }
        w.set(|s| {
            s.activity = ActivityState::Idle;
            s.context_used_pct = Some(52.0);
        });
        w.tick(&c, T0 + 20_000, &mut mem).await;
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().post_reset_baseline_pct, Some(52.0), "the reading that includes the plan");
    }

    #[tokio::test]
    async fn an_auto_reset_leaves_a_flow_a_person_has_open_to_them() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        std::fs::write(&w.plan, "# updated").unwrap();
        w.store.set_reset_marker("o", T0 + 25_000).unwrap();
        w.tick(&c, T0 + 30_000, &mut mem).await;
        assert!(w.fake.resets.lock().unwrap().is_empty(), "the dialog's flow finishes it");
        w.store.clear_reset_marker("o").unwrap();
        w.tick(&c, T0 + 45_000, &mut mem).await;
        assert_eq!(w.fake.resets.lock().unwrap().len(), 1, "no marker, no one else: the trigger resets");
    }

    #[tokio::test]
    async fn a_request_that_finished_after_a_reset_does_not_overwrite_the_rearmed_row() {
        let mut w = world();
        w.fake.rearm_during_send = true;
        let c = cfg(40, ResetMode::Prompt);
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut Memory::default()).await;
        let row = w.store.reset_row("o").unwrap().unwrap();
        assert_eq!(row.conversation_id, "conv-other");
        assert!(row.after_reset && row.state == ResetState::Fresh, "the re-armed row survived: {row:?}");
    }

    #[tokio::test]
    async fn a_flow_in_flight_for_a_session_that_ended_starts_over() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Requested);
        w.set(|s| s.status = SessionStatus::Terminated);
        w.tick(&c, T0 + 20_000, &mut mem).await;
        let row = w.store.reset_row("o").unwrap().unwrap();
        assert_eq!((row.state, row.started_at), (ResetState::Fresh, 0));
        // Resumed an hour later on the same conversation: no bogus timeout.
        w.set(|s| s.status = SessionStatus::Working);
        w.tick(&c, T0 + 60 * MIN, &mut mem).await;
        assert!(w.kinds().is_empty(), "{:?}", w.kinds());
    }

    #[tokio::test]
    async fn a_reset_whose_rearm_failed_is_still_recognised_by_its_retired_conversation() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await; // row for conv-0
        // A reset launched conv-1 and retired conv-0, but the re-arm write was lost.
        w.store.record_retired_conversation("o", "conv-0", T0, "reset-from-plan").unwrap();
        w.set(|s| {
            s.claude_session_id = Some("conv-1".into());
            s.activity = ActivityState::Idle;
            s.context_used_pct = Some(52.0);
        });
        w.tick(&c, T0 + 20_000, &mut mem).await;
        let row = w.store.reset_row("o").unwrap().unwrap();
        assert!(row.after_reset, "the loop guard still applies");
        w.tick(&c, T0 + 40_000, &mut mem).await;
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().post_reset_baseline_pct, Some(52.0));
        assert_eq!(w.requests(), 1, "only conv-0's request: conv-1 starts at 52 and waits for growth");
    }

    #[tokio::test]
    async fn an_open_dialog_owns_the_flow_through_the_timeout_and_a_dead_ones_does_not() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;

        // The dialog has been open 11 minutes and keeps refreshing its marker.
        w.store.set_reset_marker("o", T0 + 11 * MIN - 5_000).unwrap();
        w.tick(&c, T0 + 11 * MIN, &mut mem).await;
        w.tick(&c, T0 + 12 * MIN, &mut mem).await;
        assert!(w.kinds().is_empty(), "no timed-out notice under an open dialog: {:?}", w.kinds());
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Requested, "no cooldown either");
        assert!(w.fake.resets.lock().unwrap().is_empty());

        // The dialog's process died: its marker stops being refreshed.
        w.tick(&c, T0 + 20 * MIN, &mut mem).await;
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanTimedOut], "now it times out as usual");
    }

    #[tokio::test]
    async fn auto_mode_does_not_reset_while_the_orchestrator_is_waiting_on_the_user() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut mem).await;
        // After the request, the orchestrator posts a question and goes idle.
        w.store
            .insert_user_request(&ninox_core::store::UserRequestRow {
                id: "ur".into(), requester_session_id: "o".into(), prompt: "which?".into(),
                mode: ninox_core::types::UserRequestMode::Async, status: ninox_core::types::UserRequestStatus::Pending,
                answer: None, answered_at: None, answer_delivered_at: None, created_at: 1,
            })
            .unwrap();
        std::fs::write(&w.plan, "# updated").unwrap();
        w.tick(&c, T0 + 30_000, &mut mem).await;
        w.tick(&c, T0 + 45_000, &mut mem).await;
        assert!(w.fake.resets.lock().unwrap().is_empty(), "the conversation waiting on the answer is kept");
        assert_eq!(w.store.reset_row("o").unwrap().unwrap().state, ResetState::Requested, "and it stays open, not timed out");
        w.store.mark_user_request_cancelled("ur").unwrap();
        w.tick(&c, T0 + 60_000, &mut mem).await;
        assert_eq!(w.fake.resets.lock().unwrap().len(), 1, "once the question is closed it resets");
    }

    #[tokio::test]
    async fn a_stale_marker_from_a_dead_flow_is_ignored() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        let mut mem = Memory::default();
        w.set(|s| s.context_used_pct = Some(45.0));
        w.store.set_reset_marker("o", T0).unwrap();
        w.tick(&c, T0 + 20 * MIN, &mut mem).await;
        assert_eq!(w.requests(), 1);
    }

    #[tokio::test]
    async fn a_server_restart_resumes_a_running_flow_without_firing_again() {
        let w = world();
        let c = cfg(40, ResetMode::Prompt);
        w.set(|s| s.context_used_pct = Some(45.0));
        w.tick(&c, T0, &mut Memory::default()).await;
        assert_eq!(w.requests(), 1);

        // A new process: fresh memory, same store.
        let mut after_restart = Memory::default();
        w.tick(&c, T0 + 20_000, &mut after_restart).await;
        assert_eq!(w.requests(), 1, "no second request for the conversation");
        std::fs::write(&w.plan, "# updated while the server was down").unwrap();
        w.tick(&c, T0 + 40_000, &mut after_restart).await;
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanReady], "the stored flow completes");
    }

    #[tokio::test]
    async fn a_reset_drops_the_old_conversations_context_reading() {
        let w = world();
        w.set(|s| {
            s.context_used_pct = Some(55.0);
            s.context_tokens = Some(1);
            s.context_total_tokens = Some(2);
            s.context_window_size = Some(3);
        });
        w.store.note_conversation_reset("o", "conv-1").unwrap();
        let s = w.store.get_session("o").unwrap().unwrap();
        assert_eq!((s.context_used_pct, s.context_tokens, s.context_total_tokens, s.context_window_size), (None, None, None, None));

        // The new conversation has not reported yet: no baseline, no trigger.
        w.set(|s| s.claude_session_id = Some("conv-1".into()));
        let mut mem = Memory::default();
        w.tick(&cfg(40, ResetMode::Auto), T0, &mut mem).await;
        let row = w.store.reset_row("o").unwrap().unwrap();
        assert_eq!(row.post_reset_baseline_pct, None, "the old 55% was not read as the new conversation's");
        assert_eq!(w.requests(), 0);
        assert!(w.kinds().is_empty());
    }

    /// Context climbs, the plan is updated, the reset happens, and the next
    /// conversation does it again, three times, each firing exactly once.
    #[tokio::test]
    async fn three_consecutive_auto_resets_each_fire_once() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        let mut now = T0;
        for round in 1..=3u32 {
            let before = w.requests();
            // A fresh conversation reports Idle after its first turn.
            w.set(|s| s.activity = ActivityState::Idle);
            // Below the bar: nothing.
            w.set(|s| s.context_used_pct = Some(20.0));
            now += 20_000;
            w.tick(&c, now, &mut mem).await;
            assert_eq!(w.requests(), before, "round {round}: below the threshold");

            w.set(|s| s.context_used_pct = Some(45.0));
            now += 20_000;
            w.tick(&c, now, &mut mem).await;
            assert_eq!(w.requests(), before + 1, "round {round}: one plan request");

            // Repeated polls do not ask again.
            now += 20_000;
            w.tick(&c, now, &mut mem).await;
            assert_eq!(w.requests(), before + 1, "round {round}");

            // The plan is updated; the orchestrator is idle: reset.
            std::fs::write(&w.plan, format!("# plan v{round}")).unwrap();
            now += 40_000;
            w.tick(&c, now, &mut mem).await;
            assert_eq!(w.fake.resets.lock().unwrap().len() as u32, round, "round {round}: one reset");
            let s = w.store.get_session("o").unwrap().unwrap();
            assert_eq!(s.claude_session_id.as_deref(), Some(format!("conv-{round}").as_str()));
            assert_eq!(s.context_used_pct, None);
        }
    }

    #[tokio::test]
    async fn the_loop_guard_waits_for_growth_and_notifies_once_about_a_big_plan() {
        let w = world();
        let c = cfg(40, ResetMode::Auto);
        let mut mem = Memory::default();
        w.store.note_conversation_reset("o", "conv-1").unwrap();
        w.set(|s| {
            s.claude_session_id = Some("conv-1".into());
            s.activity = ActivityState::Idle;
        });

        // The new conversation starts above the threshold (a big plan).
        w.set(|s| s.context_used_pct = Some(55.0));
        w.tick(&c, T0, &mut mem).await;
        w.set(|s| s.context_used_pct = Some(58.0));
        w.tick(&c, T0 + 15_000, &mut mem).await;
        w.tick(&c, T0 + 30_000, &mut mem).await;
        assert_eq!(w.requests(), 0, "does not fire straight away");
        assert_eq!(w.kinds(), vec![NotificationKind::ResetPlanTooLarge], "told once");

        // It does fire once it has grown by the margin.
        w.set(|s| s.context_used_pct = Some(65.0));
        w.tick(&c, T0 + 45_000, &mut mem).await;
        assert_eq!(w.requests(), 1, "resets are delayed, not disabled");
        assert_eq!(w.kinds().len(), 1);
    }

    #[test]
    fn manual_resets_use_the_same_rearm_path() {
        let w = world();
        w.store.note_conversation_reset("o", "conv-9").unwrap();
        let row = w.store.reset_row("o").unwrap().unwrap();
        assert_eq!((row.conversation_id.as_str(), row.after_reset, row.state), ("conv-9", true, ResetState::Fresh));
        let _ = SessionStatus::Working;
    }
}
