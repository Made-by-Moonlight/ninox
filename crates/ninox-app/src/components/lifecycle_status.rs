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
}
