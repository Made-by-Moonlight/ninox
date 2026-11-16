//! The Ninox prompt's conversation state (`PromptReply.dc.html`), shared by
//! the GUI reply drawer and the TUI reply panel: turns, relays, header
//! status, and the async delivery/poll reads that feed them. `R` is each
//! frontend's per-turn render cache (parsed Markdown), rebuilt by the caller
//! whenever an apply reports the answer changed.

use std::sync::Arc;

use ninox_core::{config::AppConfig, store::Store};

/// How long a relay with no direct reply keeps reading "waiting for its
/// answer…" after the prompt agent's turn ends. The target may answer its
/// own parent orchestrator rather than `ninox-prompt`, which this drawer
/// can't observe — arbitrary, tune freely.
pub const RELAY_REPLY_GRACE_MS: i64 = 5 * 60 * 1000;

/// Where a turn's answer is read from, set once delivery succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerSource {
    /// Not delivered yet.
    Unresolved,
    /// A claude-code prompt agent: its transcript JSONL (Markdown intact).
    Transcript(super::TranscriptCursor),
    /// Any other prompt-agent harness: `reply_since`'s pane-text cursor.
    PromptPane { cursor: usize },
    /// A directly picked orchestrator: its pane's tail, re-read each poll.
    /// `baseline`: the pane's last reply block just before delivery, which
    /// is the previous answer and never this turn's.
    OrchestratorPane { id: String, baseline: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerState {
    Pending,
    Streaming,
    Done,
    Failed,
}

/// One `ninox send` from the prompt agent during a turn
/// (`message_events` rows from `PROMPT_SESSION_ID`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    pub to_id: String,
    pub relayed_at_ms: i64,
    pub replied_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayState {
    Waiting,
    Replied(i64),
    NoDirectReply,
}

#[derive(Debug, Clone)]
pub struct Turn<R = ()> {
    pub question: String,
    pub target_label: String,
    pub asked_at_ms: i64,
    /// Markdown for transcript answers; extracted pane text otherwise.
    pub answer: String,
    /// Raw accumulated pane text for `AnswerSource::PromptPane`, from which
    /// `answer` is re-extracted on every delta.
    pane_text: String,
    pub state: AnswerState,
    pub ended_at_ms: Option<i64>,
    pub error: Option<String>,
    pub relays: Vec<Relay>,
    pub source: AnswerSource,
    /// The frontend's render of `answer`, refreshed by the caller after
    /// each change — never while drawing.
    pub rendered: R,
}

impl<R: Default> Turn<R> {
    fn new(question: String, target_label: String, asked_at_ms: i64) -> Self {
        Turn {
            question,
            target_label,
            asked_at_ms,
            answer: String::new(),
            pane_text: String::new(),
            state: AnswerState::Pending,
            ended_at_ms: None,
            error: None,
            relays: Vec::new(),
            source: AnswerSource::Unresolved,
            rendered: R::default(),
        }
    }

    /// Answered by the prompt agent rather than a directly picked
    /// orchestrator, so its plans are the ones proposed as `ninox-prompt`.
    pub fn from_prompt_agent(&self) -> bool {
        matches!(self.source, AnswerSource::Transcript(_) | AnswerSource::PromptPane { .. })
    }

    /// [`crate::ghost_view::claims_unstored_plan`] for this turn, given the
    /// plan ids `plans_per_turn` matched to it and every listed plan. A
    /// failed turn already shows its error instead.
    pub fn claims_unstored_plan(
        &self,
        turn_plans: &[String],
        plans: &[ninox_core::ghost_plan::PlanWithNodes],
        now_ms: i64,
    ) -> bool {
        let finished = !self.is_active() && self.state != AnswerState::Failed;
        self.from_prompt_agent()
            && crate::ghost_view::claims_unstored_plan(&self.answer, finished, turn_plans, plans, self.asked_at_ms, now_ms)
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, AnswerState::Pending | AnswerState::Streaming)
    }

    /// Waiting on the answer's first text: what the "is thinking…"
    /// placeholder (and its animation) covers.
    pub fn is_thinking(&self) -> bool {
        self.is_active() && self.answer.trim().is_empty()
    }

    fn end(&mut self, state: AnswerState, now_ms: i64) {
        self.state = state;
        self.ended_at_ms.get_or_insert(now_ms);
    }

    pub fn fail(&mut self, message: String, now_ms: i64) {
        self.error = Some(message);
        self.end(AnswerState::Failed, now_ms);
    }

    /// Returns whether `answer` changed. The agent can resume a turn it
    /// ended (e.g. to report a relayed answer), so a later delta re-opens it.
    pub fn apply_transcript(
        &mut self,
        delta: super::TranscriptDelta,
        now_ms: i64,
    ) -> bool {
        let changed = !delta.text_blocks.is_empty();
        for block in delta.text_blocks {
            if !self.answer.is_empty() {
                self.answer.push_str("\n\n");
            }
            self.answer.push_str(&block);
        }
        match delta.ended {
            Some(true) => self.end(AnswerState::Done, now_ms),
            Some(false) => {
                self.state = AnswerState::Streaming;
                self.ended_at_ms = None;
            }
            None => {}
        }
        changed
    }

    /// `append`: an incremental `reply_since` delta (prompt agent on a
    /// non-claude harness) rather than a full tail snapshot (picked
    /// orchestrator). A pane gives no end-of-turn signal, so these turns
    /// stay `Streaming` until the next ask. Returns whether `answer` changed.
    pub fn apply_pane_text(&mut self, text: &str, append: bool) -> bool {
        if append {
            self.pane_text.push_str(text);
        } else {
            self.pane_text = text.to_string();
        }
        let extracted = last_reply_block(&self.pane_text);
        let stale = matches!(&self.source, AnswerSource::OrchestratorPane { baseline, .. } if *baseline == extracted);
        if extracted.is_empty() || extracted == self.answer || stale {
            return false;
        }
        self.answer = extracted;
        self.state = AnswerState::Streaming;
        true
    }

    /// Merges the store's recent `message_events` into `relays`: a send
    /// from the prompt agent between this turn's ask and `until_ms` (the
    /// next turn's ask), answered by the first send back from that target.
    /// Merged, never rebuilt: `message_events` keeps only its newest 200
    /// rows, so a relay or its reply can age out of `events`.
    pub fn sync_relays(
        &mut self,
        events: &[ninox_core::types::MessageEvent],
        until_ms: Option<i64>,
    ) {
        let agent = ninox_core::worker_type::PROMPT_SESSION_ID;
        let mut relays = std::mem::take(&mut self.relays);
        for e in events {
            let in_turn =
                e.delivered_at >= self.asked_at_ms && until_ms.is_none_or(|u| e.delivered_at < u);
            if !in_turn || e.from_id.as_deref() != Some(agent) {
                continue;
            }
            let replied_at_ms = events
                .iter()
                .find(|r| {
                    r.from_id.as_deref() == Some(e.to_id.as_str())
                        && r.to_id == agent
                        && r.delivered_at > e.delivered_at
                })
                .map(|r| r.delivered_at);
            match relays.iter_mut().find(|r| r.to_id == e.to_id) {
                Some(existing) => {
                    existing.replied_at_ms = existing.replied_at_ms.or(replied_at_ms);
                }
                None => relays.push(Relay {
                    to_id: e.to_id.clone(),
                    relayed_at_ms: e.delivered_at,
                    replied_at_ms,
                }),
            }
        }
        for relay in relays.iter_mut().filter(|r| r.replied_at_ms.is_none()) {
            relay.replied_at_ms = events
                .iter()
                .find(|r| {
                    r.from_id.as_deref() == Some(relay.to_id.as_str())
                        && r.to_id == agent
                        && r.delivered_at > relay.relayed_at_ms
                })
                .map(|r| r.delivered_at);
        }
        self.relays = relays;
    }

    /// Whether polling can still change anything: the answer is in
    /// progress, or a transcript turn may yet resume (the agent reports a
    /// relayed answer back) while a relay waits or the turn ended recently.
    pub fn wants_poll(&self, now_ms: i64) -> bool {
        match self.source {
            AnswerSource::Unresolved => false,
            AnswerSource::Transcript(_) => {
                self.is_active()
                    || self
                        .relays
                        .iter()
                        .any(|r| self.relay_state(r, now_ms) == RelayState::Waiting)
                    || self
                        .ended_at_ms
                        .is_some_and(|t| now_ms - t < RELAY_REPLY_GRACE_MS)
            }
            AnswerSource::PromptPane { .. } | AnswerSource::OrchestratorPane { .. } => {
                self.is_active()
            }
        }
    }

    pub fn relay_state(&self, relay: &Relay, now_ms: i64) -> RelayState {
        if let Some(at) = relay.replied_at_ms {
            return RelayState::Replied(at);
        }
        match self.ended_at_ms {
            Some(ended) if now_ms - ended >= RELAY_REPLY_GRACE_MS => RelayState::NoDirectReply,
            _ => RelayState::Waiting,
        }
    }
}

/// The drawer's header status, from the latest turn only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerStatus {
    Answering,
    Relaying,
    Idle,
}

impl DrawerStatus {
    pub fn label(self) -> &'static str {
        match self {
            DrawerStatus::Answering => "answering",
            DrawerStatus::Relaying => "relaying",
            DrawerStatus::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    ContextCleared,
    /// Clear context was asked for mid-answer; it restarts once that
    /// answer finishes.
    ClearPending,
    Error(String),
}

pub const CLEAR_PENDING_TEXT: &str = "Clearing after the current answer…";

/// The reply drawer (`PromptReply.dc.html`): the prompt conversation,
/// expanded over the bottom of the current view or collapsed to one line
/// above the prompt bar.
#[derive(Debug, Clone, Default)]
pub struct ReplyDrawer<R = ()> {
    pub turns: Vec<Turn<R>>,
    pub expanded: bool,
    pub notice: Option<Notice>,
    /// The prompt agent's `context_used_pct`; `None` hides the figure.
    pub context_pct: Option<f64>,
    /// A poll is running; the 3s tick skips issuing another, so two reads
    /// from the same cursor can never both append.
    pub poll_in_flight: bool,
}

impl<R: Default> ReplyDrawer<R> {
    pub fn has_content(&self) -> bool {
        !self.turns.is_empty() || self.notice.is_some()
    }

    pub fn is_open(&self) -> bool {
        self.expanded && self.has_content()
    }

    /// The latest turn is on screen in its "is thinking…" state.
    pub fn shows_thinking(&self) -> bool {
        self.is_open() && self.turns.last().is_some_and(Turn::is_thinking)
    }

    /// Asking again freezes a still-streaming previous turn: its reads are
    /// generation-gated out from here on.
    pub fn ask(&mut self, question: String, target_label: String, now_ms: i64) {
        if let Some(prev) = self.turns.last_mut() {
            if prev.is_active() {
                prev.end(AnswerState::Done, now_ms);
            }
        }
        self.turns.push(Turn::new(question, target_label, now_ms));
        self.notice = None;
        self.expanded = true;
        self.poll_in_flight = false;
    }

    pub fn collapse(&mut self) {
        self.expanded = false;
    }

    pub fn reopen(&mut self) {
        self.expanded = self.has_content();
    }

    pub fn clear(&mut self) {
        self.turns.clear();
        self.poll_in_flight = false;
        self.notice = Some(Notice::ContextCleared);
        self.expanded = true;
    }

    /// Call when Clear context is requested; shows "clearing after the
    /// current answer" while a turn is still in flight.
    pub fn clear_requested(&mut self) {
        if self.turns.last().is_some_and(Turn::is_active) {
            self.notice = Some(Notice::ClearPending);
        }
    }

    pub fn clear_failed(&mut self, message: String) {
        self.notice = Some(Notice::Error(message));
        self.expanded = true;
    }

    pub fn latest_mut(&mut self) -> Option<&mut Turn<R>> {
        self.turns.last_mut()
    }

    pub fn status(&self, now_ms: i64) -> DrawerStatus {
        let Some(turn) = self.turns.last() else {
            return DrawerStatus::Idle;
        };
        if turn
            .relays
            .iter()
            .any(|r| turn.relay_state(r, now_ms) == RelayState::Waiting)
        {
            DrawerStatus::Relaying
        } else if turn.is_active() {
            DrawerStatus::Answering
        } else {
            DrawerStatus::Idle
        }
    }

    /// First line of the latest answer (or question), for the collapsed
    /// indicator.
    pub fn summary_line(&self) -> String {
        let Some(turn) = self.turns.last() else {
            return match &self.notice {
                Some(Notice::ContextCleared) => "context cleared".to_string(),
                Some(Notice::ClearPending) => CLEAR_PENDING_TEXT.to_string(),
                Some(Notice::Error(e)) => e.clone(),
                None => String::new(),
            };
        };
        let source = if turn.answer.trim().is_empty() {
            &turn.question
        } else {
            &turn.answer
        };
        let line = source
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        line.replace("**", "").replace('`', "")
    }
}

/// What one poll of the latest turn read.
#[derive(Debug, Clone)]
pub enum AnswerUpdate {
    Transcript(
        super::TranscriptDelta,
        super::TranscriptCursor,
    ),
    PromptPane {
        text: String,
        cursor: usize,
    },
    OrchestratorPane(String),
    Error(String),
    Nothing,
}

#[derive(Debug, Clone)]
pub struct PromptPoll {
    pub answer: AnswerUpdate,
    pub context_pct: Option<f64>,
    pub events: Vec<ninox_core::types::MessageEvent>,
}

impl<R: Default> ReplyDrawer<R> {
    /// Applies a poll to the latest turn; returns whether its answer text
    /// changed (so the caller re-parses its markdown). Relays only apply to
    /// prompt-agent turns — a picked orchestrator's own sends aren't the
    /// prompt agent's.
    pub fn apply_poll(&mut self, poll: PromptPoll, now_ms: i64) -> bool {
        self.poll_in_flight = false;
        self.context_pct = poll.context_pct;
        let Some(turn) = self.turns.last_mut() else {
            return false;
        };
        if matches!(
            turn.source,
            AnswerSource::Transcript(_) | AnswerSource::PromptPane { .. }
        ) {
            turn.sync_relays(&poll.events, None);
        }
        match poll.answer {
            AnswerUpdate::Transcript(delta, cursor) => {
                turn.source = AnswerSource::Transcript(cursor);
                turn.apply_transcript(delta, now_ms)
            }
            AnswerUpdate::PromptPane { text, cursor } => {
                turn.source = AnswerSource::PromptPane { cursor };
                turn.apply_pane_text(&text, true)
            }
            AnswerUpdate::OrchestratorPane(text) => turn.apply_pane_text(&text, false),
            AnswerUpdate::Error(message) => {
                turn.fail(message, now_ms);
                false
            }
            AnswerUpdate::Nothing => false,
        }
    }
}

/// The header status text: "answering · context 18% used", or just the
/// status when the harness doesn't report a context figure.
pub fn status_line(status: DrawerStatus, context_pct: Option<f64>) -> String {
    match context_pct {
        Some(pct) => format!("{} · context {}% used", status.label(), pct.round() as i64),
        None => status.label().to_string(),
    }
}

pub fn format_hhmm(at_ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(at_ms)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// The relay row's text after "Asked <target> (<role>) — ".
pub fn relay_tail(state: RelayState, relayed_at_ms: i64) -> String {
    let relayed = format_hhmm(relayed_at_ms);
    match state {
        RelayState::Waiting => format!("relayed {relayed}, waiting for its answer…"),
        RelayState::Replied(at) => format!("relayed {relayed}, replied {}", format_hhmm(at)),
        RelayState::NoDirectReply => format!("relayed {relayed} · no direct reply yet"),
    }
}

/// Delivers `text` to the prompt agent and picks where its answer will be
/// read from. The transcript is baselined before `ask` delivers, so a fast
/// first line can't land ahead of the cursor; a first-ever ask has no row
/// yet and reads its new transcript from the top. A non-claude harness is
/// read from its pane, baselined to "now" so the agent's scrollback (role
/// brief, earlier turns) never shows.
pub async fn deliver_to_agent(
    store: Arc<Store>,
    config: &AppConfig,
    text: &str,
    asked_at_ms: i64,
) -> Result<AnswerSource, String> {
    let baseline = store
        .get_session(super::PROMPT_SESSION_ID)
        .ok()
        .flatten()
        .and_then(|s| {
            Some(super::transcript_baseline(
                &super::transcript_dir_for(&s)?,
                s.claude_session_id.as_deref(),
                asked_at_ms,
            ))
        })
        .unwrap_or_default();
    let session_id = super::ask(store.clone(), config, text).await.map_err(|e| e.to_string())?;
    let session = store.get_session(&session_id).ok().flatten();
    if session.as_ref().and_then(super::transcript_dir_for).is_some() {
        Ok(AnswerSource::Transcript(baseline))
    } else {
        let cursor = ninox_core::runtime::read_screen(&session_id, None, false)
            .await
            .map(|s| s.len())
            .unwrap_or(0);
        Ok(AnswerSource::PromptPane { cursor })
    }
}

/// A picked orchestrator's last reply block just before delivery: the
/// previous answer, which must never show as this turn's.
pub async fn orchestrator_baseline(id: &str) -> String {
    ninox_core::runtime::read_screen(id, Some(ORCHESTRATOR_TAIL_LINES), false)
        .await
        .map(|s| last_reply_block(&s))
        .unwrap_or_default()
}

const ORCHESTRATOR_TAIL_LINES: usize = 40;

/// One poll of the latest turn's answer, plus the prompt agent's context
/// figure and recent message events (relays).
pub async fn poll(store: Arc<Store>, source: AnswerSource, asked_at_ms: i64) -> PromptPoll {
    let session = store.get_session(super::PROMPT_SESSION_ID).ok().flatten();
    let context_pct = session.as_ref().and_then(|s| s.context_used_pct);
    let events = store.recent_message_events(0).unwrap_or_default();
    let answer = match source {
        AnswerSource::Unresolved => AnswerUpdate::Nothing,
        AnswerSource::Transcript(cursor) => {
            match session.as_ref().and_then(|s| Some((super::transcript_dir_for(s)?, s))) {
                Some((dir, s)) => {
                    let id = s.claude_session_id.clone();
                    let read = tokio::task::spawn_blocking(move || {
                        super::transcript_since(&dir, id.as_deref(), &cursor, asked_at_ms)
                    })
                    .await;
                    match read {
                        Ok((delta, cursor)) => AnswerUpdate::Transcript(delta, cursor),
                        Err(e) => AnswerUpdate::Error(e.to_string()),
                    }
                }
                None => AnswerUpdate::Nothing,
            }
        }
        AnswerSource::PromptPane { cursor } => match super::reply_since(cursor).await {
            Ok((text, cursor)) => AnswerUpdate::PromptPane { text, cursor },
            Err(e) => AnswerUpdate::Error(e.to_string()),
        },
        AnswerSource::OrchestratorPane { id, .. } => {
            match ninox_core::runtime::read_screen(&id, Some(ORCHESTRATOR_TAIL_LINES), false).await {
                Ok(text) => AnswerUpdate::OrchestratorPane(text),
                Err(e) => AnswerUpdate::Error(e.to_string()),
            }
        }
    };
    PromptPoll { answer, context_pct, events }
}

/// The last Claude Code assistant block (`⏺`, U+23FA) in pane text: its
/// marker line plus the indented continuation lines under it. The LAST
/// block, because the agent's tools emit intermediate `⏺` actions before
/// its final answer, and a first-ever turn's text leads with the injected
/// role brief. Falls back to the whole trimmed text when no marker is
/// present (a non-Claude-Code harness).
pub fn last_reply_block(pane_text: &str) -> String {
    let lines: Vec<&str> = pane_text.lines().collect();
    let Some(start) = lines.iter().rposition(|l| l.trim_start().starts_with('⏺')) else {
        return pane_text.trim().to_string();
    };
    let mut block = vec![lines[start]
        .trim_start()
        .trim_start_matches('⏺')
        .trim()
        .to_string()];
    for line in &lines[start + 1..] {
        if !line.is_empty() && !line.starts_with("  ") {
            break;
        }
        block.push(
            line.strip_prefix("  ")
                .unwrap_or(line)
                .trim_end()
                .to_string(),
        );
    }
    block.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{MessageEvent, MessageKind};
    use super::super::TranscriptDelta;

    fn delta(blocks: &[&str], ended: Option<bool>) -> TranscriptDelta {
        TranscriptDelta {
            text_blocks: blocks.iter().map(|b| b.to_string()).collect(),
            ended,
        }
    }

    fn event(from: &str, to: &str, at: i64) -> MessageEvent {
        MessageEvent {
            id: at,
            from_id: Some(from.into()),
            to_id: to.into(),
            text: String::new(),
            delivered_at: at,
            kind: MessageKind::Freeform,
            outcome: Default::default(),
        }
    }

    #[test]
    fn ask_opens_the_drawer_with_a_pending_turn() {
        let mut d = ReplyDrawer::<()>::default();
        assert!(!d.is_open());
        d.ask("why?".into(), "ninox".into(), 1_000);
        assert!(d.is_open());
        assert_eq!(d.turns.len(), 1);
        assert_eq!(d.turns[0].state, AnswerState::Pending);
        assert_eq!(d.status(1_000), DrawerStatus::Answering);
    }

    #[test]
    fn collapse_then_reopen_keeps_history() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("one".into(), "ninox".into(), 1);
        d.latest_mut()
            .unwrap()
            .apply_transcript(delta(&["answer"], Some(true)), 2);
        d.collapse();
        assert!(!d.is_open());
        assert!(
            d.has_content(),
            "collapsed still shows the one-line indicator"
        );
        d.reopen();
        assert!(d.is_open());
        assert_eq!(d.turns[0].answer, "answer");
    }

    #[test]
    fn asking_again_reopens_a_collapsed_drawer_and_stacks_turns() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("one".into(), "ninox".into(), 1);
        d.collapse();
        d.ask("two".into(), "ninox".into(), 2);
        assert!(d.is_open());
        assert_eq!(
            d.turns
                .iter()
                .map(|t| t.question.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(
            d.turns[0].state,
            AnswerState::Done,
            "a superseded streaming turn is frozen"
        );
    }

    #[test]
    fn reopen_with_nothing_to_show_stays_closed() {
        let mut d = ReplyDrawer::<()>::default();
        d.reopen();
        assert!(!d.is_open());
    }

    #[test]
    fn clear_empties_history_and_shows_the_cleared_notice() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("one".into(), "ninox".into(), 1);
        d.collapse();
        d.clear();
        assert!(d.turns.is_empty());
        assert_eq!(d.notice, Some(Notice::ContextCleared));
        assert!(d.is_open());
        d.ask("two".into(), "ninox".into(), 2);
        assert_eq!(d.notice, None);
    }

    #[test]
    fn clear_failure_keeps_history() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("one".into(), "ninox".into(), 1);
        d.clear_failed("nope".into());
        assert_eq!(d.turns.len(), 1);
        assert_eq!(d.notice, Some(Notice::Error("nope".into())));
    }

    #[test]
    fn transcript_answer_grows_in_place_then_ends() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 1);
        let t = d.latest_mut().unwrap();
        assert!(t.apply_transcript(delta(&["Looking…"], Some(false)), 2));
        assert_eq!(t.state, AnswerState::Streaming);
        assert!(
            !t.apply_transcript(delta(&[], None), 3),
            "a quiet poll changes nothing"
        );
        assert!(t.apply_transcript(delta(&["**Done.**"], Some(true)), 4));
        assert_eq!(t.answer, "Looking…\n\n**Done.**");
        assert_eq!(t.state, AnswerState::Done);
        assert_eq!(t.ended_at_ms, Some(4));
        assert!(t.apply_transcript(delta(&["Relay answered: yes."], Some(false)), 5));
        assert_eq!(
            t.state,
            AnswerState::Streaming,
            "the agent resumed the turn"
        );
        assert_eq!(t.ended_at_ms, None);
    }

    #[test]
    fn status_is_idle_once_the_latest_turn_ends() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 1);
        d.latest_mut()
            .unwrap()
            .apply_transcript(delta(&["a"], Some(true)), 2);
        assert_eq!(d.status(3), DrawerStatus::Idle);
    }

    #[test]
    fn relays_wait_then_resolve_on_a_direct_reply() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("is #1099 blocked?".into(), "ninox".into(), 100);
        let events = vec![
            event(PROMPT, "orch-1", 50), // before this turn
            event(PROMPT, "orch-1", 150),
            event(PROMPT, "orch-1", 160), // duplicate target collapses
        ];
        let t = d.latest_mut().unwrap();
        t.sync_relays(&events, None);
        assert_eq!(
            t.relays,
            vec![Relay {
                to_id: "orch-1".into(),
                relayed_at_ms: 150,
                replied_at_ms: None
            }]
        );
        assert_eq!(d.status(200), DrawerStatus::Relaying);

        let mut events = events;
        events.push(event("orch-1", PROMPT, 300));
        let t = d.latest_mut().unwrap();
        t.sync_relays(&events, None);
        assert_eq!(t.relay_state(&t.relays[0], 400), RelayState::Replied(300));
    }

    #[test]
    fn an_unanswered_relay_times_out_after_the_turn_ends() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 100);
        let t = d.latest_mut().unwrap();
        t.sync_relays(&[event(PROMPT, "w-1", 150)], None);
        t.apply_transcript(delta(&["Relayed to w-1."], Some(true)), 1_000);
        let r = t.relays[0].clone();
        assert_eq!(
            t.relay_state(&r, 1_000 + RELAY_REPLY_GRACE_MS - 1),
            RelayState::Waiting
        );
        assert_eq!(
            t.relay_state(&r, 1_000 + RELAY_REPLY_GRACE_MS),
            RelayState::NoDirectReply
        );
        assert_eq!(d.status(1_000 + RELAY_REPLY_GRACE_MS), DrawerStatus::Idle);
    }

    #[test]
    fn relays_ignore_sends_from_other_sessions_and_after_the_next_ask() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 100);
        let t = d.latest_mut().unwrap();
        t.sync_relays(
            &[event("orch-2", "w-1", 150), event(PROMPT, "w-1", 500)],
            Some(400),
        );
        assert!(t.relays.is_empty());
    }

    #[test]
    fn pane_text_extracts_the_last_assistant_block() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "Orch".into(), 1);
        let t = d.latest_mut().unwrap();
        assert!(t.apply_pane_text(
            "brief\n⏺ Bash(ninox list)\n  ⎿ out\n⏺ It is **fine**.\n  Second line.\n\n✻ Done\n",
            false
        ));
        assert_eq!(t.answer, "It is **fine**.\nSecond line.");
        assert!(
            !t.apply_pane_text("", true),
            "an empty delta never blanks the answer"
        );
        assert_eq!(t.answer, "It is **fine**.\nSecond line.");
    }

    #[test]
    fn last_reply_block_falls_back_to_plain_text() {
        assert_eq!(last_reply_block("\n  plain answer \n"), "plain answer");
        assert_eq!(last_reply_block(""), "");
    }

    #[test]
    fn summary_line_prefers_the_answer() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("question".into(), "ninox".into(), 1);
        assert_eq!(d.summary_line(), "question");
        d.latest_mut()
            .unwrap()
            .apply_transcript(delta(&["\nFour orchestrators.\nMore."], Some(true)), 2);
        assert_eq!(d.summary_line(), "Four orchestrators.");
        d.latest_mut()
            .unwrap()
            .apply_transcript(delta(&[], None), 3);
        d.ask("q2".into(), "ninox".into(), 4);
        d.latest_mut().unwrap().apply_transcript(
            delta(&["Relayed to **Bellhop** via `ninox send`."], Some(true)),
            5,
        );
        assert_eq!(
            d.summary_line(),
            "Relayed to Bellhop via ninox send.",
            "the one-line indicator is plain text"
        );
    }

    #[test]
    fn apply_poll_updates_context_relays_and_the_transcript_cursor() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 100);
        d.latest_mut().unwrap().source = AnswerSource::Transcript(Default::default());
        let cursor = super::super::TranscriptCursor {
            path: Some("/t.jsonl".into()),
            offset: 42,
        };
        let changed = d.apply_poll(
            PromptPoll {
                answer: AnswerUpdate::Transcript(delta(&["hi"], Some(true)), cursor.clone()),
                context_pct: Some(18.0),
                events: vec![event(PROMPT, "o1", 150)],
            },
            200,
        );
        assert!(changed);
        assert_eq!(d.context_pct, Some(18.0));
        assert_eq!(d.turns[0].source, AnswerSource::Transcript(cursor));
        assert_eq!(d.turns[0].relays.len(), 1);
    }

    #[test]
    fn apply_poll_never_attributes_relays_to_a_picked_orchestrator_turn() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "Orch".into(), 100);
        d.latest_mut().unwrap().source = AnswerSource::OrchestratorPane {
            id: "o1".into(),
            baseline: String::new(),
        };
        d.apply_poll(
            PromptPoll {
                answer: AnswerUpdate::OrchestratorPane("⏺ ok".into()),
                context_pct: None,
                events: vec![event(PROMPT, "o2", 150)],
            },
            200,
        );
        assert!(d.turns[0].relays.is_empty());
        assert_eq!(d.turns[0].answer, "ok");
    }

    #[test]
    fn apply_poll_error_fails_the_turn() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 100);
        d.apply_poll(
            PromptPoll {
                answer: AnswerUpdate::Error("boom".into()),
                context_pct: None,
                events: vec![],
            },
            200,
        );
        assert_eq!(d.turns[0].state, AnswerState::Failed);
        assert_eq!(d.turns[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn a_picked_orchestrators_previous_reply_never_answers_the_new_question() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "Orch".into(), 1);
        let t = d.latest_mut().unwrap();
        t.source = AnswerSource::OrchestratorPane {
            id: "o1".into(),
            baseline: "old answer".into(),
        };
        assert!(!t.apply_pane_text("⏺ old answer\n", false));
        assert_eq!(t.state, AnswerState::Pending);
        assert!(t.apply_pane_text("⏺ old answer\n> q\n⏺ new answer\n", false));
        assert_eq!(t.answer, "new answer");
    }

    #[test]
    fn relays_survive_their_events_aging_out_of_the_store() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 100);
        let t = d.latest_mut().unwrap();
        t.sync_relays(&[event(PROMPT, "o1", 150)], None);
        t.sync_relays(&[], None);
        assert_eq!(t.relays.len(), 1, "a pruned relay stays visible");
        t.sync_relays(&[event("o1", PROMPT, 300)], None);
        assert_eq!(
            t.relays[0].replied_at_ms,
            Some(300),
            "a reply still resolves it"
        );
    }

    #[test]
    fn polling_stops_once_the_turn_and_its_relays_are_settled() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 100);
        assert!(
            !d.turns[0].wants_poll(100),
            "nothing to poll before delivery"
        );
        let t = d.latest_mut().unwrap();
        t.source = AnswerSource::Transcript(Default::default());
        assert!(t.wants_poll(100));
        t.apply_transcript(delta(&["done"], Some(true)), 1_000);
        assert!(
            t.wants_poll(1_000 + RELAY_REPLY_GRACE_MS - 1),
            "the agent may still resume"
        );
        assert!(!t.wants_poll(1_000 + RELAY_REPLY_GRACE_MS));
        t.sync_relays(&[event(PROMPT, "o1", 150)], None);
        assert!(
            !t.wants_poll(1_000 + RELAY_REPLY_GRACE_MS),
            "a timed-out relay stops polling too"
        );
    }

    #[test]
    fn a_poll_clears_the_in_flight_flag_and_a_new_ask_resets_it() {
        let mut d = ReplyDrawer::<()>::default();
        d.ask("q".into(), "ninox".into(), 1);
        d.poll_in_flight = true;
        d.apply_poll(
            PromptPoll {
                answer: AnswerUpdate::Nothing,
                context_pct: None,
                events: vec![],
            },
            2,
        );
        assert!(!d.poll_in_flight);
        d.poll_in_flight = true;
        d.ask("q2".into(), "ninox".into(), 3);
        assert!(
            !d.poll_in_flight,
            "the old poll's result is dropped by generation"
        );
    }
    #[test]
    fn status_line_omits_an_unknown_context_figure() {
        assert_eq!(status_line(DrawerStatus::Answering, Some(18.4)), "answering · context 18% used");
        assert_eq!(status_line(DrawerStatus::Idle, None), "idle");
    }

    #[test]
    fn relay_tail_matches_the_board_wording() {
        let at = chrono::Local::now().timestamp_millis();
        let hhmm = format_hhmm(at);
        assert_eq!(relay_tail(RelayState::Waiting, at), format!("relayed {hhmm}, waiting for its answer…"));
        assert_eq!(relay_tail(RelayState::NoDirectReply, at), format!("relayed {hhmm} · no direct reply yet"));
        assert_eq!(relay_tail(RelayState::Replied(at), at), format!("relayed {hhmm}, replied {hhmm}"));
    }

    const PROMPT: &str = ninox_core::worker_type::PROMPT_SESSION_ID;

    #[test]
    fn thinking_lasts_from_the_ask_until_the_first_answer_text() {
        let mut d: ReplyDrawer = ReplyDrawer::default();
        assert!(!d.shows_thinking());
        d.ask("q".into(), "ninox".into(), 0);
        assert!(d.shows_thinking(), "pending");
        d.latest_mut().unwrap().state = AnswerState::Streaming;
        assert!(d.shows_thinking(), "streaming, no text yet");
        d.latest_mut().unwrap().answer = "first words".into();
        assert!(!d.shows_thinking(), "text is streaming in");
        d.latest_mut().unwrap().answer.clear();
        d.collapse();
        assert!(!d.shows_thinking(), "a collapsed drawer shows no placeholder");
        d.reopen();
        d.latest_mut().unwrap().fail("nope".into(), 1);
        assert!(!d.shows_thinking());
    }
}
