//! Top-level Workers view: every live worker grouped by orchestrator, with
//! its hook-reported activity state (see `ninox_core::worker_status`) and
//! its dependency edges (declared + PR-stacked, from `session_deps`).

use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use crate::app::{App, Message};
use ninox_core::types::{ActivityState, DepKind, Session, SessionDep};

/// Compact "how long has it been in this state" stamp: `just now`, `4m`,
/// `2h 05m`, `3d`.
pub fn since_label(since_ms: i64, now_ms: i64) -> String {
    let mins = (now_ms - since_ms).max(0) / 60_000;
    match mins {
        0 => "just now".to_string(),
        1..=59 => format!("{mins}m"),
        60..=1439 => format!("{}h {:02}m", mins / 60, mins % 60),
        _ => format!("{}d", mins / 1440),
    }
}

/// The word on the activity badge. `can_report = false` (no status hooks in
/// the worktree — pre-feature worktree, checked-in settings, non-Claude
/// harness) turns an uninformative `unknown` into an explicit `no hooks`,
/// so "idle" and "can't tell" never read the same.
pub fn activity_label(session: &Session, can_report: bool) -> &'static str {
    match session.activity {
        ActivityState::Working => "working",
        ActivityState::Idle    => "idle",
        ActivityState::Blocked => "blocked",
        ActivityState::Unknown if can_report => "unknown",
        ActivityState::Unknown => "no hooks",
    }
}

/// Whether a dependency edge is already satisfied: the depended-on session
/// finished (`Done`). Everything else — including a dependency that
/// terminated without merging — still blocks.
pub fn dep_resolved(target: Option<&Session>) -> bool {
    matches!(target.map(|s| &s.status), Some(ninox_core::SessionStatus::Done))
}

fn activity_color(app: &App, state: ActivityState) -> Color {
    let s = &app.scheme;
    match state {
        ActivityState::Working => s.status_working,
        ActivityState::Idle    => s.status_mergeable,
        ActivityState::Blocked => s.accent,
        ActivityState::Unknown => s.faint,
    }
}

/// Live workers grouped by orchestrator, in sidebar order, with unparented
/// workers last under `None`. Groups and rows are name-sorted for a stable
/// read. Terminal sessions are excluded — they linger in `app.sessions`
/// for the fleet board's retention window, but an activity view has
/// nothing truthful to say about a dead session (its `Unknown` would
/// render as "no hooks", which is a claim about hook installation, not
/// about being finished). Matches `ninox worker-status list`.
pub fn grouped_workers<'a>(
    sessions: &'a std::collections::HashMap<ninox_core::SessionId, Session>,
    orchestrators: &'a [ninox_core::Orchestrator],
) -> Vec<(Option<&'a ninox_core::Orchestrator>, Vec<&'a Session>)> {
    let orch_ids: std::collections::HashSet<&str> =
        orchestrators.iter().map(|o| o.id.as_str()).collect();
    let live = |s: &&Session| !s.status.is_terminal() && !orch_ids.contains(s.id.as_str());
    let mut groups: Vec<(Option<&ninox_core::Orchestrator>, Vec<&Session>)> = Vec::new();
    for orch in orchestrators {
        let mut members: Vec<&Session> = sessions.values()
            .filter(live)
            .filter(|s| s.orchestrator_id.as_deref() == Some(orch.id.as_str()))
            .collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        if !members.is_empty() {
            groups.push((Some(orch), members));
        }
    }
    let mut loose: Vec<&Session> = sessions.values()
        .filter(live)
        .filter(|s| s.orchestrator_id.is_none())
        .collect();
    loose.sort_by(|a, b| a.name.cmp(&b.name));
    if !loose.is_empty() {
        groups.push((None, loose));
    }
    groups
}

fn badge<'a>(label: &'a str, color: Color) -> Element<'a, Message> {
    container(text(label).size(9.5).font(crate::style::MONO_MEDIUM).color(color))
        .padding([2, 8])
        .style(move |_| container::Style {
            background: Some(Background::Color(Color { a: 0.08, ..color })),
            border: Border { color, width: 1.0, radius: 2.0.into() },
            ..Default::default()
        })
        .into()
}

fn dep_line<'a>(app: &'a App, dep: &'a SessionDep) -> Element<'a, Message> {
    let s = &app.scheme;
    let target = app.sessions.get(&dep.depends_on);
    let target_name = target.map(|t| t.name.as_str()).unwrap_or(dep.depends_on.as_str());
    let resolved = dep_resolved(target);
    let kind = match dep.kind {
        DepKind::Declared => "declared",
        DepKind::Stacked  => "stacked",
    };
    let color = if resolved { s.status_done } else { s.ink_2 };
    let mut pieces: Vec<Element<Message>> = vec![
        text("⭢ ").size(11).font(crate::style::GLYPH).color(color).into(),
        text(format!("depends on {target_name}")).size(11).font(crate::style::SANS).color(color).into(),
        Space::new(6, 0).into(),
        text(format!("[{kind}]")).size(9).font(crate::style::MONO).color(s.faint).into(),
    ];
    if resolved {
        pieces.push(Space::new(6, 0).into());
        pieces.push(text("resolved — dependency is done").size(9.5).font(crate::style::MONO).color(s.status_done).into());
    } else if let Some(note) = dep.note.as_deref().filter(|n| !n.is_empty()) {
        pieces.push(Space::new(6, 0).into());
        pieces.push(text(format!("— {note}")).size(10).font(crate::style::SANS).color(s.faint).into());
    }
    row(pieces).align_y(Alignment::Center).into()
}

fn worker_row<'a>(app: &'a App, session: &'a Session, now_ms: i64) -> Element<'a, Message> {
    let s = &app.scheme;
    let can_report = app.status_probe.get(&session.id).copied().unwrap_or(false);
    let label = activity_label(session, can_report);
    let color = activity_color(app, session.activity);
    let (card_a, _, _) = crate::style::shadow_alpha(s);

    let mut head: Vec<Element<Message>> = vec![
        text(&session.name).size(15).font(crate::style::SERIF_MEDIUM).color(s.ink).into(),
        Space::new(10, 0).into(),
        badge(label, color),
    ];
    if let Some(since) = session.activity_since {
        head.push(Space::new(6, 0).into());
        head.push(
            text(since_label(since, now_ms)).size(9.5).font(crate::style::MONO).color(s.faint).into(),
        );
    }
    head.push(Space::new(Length::Fill, 0).into());
    let lifecycle = crate::components::lifecycle_status::with_gate_tooltip(
        s, session,
        crate::style::stamp(crate::style::stamp_word(&session.status), s.status_color(&session.status)),
    );
    head.push(lifecycle);

    let mut body: Vec<Element<Message>> = vec![row(head).align_y(Alignment::Center).into()];
    if let Some(note) = session.activity_note.as_deref().filter(|n| !n.is_empty()) {
        body.push(Space::new(0, 3).into());
        body.push(text(note.to_owned()).size(11).font(crate::style::SANS_ITALIC).color(s.ink_2).into());
    }
    let deps: Vec<&SessionDep> = app.session_deps.iter()
        .filter(|d| d.session_id == session.id)
        .collect();
    if !deps.is_empty() {
        body.push(Space::new(0, 5).into());
        for dep in deps {
            body.push(container(dep_line(app, dep)).padding(iced::Padding {
                top: 1.0, right: 0.0, bottom: 1.0, left: 12.0,
            }).into());
        }
    }

    button(column(body).padding(iced::Padding { top: 10.0, right: 13.0, bottom: 10.0, left: 13.0 }))
        .on_press(Message::NavigateSession(session.id.clone()))
        .width(Length::Fill)
        .style(move |_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(s.card)),
                text_color: s.ink,
                border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
                shadow: crate::style::hard_shadow(
                    s,
                    if hovered { 4.0 } else { 2.0 },
                    if hovered { 6.0 } else { 3.0 },
                    card_a + if hovered { 0.02 } else { 0.0 },
                ),
            }
        })
        .into()
}

pub fn workers_view(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let now_ms = crate::components::lifecycle_status::now_millis();

    let mut sections: Vec<Element<Message>> = vec![
        container(
            row![
                text("Worker ").size(34).font(crate::style::SERIF).color(s.ink),
                text("registry").size(34).font(crate::style::SERIF_ITALIC).color(s.ink),
                Space::new(18, 0),
                text("activity · interdependencies")
                    .size(10.5).font(crate::style::MONO).color(s.faint),
            ]
            .align_y(Alignment::End),
        )
        .padding(iced::Padding { top: 24.0, right: 28.0, bottom: 14.0, left: 28.0 })
        .into(),
    ];

    let groups = grouped_workers(&app.sessions, &app.orchestrators);
    if groups.is_empty() {
        sections.push(
            container(
                text("No workers in the field.")
                    .size(15).font(crate::style::SERIF_ITALIC).color(s.faint),
            )
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        );
    } else {
        let mut list: Vec<Element<Message>> = Vec::new();
        for (orch, members) in groups {
            let group_label = orch.map(|o| o.name.as_str()).unwrap_or("Standalone");
            list.push(
                column![
                    row![
                        text(group_label).size(16.5).font(crate::style::SERIF_MEDIUM_ITALIC).color(s.ink),
                        Space::new(Length::Fill, 0),
                        text(format!("№ {}", members.len())).size(10).font(crate::style::MONO).color(s.faint),
                    ]
                    .align_y(Alignment::End),
                    Space::new(0, 6),
                    crate::style::hline(s.ink, 2.0),
                ]
                .into(),
            );
            for session in members {
                list.push(worker_row(app, session, now_ms));
            }
            list.push(Space::new(0, 10).into());
        }
        sections.push(
            container(
                scrollable(column(list).spacing(10).padding(iced::Padding {
                    top: 4.0, right: 12.0, bottom: 16.0, left: 0.0,
                }))
                .height(Length::Fill),
            )
            .padding(iced::Padding { top: 0.0, right: 28.0, bottom: 16.0, left: 28.0 })
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
        );
    }

    column(sections).width(Length::Fill).height(Length::Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::SessionStatus;

    fn session(id: &str, activity: ActivityState) -> Session {
        Session {
            id: id.into(), orchestrator_id: None, name: id.into(),
            repo: "o/r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None,
            gate_status: None, merged_at: None,
            activity, activity_note: None, activity_since: None,
        }
    }

    #[test]
    fn since_label_scales_with_elapsed_time() {
        assert_eq!(since_label(1_000, 30_000), "just now");
        assert_eq!(since_label(0, 4 * 60_000), "4m");
        assert_eq!(since_label(0, 125 * 60_000), "2h 05m");
        assert_eq!(since_label(0, 3 * 1440 * 60_000), "3d");
        // A clock skew (since in the future) must not underflow.
        assert_eq!(since_label(99_999, 0), "just now");
    }

    #[test]
    fn activity_label_distinguishes_cannot_report_from_unknown() {
        let s = session("w", ActivityState::Unknown);
        assert_eq!(activity_label(&s, true), "unknown");
        assert_eq!(activity_label(&s, false), "no hooks");
        let s = session("w", ActivityState::Blocked);
        assert_eq!(activity_label(&s, false), "blocked", "a real report always wins over the probe");
    }

    #[test]
    fn grouped_workers_excludes_terminal_and_orchestrator_sessions() {
        let mut sessions = std::collections::HashMap::new();
        let orch = ninox_core::Orchestrator { id: "orch".into(), name: "Orch".into(), created_at: 0 };
        let mut worker = session("w1", ActivityState::Working);
        worker.orchestrator_id = Some("orch".into());
        sessions.insert("w1".to_string(), worker);
        let mut done = session("done-w", ActivityState::Unknown);
        done.orchestrator_id = Some("orch".into());
        done.status = SessionStatus::Done;
        sessions.insert("done-w".to_string(), done);
        // The orchestrator's own bookkeeping session must not render as a worker.
        let mut orch_row = session("orch", ActivityState::Working);
        orch_row.orchestrator_id = None;
        sessions.insert("orch".to_string(), orch_row);

        let groups = grouped_workers(&sessions, std::slice::from_ref(&orch));
        assert_eq!(groups.len(), 1);
        let (group_orch, members) = &groups[0];
        assert_eq!(group_orch.map(|o| o.id.as_str()), Some("orch"));
        let names: Vec<&str> = members.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(names, vec!["w1"], "terminal + orchestrator rows must be excluded");
    }

    #[test]
    fn dep_resolved_only_for_done_dependencies() {
        let mut t = session("t", ActivityState::Idle);
        assert!(!dep_resolved(Some(&t)), "working dependency still blocks");
        t.status = SessionStatus::Done;
        assert!(dep_resolved(Some(&t)));
        t.status = SessionStatus::Terminated;
        assert!(!dep_resolved(Some(&t)), "terminated-without-merge still blocks");
        assert!(!dep_resolved(None), "a purged dependency renders as unresolved, not resolved");
    }
}
