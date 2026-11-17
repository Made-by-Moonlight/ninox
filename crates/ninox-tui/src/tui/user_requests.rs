//! The pending-requests screen's key handler.
//!
//! `Action::AnswerUserRequest` (`ninox-tui`'s only path to
//! `Store::answer_user_request`) is built in exactly one place below:
//! `draft_key`'s `Enter` arm. That arm only ever runs once
//! `user_requests_draft` holds `Some(..)`, and the only place that field is
//! ever set to `Some` is `open_selected_request`'s `UserRequestMode::Async`
//! arm — the `Sync` arm only calls `st.notify(..)`. So a `sync` row has no
//! code path to an answer at all: nothing here ever constructs
//! `Action::AnswerUserRequest` from a `Sync` selection. The draft carries
//! the id it was opened on, and submit re-resolves that id against the
//! current list, so a reorder under an open draft can't redirect it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ninox_core::types::{UserRequestMode, UserRequestStatus};

use super::state::{Action, Level, Modal, TuiState, UserRequestDraft, View};

/// Whether `session_id` is independently confirmed unable to receive an
/// answer, looked up fresh against `st.rows` — never on the human's say-so
/// alone: its status is terminal (`SessionStatus::is_terminal()`), or it's
/// gone from the store entirely (`refresh` loads requests before sessions,
/// so a live asker is always present). Gates both the stale badge
/// (`view::draw_user_requests`) and the dismiss key below.
pub(crate) fn requester_is_stale(st: &TuiState, session_id: &str) -> bool {
    st.rows.iter().find(|r| r.id() == session_id).is_none_or(|r| r.session.status.is_terminal())
}

fn move_cursor(st: &mut TuiState, delta: i64) {
    if st.pending_user_requests.is_empty() {
        return;
    }
    let max = st.pending_user_requests.len() as i64 - 1;
    st.user_requests_selected = (st.user_requests_selected as i64 + delta).clamp(0, max) as usize;
}

/// Enter on the selected row: `sync` only ever gets a read-only notice;
/// only `async` opens the draft input.
fn open_selected_request(st: &mut TuiState) {
    let Some(row) = st.pending_user_requests.get(st.user_requests_selected).cloned() else { return };
    match row.mode {
        UserRequestMode::Sync => st.notify(Level::Info, "sync requests can only be answered from ninox-gui".to_string()),
        UserRequestMode::Async => st.user_requests_draft = Some(UserRequestDraft { id: row.id, buf: String::new() }),
    }
}

/// The draft sub-mode: captures every subsequent key into the draft until
/// Enter submits or Esc cancels. The only place this crate builds
/// `Action::AnswerUserRequest`.
fn draft_key(st: &mut TuiState, key: KeyEvent, mut draft: UserRequestDraft) -> Action {
    match key.code {
        KeyCode::Esc => {}
        KeyCode::Enter => {
            // An empty answer would permanently close the request with
            // nothing in it — keep the draft open instead.
            if draft.buf.trim().is_empty() {
                st.user_requests_draft = Some(draft);
                return Action::None;
            }
            // Re-resolved by id, never by `user_requests_selected`: the list
            // may have reordered since the draft opened. A row that has
            // vanished (answered/dismissed elsewhere) drops the draft.
            // `open_selected_request` only ever opens a draft from its
            // `Async` arm — the mode check documents that invariant rather
            // than relying on it.
            let Some(row) = st.pending_user_requests.iter().find(|r| r.id == draft.id) else { return Action::None };
            if row.mode != UserRequestMode::Async || row.status != UserRequestStatus::Pending {
                return Action::None;
            }
            return Action::AnswerUserRequest { id: draft.id, answer: draft.buf };
        }
        KeyCode::Backspace => {
            draft.buf.pop();
            st.user_requests_draft = Some(draft);
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.buf.clear();
            st.user_requests_draft = Some(draft);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.buf.push(c);
            st.user_requests_draft = Some(draft);
        }
        _ => st.user_requests_draft = Some(draft),
    }
    Action::None
}

/// `x` on the selected row: dismisses it, but only once its requester's
/// live session status is independently confirmed terminal — never on the
/// human's say-so alone while the session still looks alive. A no-op
/// otherwise, so there is no reachable key sequence that cancels a row
/// whose requester might still be running.
fn dismiss_selected(st: &TuiState) -> Action {
    let Some(row) = st.pending_user_requests.get(st.user_requests_selected) else { return Action::None };
    if !requester_is_stale(st, &row.requester_session_id) {
        return Action::None;
    }
    Action::CancelUserRequest { id: row.id.clone() }
}

pub fn user_requests_key(st: &mut TuiState, key: KeyEvent) -> Action {
    if let Some(draft) = st.user_requests_draft.take() {
        return draft_key(st, key, draft);
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('u') => st.view = View::Fleet,
        KeyCode::Char('j') | KeyCode::Down => move_cursor(st, 1),
        KeyCode::Char('k') | KeyCode::Up => move_cursor(st, -1),
        KeyCode::Char('g') | KeyCode::Home => move_cursor(st, i64::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => move_cursor(st, i64::MAX / 2),
        KeyCode::Char('?') => st.modal = Some(Modal::Help),
        KeyCode::Enter => open_selected_request(st),
        KeyCode::Char('x') => return dismiss_selected(st),
        _ => {}
    }
    Action::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::store::UserRequestRow;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn row(id: &str, mode: UserRequestMode) -> UserRequestRow {
        UserRequestRow {
            id: id.into(),
            requester_session_id: "w1".into(),
            prompt: "merge now?".into(),
            mode,
            status: UserRequestStatus::Pending,
            answer: None,
            answered_at: None,
            answer_delivered_at: None,
            created_at: 0,
        }
    }

    fn state_with(rows: Vec<UserRequestRow>) -> TuiState {
        TuiState { view: View::UserRequests, pending_user_requests: rows, ..Default::default() }
    }

    fn session_row(id: &str, status: ninox_core::types::SessionStatus) -> crate::tui::state::Row {
        crate::tui::state::Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, None, status),
            is_orchestrator: false,
            group: None,
            machine_health: None,
            queue_position: None, prs: Vec::new(),
        }
    }

    #[test]
    fn sync_row_has_no_answer_key_path() {
        let mut st = state_with(vec![row("ur-1", UserRequestMode::Sync)]);

        // Enter on a sync row must only notify — never enter the draft
        // sub-mode and never return an answering Action.
        assert_eq!(user_requests_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert!(st.user_requests_draft.is_none(), "a sync row must never open the draft input");
        assert!(st.notice.is_some(), "a sync row gets a read-only notice");

        // Exhaustively drive every key a human could plausibly press next —
        // none of them may produce an answering Action, since there is no
        // draft state for them to be captured into.
        for k in ['a', 'y', 'n', 'e', 's']
            .into_iter()
            .map(key)
            .chain([code(KeyCode::Enter), code(KeyCode::Backspace), code(KeyCode::Esc)])
        {
            let action = user_requests_key(&mut st, k);
            assert_ne!(
                std::mem::discriminant(&action),
                std::mem::discriminant(&Action::AnswerUserRequest { id: String::new(), answer: String::new() }),
                "no key sequence may answer a sync row"
            );
            assert!(st.user_requests_draft.is_none(), "a sync row must never enter the draft sub-mode");
        }
    }

    #[test]
    fn async_row_enter_then_submit_returns_answer_action() {
        let mut st = state_with(vec![row("ur-2", UserRequestMode::Async)]);

        assert_eq!(user_requests_key(&mut st, code(KeyCode::Enter)), Action::None);
        let draft_buf = |st: &TuiState| st.user_requests_draft.as_ref().map(|d| d.buf.clone());
        assert_eq!(draft_buf(&st).as_deref(), Some(""), "Enter on an async row opens the draft input");

        for c in "yes".chars() {
            assert_eq!(user_requests_key(&mut st, key(c)), Action::None);
        }
        assert_eq!(draft_buf(&st).as_deref(), Some("yes"));

        assert_eq!(
            user_requests_key(&mut st, code(KeyCode::Enter)),
            Action::AnswerUserRequest { id: "ur-2".into(), answer: "yes".into() }
        );
        assert!(st.user_requests_draft.is_none(), "submitting closes the draft input");
    }

    #[test]
    fn esc_cancels_the_draft_without_answering() {
        let mut st = state_with(vec![row("ur-3", UserRequestMode::Async)]);
        user_requests_key(&mut st, code(KeyCode::Enter));
        user_requests_key(&mut st, key('x'));
        assert_eq!(user_requests_key(&mut st, code(KeyCode::Esc)), Action::None);
        assert!(st.user_requests_draft.is_none());
    }

    #[test]
    fn navigation_moves_and_clamps() {
        let mut st = state_with(vec![
            row("ur-a", UserRequestMode::Async),
            row("ur-b", UserRequestMode::Sync),
            row("ur-c", UserRequestMode::Async),
        ]);
        user_requests_key(&mut st, key('j'));
        user_requests_key(&mut st, key('j'));
        assert_eq!(st.user_requests_selected, 2);
        user_requests_key(&mut st, key('j'));
        assert_eq!(st.user_requests_selected, 2, "clamped at the end");
        user_requests_key(&mut st, key('g'));
        assert_eq!(st.user_requests_selected, 0);
        user_requests_key(&mut st, key('k'));
        assert_eq!(st.user_requests_selected, 0, "clamped at the start");
    }

    #[test]
    fn u_and_q_and_esc_leave_the_screen() {
        for k in [key('u'), key('q'), code(KeyCode::Esc)] {
            let mut st = state_with(vec![]);
            user_requests_key(&mut st, k);
            assert_eq!(st.view, View::Fleet);
        }
    }

    #[test]
    fn enter_on_an_empty_list_does_nothing() {
        let mut st = state_with(vec![]);
        assert_eq!(user_requests_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert!(st.user_requests_draft.is_none());
    }

    #[test]
    fn dismiss_key_cancels_a_stale_row_never_answers() {
        use ninox_core::types::SessionStatus;
        let mut st = state_with(vec![row("ur-1", UserRequestMode::Async)]);
        st.rows = vec![session_row("w1", SessionStatus::Terminated)];

        assert!(requester_is_stale(&st, "w1"), "the requester must register as independently confirmed terminal");
        assert_eq!(
            user_requests_key(&mut st, key('x')),
            Action::CancelUserRequest { id: "ur-1".into() },
        );
        assert_ne!(
            std::mem::discriminant(&user_requests_key(&mut st, key('x'))),
            std::mem::discriminant(&Action::AnswerUserRequest { id: String::new(), answer: String::new() }),
            "dismissal must never answer",
        );
    }

    #[test]
    fn dismiss_key_is_a_noop_for_a_live_requester() {
        use ninox_core::types::SessionStatus;
        let mut st = state_with(vec![row("ur-1", UserRequestMode::Async)]);
        st.rows = vec![session_row("w1", SessionStatus::Working)];

        assert!(!requester_is_stale(&st, "w1"));
        assert_eq!(user_requests_key(&mut st, key('x')), Action::None);
    }

    #[test]
    fn dismiss_key_cancels_a_row_whose_requester_no_longer_exists() {
        use ninox_core::types::SessionStatus;
        // The asker was purged from the store entirely — nothing can ever
        // deliver to it, so the row must not be stuck undismissable.
        let mut st = state_with(vec![row("ur-1", UserRequestMode::Async)]);
        st.rows = vec![session_row("someone-else", SessionStatus::Working)];

        assert!(requester_is_stale(&st, "w1"));
        assert_eq!(user_requests_key(&mut st, key('x')), Action::CancelUserRequest { id: "ur-1".into() });
    }

    #[test]
    fn a_reorder_under_an_open_draft_still_submits_to_the_request_it_was_opened_on() {
        let mut st = state_with(vec![row("ur-a", UserRequestMode::Async), row("ur-b", UserRequestMode::Async)]);
        user_requests_key(&mut st, key('j'));
        user_requests_key(&mut st, code(KeyCode::Enter));
        for c in "for b".chars() {
            user_requests_key(&mut st, key(c));
        }

        // A refresh tick: ur-a was answered elsewhere and a new row sorted
        // in ahead, so index 1 (still `user_requests_selected`) is now ur-c.
        st.pending_user_requests =
            vec![row("ur-new", UserRequestMode::Async), row("ur-c", UserRequestMode::Async), row("ur-b", UserRequestMode::Async)];
        assert_eq!(
            user_requests_key(&mut st, code(KeyCode::Enter)),
            Action::AnswerUserRequest { id: "ur-b".into(), answer: "for b".into() },
        );
    }

    #[test]
    fn a_draft_whose_request_vanished_is_dropped_not_redirected() {
        let mut st = state_with(vec![row("ur-a", UserRequestMode::Async), row("ur-b", UserRequestMode::Async)]);
        user_requests_key(&mut st, code(KeyCode::Enter));
        user_requests_key(&mut st, key('y'));

        // ur-a resolved elsewhere; index 0 now holds a different async row.
        st.pending_user_requests = vec![row("ur-b", UserRequestMode::Async)];
        assert_eq!(user_requests_key(&mut st, code(KeyCode::Enter)), Action::None);
        assert!(st.user_requests_draft.is_none());
    }

    #[test]
    fn an_empty_or_blank_draft_is_never_submitted() {
        let mut st = state_with(vec![row("ur-1", UserRequestMode::Async)]);
        user_requests_key(&mut st, code(KeyCode::Enter));
        assert_eq!(user_requests_key(&mut st, code(KeyCode::Enter)), Action::None, "empty");
        user_requests_key(&mut st, key(' '));
        assert_eq!(user_requests_key(&mut st, code(KeyCode::Enter)), Action::None, "whitespace only");
        assert!(st.user_requests_draft.is_some(), "the draft stays open for a real answer");

        user_requests_key(&mut st, key('y'));
        assert_eq!(
            user_requests_key(&mut st, code(KeyCode::Enter)),
            Action::AnswerUserRequest { id: "ur-1".into(), answer: " y".into() },
        );
    }
}
