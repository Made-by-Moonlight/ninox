//! The Idle screen (`7`): live workers parked on a review, CI run, merge or
//! an answer, grouped by orchestrator, with the worktree disk they hold.
//! Reads `ninox_core::idle_workers` (stored state and cached disk numbers
//! only — no GitHub calls, no disk walks on the render path).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ninox_core::idle_workers::{self, IdleReport, IdleWorker, WaitingOn};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::state::{Action, Level, MessageComposer, Modal, TuiState, View};

#[derive(Clone, Debug, Default)]
pub struct IdleView {
    pub report: IdleReport,
    pub as_of: i64,
    pub selected: usize,
}

impl IdleView {
    pub fn set_report(&mut self, report: IdleReport, now: i64) {
        self.report = report;
        self.as_of = now;
        self.selected = self.selected.min(self.report.total.saturating_sub(1));
    }

    pub fn selected_worker(&self) -> Option<&IdleWorker> {
        self.report.workers().nth(self.selected)
    }
}

pub(crate) fn idle_key(st: &mut TuiState, key: KeyEvent) -> Action {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::None;
    }
    let total = st.idle.report.total;
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => st.idle.selected = (st.idle.selected + 1).min(total.saturating_sub(1)),
        KeyCode::Char('k') | KeyCode::Up => st.idle.selected = st.idle.selected.saturating_sub(1),
        KeyCode::Esc | KeyCode::Char('q') => return st.switch_view(View::Fleet),
        KeyCode::Char('P') => return Action::PromptIdle(None),
        KeyCode::Char('p') => match st.idle.selected_worker().and_then(|w| w.orchestrator_id.clone()) {
            Some(o) => return Action::PromptIdle(Some(o)),
            None => st.notify(Level::Info, "this worker has no orchestrator to prompt"),
        },
        KeyCode::Char('c') => match st.idle.selected_worker() {
            Some(w) if w.can_clean() => return Action::CleanIdle(w.id.clone()),
            Some(w) if w.disk.is_some_and(|d| d.build_bytes > 0) => {
                st.notify(Level::Info, "still mid-turn — its build may be running; clean once it is idle")
            }
            Some(_) => st.notify(Level::Info, "no measured build output to clean"),
            None => {}
        },
        KeyCode::Char('m') => {
            if let Some(w) = st.idle.selected_worker() {
                let (id, name) = (w.id.clone(), w.name.clone());
                st.modal = Some(Modal::Message(MessageComposer { id, name, draft: String::new() }));
            }
        }
        KeyCode::Char('o') => {
            let url = st.idle.selected_worker().and_then(|w| {
                w.pr_number.filter(|_| !w.repo.is_empty()).map(|n| format!("https://github.com/{}/pull/{n}", w.repo))
            });
            match url {
                Some(u) => return Action::OpenUrl(u),
                None => st.notify(Level::Info, "this worker has no PR"),
            }
        }
        KeyCode::Char('x') => {
            if let Some(id) = st.idle.selected_worker().map(|w| w.id.clone()) {
                st.ask_delete_id(id);
            }
        }
        KeyCode::Enter => {
            if let Some(id) = st.idle.selected_worker().map(|w| w.id.clone()) {
                if let Some(i) = st.rows.iter().position(|r| r.id() == id) {
                    st.selected = i;
                }
                return st.switch_view(View::Fleet);
            }
        }
        _ => {}
    }
    Action::None
}

fn waiting_color(st: &TuiState, w: &WaitingOn) -> Color {
    let p = &st.palette;
    match w {
        WaitingOn::CiFailed | WaitingOn::Blocked { .. } => p.ci_failed,
        WaitingOn::MergedNotCompleted | WaitingOn::Mergeable => p.mergeable,
        WaitingOn::Unknown => p.faint,
        _ => p.review,
    }
}

/// `n` columns of `s`, truncated with an ellipsis or padded with spaces.
fn fit(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        let mut out = s.to_string();
        out.extend(std::iter::repeat_n(' ', n - chars.len()));
        out
    } else {
        chars.iter().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

pub(crate) fn draw_idle(f: &mut Frame, st: &TuiState, body: Rect) {
    let p = &st.palette;
    if body.width == 0 || body.height < 3 {
        return;
    }
    let r = &st.idle.report;
    let faint = Style::default().fg(p.faint);
    let summary = if r.total == 0 {
        "No idle workers".to_string()
    } else {
        format!(
            "{} idle worker{} · {} on disk, {} build output",
            r.total,
            if r.total == 1 { "" } else { "s" },
            idle_workers::format_bytes(r.total_bytes),
            idle_workers::format_bytes(r.build_bytes),
        )
    };
    f.render_widget(Paragraph::new(Span::styled(summary, Style::default().fg(p.ink_2))), Rect { height: 1, ..body });
    f.render_widget(
        Paragraph::new(Span::styled("─".repeat(body.width as usize), Style::default().fg(p.rule))),
        Rect { y: body.y + 1, height: 1, ..body },
    );
    let list = Rect { y: body.y + 2, height: body.height - 2, ..body };
    if r.total == 0 {
        f.render_widget(
            Paragraph::new(Span::styled(
                "Workers show up after being idle or blocked for the configured time, or once their PR merged unfinished.",
                faint,
            )),
            list,
        );
        return;
    }

    let mut lines: Vec<(Line<'static>, bool)> = Vec::new();
    let mut idx = 0usize;
    let mut selected_line = 0usize;
    for g in &r.groups {
        let mut head = vec![Span::styled(
            format!("{} ({})", g.orchestrator_id.as_deref().unwrap_or("no orchestrator"), g.workers.len()),
            Style::default().fg(p.ink).add_modifier(Modifier::BOLD),
        )];
        if let Some(t) = g.prompted_at {
            head.push(Span::styled(
                format!("   prompted {} ago", idle_workers::format_duration(st.idle.as_of.saturating_sub(t))),
                faint,
            ));
        }
        lines.push((Line::from(head), false));
        for w in &g.workers {
            let selected = idx == st.idle.selected;
            if selected {
                selected_line = lines.len();
            }
            let idle = w.idle_ms.map(idle_workers::format_duration).unwrap_or_else(|| "?".into());
            let narrow = body.width < 100;
            let (name_w, wait_w) = if narrow { (20, 24) } else { (28, 30) };
            let disk = match w.disk {
                Some(d) => format!(
                    "{} / {} build{}",
                    idle_workers::format_bytes(d.total_bytes),
                    idle_workers::format_bytes(d.build_bytes),
                    if narrow {
                        String::new()
                    } else {
                        format!(" ({} ago)", idle_workers::format_duration(st.idle.as_of.saturating_sub(d.measured_at)))
                    },
                ),
                None => "not measured".into(),
            };
            let spans = vec![
                Span::raw(if selected { "▌ " } else { "  " }),
                Span::styled(fit(&w.name, name_w), Style::default().fg(p.ink)),
                Span::styled(format!("{:>6}  ", idle), faint),
                Span::styled(fit(&w.waiting_on.label(), wait_w), Style::default().fg(waiting_color(st, &w.waiting_on))),
                Span::styled(format!("  {disk}"), faint),
            ];
            lines.push((Line::from(spans), selected));
            idx += 1;
        }
    }
    let h = list.height as usize;
    let offset = selected_line.saturating_sub(h.saturating_sub(1));
    for (i, (line, selected)) in lines.into_iter().skip(offset).take(h).enumerate() {
        let row = Rect { y: list.y + i as u16, height: 1, ..list };
        let style = if selected { Style::default().bg(p.card) } else { Style::default() };
        f.render_widget(Paragraph::new(line).style(style), row);
    }
}

pub(crate) fn footer_hints() -> Vec<(String, &'static str)> {
    vec![
        ("j/k".into(), "move"),
        ("↵".into(), "open"),
        ("m".into(), "message"),
        ("o".into(), "PR"),
        ("c".into(), "clean build"),
        ("x".into(), "delete"),
        ("p/P".into(), "prompt orch/all"),
        ("esc".into(), "back"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::handle_key;
    use ninox_core::idle_workers::OrchestratorGroup;
    use ninox_core::store::DiskMeasurement;

    fn worker(id: &str, orch: Option<&str>, build: u64) -> IdleWorker {
        IdleWorker {
            id: id.into(), name: id.into(), orchestrator_id: orch.map(Into::into), repo: "acme/app".into(),
            pr_number: Some(7), idle_ms: Some(45 * 60_000), waiting_on: WaitingOn::ReviewPending,
            workspace_path: None, activity: ninox_core::types::ActivityState::Idle,
            disk: Some(DiskMeasurement { total_bytes: build + 1_000, build_bytes: build, measured_at: 0 }),
        }
    }

    fn state() -> TuiState {
        let mut st = TuiState::default();
        let groups = vec![
            OrchestratorGroup { orchestrator_id: Some("o1".into()), workers: vec![worker("a", Some("o1"), 5_000), worker("b", Some("o1"), 0)], prompted_at: None },
            OrchestratorGroup { orchestrator_id: None, workers: vec![worker("loose", None, 0)], prompted_at: None },
        ];
        let report = IdleReport { groups, total: 3, total_bytes: 8_000, build_bytes: 5_000 };
        st.idle.set_report(report, 10 * 60_000);
        st.view = View::Idle;
        st
    }

    fn press(st: &mut TuiState, c: char) -> Action {
        handle_key(st, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    #[test]
    fn seven_opens_the_idle_screen_from_any_tab() {
        for from in [View::Board, View::Brain, View::Fleet] {
            let mut st = TuiState { view: from, ..Default::default() };
            press(&mut st, '7');
            assert_eq!(st.view, View::Idle, "from {from:?}");
        }
    }

    #[test]
    fn keys_move_and_act_on_the_selected_worker() {
        let mut st = state();
        assert_eq!(press(&mut st, 'p'), Action::PromptIdle(Some("o1".into())));
        assert_eq!(press(&mut st, 'P'), Action::PromptIdle(None));
        assert_eq!(press(&mut st, 'c'), Action::CleanIdle("a".into()));
        assert_eq!(press(&mut st, 'o'), Action::OpenUrl("https://github.com/acme/app/pull/7".into()));
        press(&mut st, 'j');
        assert_eq!(press(&mut st, 'c'), Action::None, "no build output to clean on b");
        press(&mut st, 'j');
        assert_eq!(st.idle.selected_worker().unwrap().id, "loose");
        assert_eq!(press(&mut st, 'p'), Action::None, "a worker with no orchestrator can't be prompted");
        press(&mut st, 'j');
        assert_eq!(st.idle.selected, 2, "selection stops at the last worker");
    }

    #[test]
    fn message_and_delete_use_the_existing_modals() {
        let mut st = state();
        press(&mut st, 'm');
        assert!(matches!(&st.modal, Some(Modal::Message(m)) if m.id == "a"));
        st.modal = None;
        press(&mut st, 'x');
        assert!(matches!(&st.modal, Some(Modal::Delete(d)) if d.root == "a"));
    }

    #[test]
    fn it_renders_groups_and_fits_a_tiny_terminal() {
        let st = state();
        for (w, h) in [(80, 24), (20, 5)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut term = ratatui::Terminal::new(backend).unwrap();
            term.draw(|f| draw_idle(f, &st, f.area())).unwrap();
        }
        let backend = ratatui::backend::TestBackend::new(100, 12);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| draw_idle(f, &st, f.area())).unwrap();
        let text: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("3 idle workers"), "{text}");
        assert!(text.contains("o1 (2)") && text.contains("no orchestrator (1)"), "{text}");
        assert!(text.contains("PR awaiting review"), "{text}");
    }
}
