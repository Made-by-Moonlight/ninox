//! The list of every PR a session is responsible for, shared by the
//! session side panel and the fleet detail panel. The design artifacts
//! only draw a single PR, so each entry mirrors that single-PR row: a
//! clickable title over a one-line state and checks summary.

use iced::{
    widget::{rich_text, span, text, Space},
    Color, Element,
};

use ninox_core::{
    session_prs::SessionPr,
};

use crate::{app::Message, style::SANS};

pub use super::pr_chip::tone as state_color;

/// Link text and detail line for one entry.
pub fn entry_text(pr: &SessionPr) -> (String, String) {
    let title = if pr.title.is_empty() { pr.label() } else { format!("{} {}", pr.label(), pr.title) };
    let mut detail = pr.state_line();
    if pr.is_open() {
        if let Some(checks) = pr.checks_line() {
            detail.push_str(" · ");
            detail.push_str(&checks);
        }
    }
    (title, detail)
}

/// `PULL REQUEST` heading text: the count appears once there is more than one.
pub fn heading(count: usize) -> String {
    if count > 1 { format!("PULL REQUEST · {count}") } else { "PULL REQUEST".to_string() }
}

pub fn rows<'a>(prs: &'a [SessionPr], link: Color, title_size: f32, detail_size: f32) -> Vec<Element<'a, Message>> {
    let mut items: Vec<Element<Message>> = Vec::new();
    for (i, pr) in prs.iter().enumerate() {
        if i > 0 {
            items.push(Space::new(0, 8).into());
        }
        let (title, detail) = entry_text(pr);
        items.push(
            rich_text![span(title).color(link).underline(true).link(Message::OpenUrl(pr.url.clone()))]
                .size(title_size)
                .font(SANS)
                .into(),
        );
        items.push(text(detail).size(detail_size).font(SANS).color(state_color(pr)).into());
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::shell::tokens;
    use ninox_core::{pr_events::CiVerdict, session_prs::PrState};

    fn pr(state: PrState) -> SessionPr {
        SessionPr {
            repo: "o/bellhop".into(), number: 1466, url: "u".into(), title: "Spoke sizing".into(), state,
            draft: false, ci: CiVerdict::Pending, passing: 28, failing: 0, pending: 25,
            changes_requested: false, approved: false, tracked: true,
        }
    }

    #[test]
    fn open_entry_shows_label_title_state_and_checks() {
        let (title, detail) = entry_text(&pr(PrState::Open));
        assert_eq!(title, "bellhop#1466 Spoke sizing");
        assert_eq!(detail, "open · 28/53 checks passing");
    }

    #[test]
    fn merged_entry_drops_the_checks_and_untitled_falls_back_to_the_label() {
        let mut p = pr(PrState::Merged);
        p.title.clear();
        assert_eq!(entry_text(&p), ("bellhop#1466".to_string(), "merged".to_string()));
    }

    #[test]
    fn colors_flag_failing_pending_and_merged() {
        let mut p = pr(PrState::Open);
        assert_eq!(state_color(&p), tokens::BLUE);
        p.failing = 1;
        assert_eq!(state_color(&p), tokens::RED);
        assert_eq!(state_color(&pr(PrState::Merged)), tokens::VIOLET_BADGE_FG);
    }

    #[test]
    fn heading_counts_only_beyond_one() {
        assert_eq!(heading(1), "PULL REQUEST");
        assert_eq!(heading(4), "PULL REQUEST · 4");
    }

    #[test]
    fn rows_emit_two_lines_per_pr_plus_spacers() {
        let list = [pr(PrState::Open), pr(PrState::Merged)];
        assert_eq!(rows(&list, tokens::BLUE, 12.0, 11.0).len(), 5);
    }
}
