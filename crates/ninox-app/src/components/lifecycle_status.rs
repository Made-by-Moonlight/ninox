use iced::{
    widget::{column, container, text, tooltip},
    Background, Border, Element, Length,
};

use crate::{app::Message, style::shadow_alpha, theme::ColorScheme};

use ninox_core::types::{GateCheck, Session};

/// Plain-English breakdown of a session's current gate state, one line per
/// check — what the hover tooltip shows. `Mergeable`'s line explains *why*
/// when it's blocked, rather than just repeating "no."
pub fn gate_lines(session: &Session) -> Vec<String> {
    let Some(gate) = &session.gate_status else {
        return vec!["No PR opened yet".to_string()];
    };

    let ci_word = match gate.ci {
        GateCheck::Passing => "passing",
        GateCheck::Failing => "failing",
        GateCheck::Pending => "pending",
        GateCheck::Unknown => "unknown",
    };
    let review_word = match gate.review {
        GateCheck::Passing => "approved",
        GateCheck::Failing => "changes requested",
        GateCheck::Pending => "pending",
        GateCheck::Unknown => "unknown",
    };
    let mergeable_line = match gate.mergeable {
        GateCheck::Passing => "Mergeable — yes".to_string(),
        GateCheck::Unknown => "Mergeable — unknown".to_string(),
        GateCheck::Pending => "Mergeable — pending".to_string(),
        GateCheck::Failing => {
            if matches!(gate.ci, GateCheck::Failing) {
                "Mergeable — blocked on CI".to_string()
            } else if matches!(gate.review, GateCheck::Failing) {
                "Mergeable — blocked on review".to_string()
            } else {
                "Mergeable — no".to_string()
            }
        }
    };

    vec![
        format!("CI — {ci_word}"),
        format!("Review — {review_word}"),
        mergeable_line,
    ]
}

/// Wraps `content` (typically a status dot/stamp) so hovering it shows a
/// plain-English gate breakdown — reuses `brain_panel.rs`'s hover-slip
/// styling (paper_2 background, ink border, hard drop shadow) but via
/// Iced's built-in `tooltip` widget rather than manual hover-state
/// tracking, since these rows are ordinary widget trees (not a canvas).
pub fn with_gate_tooltip<'a>(
    s:       &'a ColorScheme,
    session: &'a Session,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    let (card_a, _, _) = shadow_alpha(s);
    let lines: Vec<Element<Message>> = gate_lines(session)
        .into_iter()
        .map(|line| text(line).size(11).font(crate::style::SANS).color(s.ink_2).into())
        .collect();

    let body = container(column(lines).spacing(3))
        .width(Length::Fixed(200.0))
        .padding([10, 12])
        .style(move |_theme| container::Style {
            background: Some(Background::Color(s.paper_2)),
            border: Border { color: s.ink, width: 1.5, radius: 2.0.into() },
            shadow: crate::style::hard_shadow(s, 3.0, 3.0, card_a),
            ..Default::default()
        });

    tooltip(content, body, tooltip::Position::Bottom)
        .gap(6)
        .into()
}

/// "Removing in Nd/Nh/Nm" for a terminal session sitting in its retention
/// grace period, or `None` if there's nothing to show (no `terminal_at`,
/// i.e. the session isn't terminal, or was terminated by direct user
/// action and has no grace period at all).
pub fn retention_label(terminal_at: i64, retention_millis: i64, now: i64) -> Option<String> {
    let remaining_ms = terminal_at + retention_millis - now;
    Some(if remaining_ms <= 0 {
        "Removing shortly".to_string()
    } else {
        format!("Removing in {}", humanize_duration(remaining_ms))
    })
}

/// Exactly one unit, the coarsest that's still >= 1: days, else hours,
/// else minutes.
fn humanize_duration(ms: i64) -> String {
    const MINUTE: i64 = 60_000;
    const HOUR:   i64 = 60 * MINUTE;
    const DAY:    i64 = 24 * HOUR;
    if ms >= DAY {
        format!("{}d", ms / DAY)
    } else if ms >= HOUR {
        format!("{}h", ms / HOUR)
    } else {
        format!("{}m", (ms / MINUTE).max(1))
    }
}

/// Wall-clock "now" in epoch milliseconds, for the retention countdown.
/// `ninox_core::lifecycle::poller::now_millis()` exists but is
/// `pub(crate)` — deliberately scoped to `ninox-core` — so this is a
/// small private equivalent for this UI display concern rather than
/// widening that core-internal helper's visibility.
pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{GateStatus, SessionStatus};

    fn session_with(status: SessionStatus, gate: Option<GateStatus>) -> Session {
        Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status, agent_type: "c".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None,
            pid: None, model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: gate,
        }
    }

    #[test]
    fn gate_lines_before_any_pr_explains_no_gate_yet() {
        let session = session_with(SessionStatus::Working, None);
        let lines = gate_lines(&session);
        assert_eq!(lines, vec!["No PR opened yet".to_string()]);
    }

    #[test]
    fn gate_lines_renders_each_check_plainly() {
        let gate = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Failing,
            mergeable: GateCheck::Failing, since: 0,
        };
        let session = session_with(SessionStatus::ReviewPending, Some(gate));
        let lines = gate_lines(&session);
        assert_eq!(lines, vec![
            "CI — passing".to_string(),
            "Review — changes requested".to_string(),
            "Mergeable — blocked on review".to_string(),
        ]);
    }

    #[test]
    fn gate_lines_explains_ci_blocking_mergeable() {
        let gate = GateStatus {
            ci: GateCheck::Failing, review: GateCheck::Passing,
            mergeable: GateCheck::Failing, since: 0,
        };
        let session = session_with(SessionStatus::CiFailed, Some(gate));
        let lines = gate_lines(&session);
        assert_eq!(lines, vec![
            "CI — failing".to_string(),
            "Review — approved".to_string(),
            "Mergeable — blocked on CI".to_string(),
        ]);
    }

    #[test]
    fn gate_lines_reports_passing_mergeable_directly() {
        let gate = GateStatus {
            ci: GateCheck::Passing, review: GateCheck::Passing,
            mergeable: GateCheck::Passing, since: 0,
        };
        let session = session_with(SessionStatus::Mergeable, Some(gate));
        let lines = gate_lines(&session);
        assert_eq!(lines, vec![
            "CI — passing".to_string(),
            "Review — approved".to_string(),
            "Mergeable — yes".to_string(),
        ]);
    }

    #[test]
    fn retention_label_shows_days_when_more_than_a_day_remains() {
        let label = retention_label(0, 2 * 86_400_000, 86_400_000 /* now = 1 day later */);
        assert_eq!(label, Some("Removing in 1d".to_string()));
    }

    #[test]
    fn retention_label_shows_hours_under_a_day() {
        let label = retention_label(0, 86_400_000, 68_400_000 /* now = 19h later, 5h left */);
        assert_eq!(label, Some("Removing in 5h".to_string()));
    }

    #[test]
    fn retention_label_shows_minutes_under_an_hour() {
        let label = retention_label(0, 3_600_000, 3_300_000 /* now = 55m later, 5m left */);
        assert_eq!(label, Some("Removing in 5m".to_string()));
    }

    #[test]
    fn retention_label_past_the_window_says_shortly() {
        let label = retention_label(0, 3_600_000, 3_700_000 /* now past the window */);
        assert_eq!(label, Some("Removing shortly".to_string()));
    }
}
