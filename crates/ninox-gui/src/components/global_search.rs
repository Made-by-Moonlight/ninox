//! G3 (round-1 try-out feedback): the top-bar "Search sessions, plans,
//! brain…" field actually searches now. Was a static `text()` label inside
//! a styled `container` in `shell::nav` — no `text_input`, no state, no
//! handler at all ("this search also does nothing").
//!
//! Three grouped result sources, each capped at [`MAX_HITS_PER_GROUP`]:
//! Sessions (name/one-line summary), Plans (the owning orchestrator's name
//! — neither `OrchestratorPlan` nor `PlanDocState` carry a title of their
//! own), Brain (name/tags/body, via `BrainIndex::query` directly). `↑`/`↓`
//! move a single flat cursor across all three groups in that order; `Enter`
//! opens the cursored hit (or the first hit with nothing arrowed to yet).

use iced::widget::{button, column, container, mouse_area, row, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use ninox_core::store::Store;
use ninox_core::types::{Orchestrator, Session};
use ninox_core::QueryFilters;

use super::shell::tokens;
use crate::app::{App, Message};

const MAX_HITS_PER_GROUP: usize = 5;

/// Stable id for the field's `text_input` — `⌘K` focuses it (see `app.rs`'s
/// `RawKey` handling).
pub fn search_input_id() -> text_input::Id {
    text_input::Id::new("ninox-global-search")
}

/// Which half of the search UI the pointer is over — tracked separately
/// because leaving one and entering the other can arrive in either order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPart {
    Field,
    Dropdown,
}

/// One flattened, openable hit — carries just enough identity to dispatch
/// the right message(s) via `Message::GlobalSearchOpenHit`, independent of
/// which group rendered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalSearchHit {
    /// Opens the session's Fleet island (`Message::FleetOpenNode`).
    Session { id: String, label: String },
    /// Opens the owning orchestrator's own Plan detail panel —
    /// `View::Plans` itself is an unimplemented placeholder, so a
    /// session's Plan panel is the only real destination today.
    Plan { orchestrator_id: String, label: String },
    /// Switches to Brain and selects the entry (`NavigateBrain` then
    /// `BrainSelectEntry`). `label` is the readable title
    /// (`ninox_core::brain::display_title` — first H1, falling back to the
    /// filename stem — same as Brain List shows, never the raw entry id),
    /// `kind` its human-readable kind label (`ninox_core::brain::
    /// kind_label`) for a dim secondary line.
    Brain { id: String, label: String, kind: String },
}

impl GlobalSearchHit {
    pub fn label(&self) -> &str {
        match self {
            GlobalSearchHit::Session { label, .. }
            | GlobalSearchHit::Plan { label, .. }
            | GlobalSearchHit::Brain { label, .. } => label,
        }
    }
}

/// The three result groups for one query, each already capped — the single
/// source both the dropdown's rendering and the keyboard cursor/Enter logic
/// read from, so they can never disagree about what's on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalSearchResults {
    pub sessions: Vec<GlobalSearchHit>,
    pub plans:    Vec<GlobalSearchHit>,
    pub brain:    Vec<GlobalSearchHit>,
}

impl GlobalSearchResults {
    /// Sessions, then Plans, then Brain, in that order — the cursor's flat
    /// index space matches this exactly.
    pub fn flat(&self) -> Vec<&GlobalSearchHit> {
        self.sessions.iter().chain(&self.plans).chain(&self.brain).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.plans.is_empty() && self.brain.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sessions.len() + self.plans.len() + self.brain.len()
    }
}

/// Sessions matching `query` by name or one-line summary (case-insensitive)
/// — never an orchestrator's own bookkeeping session, never the
/// prompt-agent singleton (MLOPS-5415; the GUI has no session page for it
/// at all — see `app.rs`'s `NavigateSession` guard). An empty query matches
/// everything, approximating "recent items" with no last-touched timestamp
/// to actually sort by.
pub fn search_sessions<'a>(
    sessions: impl Iterator<Item = &'a Session>,
    orchestrator_ids: &std::collections::HashSet<&str>,
    query: &str,
) -> Vec<GlobalSearchHit> {
    let q = query.to_lowercase();
    sessions
        .filter(|s| !orchestrator_ids.contains(s.id.as_str()))
        .filter(|s| !ninox_core::worker_type::is_prompt_agent_session(s))
        .filter(|s| {
            q.is_empty()
                || s.name.to_lowercase().contains(&q)
                || s.summary.as_deref().unwrap_or("").to_lowercase().contains(&q)
        })
        .take(MAX_HITS_PER_GROUP)
        .map(|s| GlobalSearchHit::Session { id: s.id.clone(), label: s.name.clone() })
        .collect()
}

/// Orchestrators with an actually-registered plan (`Store::
/// get_orchestrator_plan`, the same thing `ninox plan show` would see —
/// deliberately NOT the GUI's own `app.plan_docs` cache, which is only
/// lazily populated once a session's Plan detail panel has been opened in
/// *this* GUI session and would silently miss everything before that)
/// whose name matches `query`.
pub fn search_plans<'a>(
    store: &Store,
    orchestrators: impl Iterator<Item = &'a Orchestrator>,
    query: &str,
) -> Vec<GlobalSearchHit> {
    let q = query.to_lowercase();
    orchestrators
        .filter(|o| q.is_empty() || o.name.to_lowercase().contains(&q))
        .filter(|o| store.get_orchestrator_plan(&o.id).ok().flatten().is_some())
        .take(MAX_HITS_PER_GROUP)
        .map(|o| GlobalSearchHit::Plan { orchestrator_id: o.id.clone(), label: o.name.clone() })
        .collect()
}

/// Brain entries matching `query` by name/tags/body, via the same
/// `BrainIndex::query` call `NavigateBrain` itself makes (sync, file-backed
/// SQLite FTS) — not `brain_list::matches_search` over `app.brain_view
/// .entries`, which is only populated once the Brain tab has been visited
/// this session.
pub fn search_brain(brain: &crate::app::BrainScope, query: &str) -> Vec<GlobalSearchHit> {
    match brain.query(query, None, QueryFilters::default()) {
        Ok(entries) => entries
            .into_iter()
            .take(MAX_HITS_PER_GROUP)
            .map(|e| GlobalSearchHit::Brain {
                id: e.id.clone(),
                label: ninox_core::brain::display_title(&e),
                kind: ninox_core::brain::kind_label(&ninox_core::brain::normalize_kind(&e.entry_type)),
            })
            .collect(),
        Err(e) => {
            tracing::error!("global search brain query: {e}");
            Vec::new()
        }
    }
}

/// Runs all three searches for the current draft — the single entry point
/// both the dropdown's view and `app.rs`'s cursor/Enter handling call, so
/// they're always looking at the same results.
pub fn run_search(app: &App, query: &str) -> GlobalSearchResults {
    let orchestrator_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    GlobalSearchResults {
        sessions: search_sessions(app.sessions.values(), &orchestrator_ids, query),
        plans:    search_plans(&app.engine.store, app.orchestrators.iter(), query),
        brain:    search_brain(&app.brain, query),
    }
}

/// Resolves what Enter (or a click) should open: the hit at `cursor`, or
/// the very first hit when nothing's been arrowed to yet. `None` when
/// there's nothing to open.
pub fn hit_at(results: &GlobalSearchResults, cursor: Option<usize>) -> Option<GlobalSearchHit> {
    let flat = results.flat();
    flat.get(cursor.unwrap_or(0)).map(|h| (*h).clone())
}

fn group_header(label: &str, count: usize) -> Element<'static, Message> {
    container(text(format!("{label} ({count})")).size(10).color(tokens::DIM2))
        .padding(Padding { top: 8.0, right: 10.0, bottom: 4.0, left: 10.0 })
        .into()
}

/// Same selected-row treatment as `brain_list::entry_row`'s teal tint
/// (`Brain.dc.html`'s selection color) — a consistent "this is the
/// highlighted one" language across both lists, not a one-off.
const SELECTED_BG: Color = Color { r: 0x15 as f32 / 255.0, g: 0x2a as f32 / 255.0, b: 0x33 as f32 / 255.0, a: 1.0 };
const SELECTED_BORDER: Color = Color { r: 0x2a as f32 / 255.0, g: 0x7d as f32 / 255.0, b: 0x75 as f32 / 255.0, a: 1.0 };

fn hit_row<'a>(hit: &GlobalSearchHit, selected: bool) -> Element<'a, Message> {
    let mut content: Vec<Element<Message>> =
        vec![text(hit.label().to_string()).size(12).font(crate::style::MONO).color(tokens::TEXT).into()];
    if let GlobalSearchHit::Brain { kind, .. } = hit {
        content.push(text(kind.clone()).size(10).font(crate::style::SANS).color(tokens::DIM2).into());
    }

    button(column(content).spacing(2))
        .on_press(Message::GlobalSearchOpenHit(hit.clone()))
        .width(Length::Fill)
        .padding(Padding { top: 6.0, right: 10.0, bottom: 6.0, left: 10.0 })
        .style(move |_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            button::Style {
                background: Some(Background::Color(if selected { SELECTED_BG } else if hovered { tokens::PANEL2 } else { Color::TRANSPARENT })),
                border: if selected { Border { color: SELECTED_BORDER, width: 1.0, radius: 0.0.into() } } else { Border::default() },
                text_color: tokens::TEXT,
                ..Default::default()
            }
        })
        .into()
}

/// The dropdown panel, positioned as a floating overlay by `app.rs`'s
/// `iced_view` the same way the notifications panel is (`stack![base,
/// dropdown]`, `align_right`/`align_top` + padding approximating "under the
/// search field" — the field's own exact position varies with window
/// width, so this is the same fixed-offset approximation the notifications
/// dropdown already uses for its bell icon). `None` while closed, or open
/// with zero hits and an empty draft.
///
/// Reads `app.global_search.results` — already computed by
/// `GlobalSearchChanged`/`GlobalSearchFocus` — rather than calling
/// `run_search` here: this function re-runs on every `update()` regardless
/// of what changed (engine events, the 3s poll), and `search_brain`'s
/// `BrainIndex::query` is an unbounded, unlimited full-entry-body scan, so
/// doing it here would re-run that query on every tick for as long as the
/// dropdown stayed open (found in review of PR #279).
pub fn dropdown_overlay(app: &App) -> Option<Element<'_, Message>> {
    if !app.global_search.open {
        return None;
    }
    let results = &app.global_search.results;
    if results.is_empty() {
        return None;
    }

    // Defaults to the first hit so a row is always visibly selected and
    // Enter works immediately, without waiting for an explicit arrow press
    // — `cursor` itself stays `None` until the user actually arrows (see
    // `GlobalSearchCursorFocusChecked`), this is purely the display default,
    // matching `hit_at`'s own `unwrap_or(0)` fallback.
    let effective_cursor = app.global_search.cursor.unwrap_or(0);
    let mut rows: Vec<Element<Message>> = Vec::new();
    let mut flat_index = 0usize;
    for (label, group) in [("Sessions", &results.sessions), ("Plans", &results.plans), ("Brain", &results.brain)] {
        if group.is_empty() {
            continue;
        }
        rows.push(group_header(label, group.len()));
        for hit in group {
            rows.push(hit_row(hit, flat_index == effective_cursor));
            flat_index += 1;
        }
    }

    Some(
        mouse_area(container(column(rows))
            .width(Length::Fixed(320.0))
            .padding(Padding { top: 4.0, right: 0.0, bottom: 4.0, left: 0.0 })
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }))
            .on_enter(Message::GlobalSearchHover(SearchPart::Dropdown, true))
            .on_exit(Message::GlobalSearchHover(SearchPart::Dropdown, false))
            .into(),
    )
}

/// The field itself — `⌘K` keycap on the right, replacing `shell::nav`'s
/// previously-decorative static label.
/// `width`: fixed 240 in a full-width top bar; `Fill` when the bar is
/// narrow and the field only shows while search is open.
pub fn field(app: &App, width: Length) -> Element<'_, Message> {
    let keycap = container(text("⌘K").size(10).font(crate::style::glyph_font_for("⌘K")).color(tokens::DIM))
        .padding(Padding { top: 2.0, right: 6.0, bottom: 2.0, left: 6.0 })
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        });

    let input = text_input("Search sessions, plans, brain…", &app.global_search.draft)
        .id(search_input_id())
        .size(12)
        .font(crate::style::SANS)
        .width(width)
        .padding(Padding { top: 6.0, right: 36.0, bottom: 6.0, left: 10.0 })
        .on_input(Message::GlobalSearchChanged)
        .on_submit(Message::GlobalSearchEnterPressed)
        .style(|_t, status| {
            let focused = status == text_input::Status::Focused;
            text_input::Style {
                background: Background::Color(tokens::PANEL2),
                border: Border {
                    color: if focused { tokens::TEAL } else { tokens::BORDER },
                    width: if focused { 2.0 } else { 1.0 },
                    radius: 6.0.into(),
                },
                icon: tokens::DIM,
                placeholder: tokens::DIM,
                value: tokens::TEXT,
                selection: tokens::VIOLET,
            }
        });

    mouse_area(container(iced::widget::stack![
        input,
        container(row![keycap].align_y(Alignment::Center))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::End)
            .align_y(Alignment::Center)
            .padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 0.0 }),
    ]))
    .on_enter(Message::GlobalSearchHover(SearchPart::Field, true))
    .on_exit(Message::GlobalSearchHover(SearchPart::Field, false))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, SessionStatus};

    fn session(id: &str, name: &str, summary: Option<&str>) -> Session {
        Session {
            id: id.into(), orchestrator_id: None, name: name.into(),
            repo: "o/r".into(), status: SessionStatus::Working, agent_type: "claude-code".into(),
            cost_usd: 0.0, started_at: 0, pr_number: None, pr_id: None,
            workspace_path: None, pid: None, model: None, context_tokens: None,
            brain_reads: Vec::new(), brain_write: None, context_used_pct: None, context_total_tokens: None,
            context_window_size: None, claude_session_id: None,
            summary: summary.map(String::from), terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn matches_name_or_summary_case_insensitively() {
        let sessions = [
            session("a", "Webhook rewrite", None),
            session("b", "unrelated", Some("blocked on the WEBHOOK team")),
            session("c", "also unrelated", None),
        ];
        let hits = search_sessions(sessions.iter(), &std::collections::HashSet::new(), "webhook");
        let ids: Vec<&str> = hits.iter().map(|h| match h {
            GlobalSearchHit::Session { id, .. } => id.as_str(),
            _ => unreachable!(),
        }).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn excludes_orchestrator_sessions() {
        let sessions = [session("orch", "Orch", None)];
        let orchestrator_ids: std::collections::HashSet<&str> = ["orch"].into_iter().collect();
        assert!(search_sessions(sessions.iter(), &orchestrator_ids, "").is_empty());
    }

    #[test]
    fn excludes_the_prompt_agent_session() {
        let mut prompt = session(ninox_core::worker_type::PROMPT_SESSION_ID, "Ninox prompt", None);
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        assert!(search_sessions([prompt].iter(), &std::collections::HashSet::new(), "").is_empty());
    }

    #[test]
    fn caps_at_five_hits() {
        let sessions: Vec<Session> = (0..10).map(|i| session(&format!("s{i}"), "match", None)).collect();
        let hits = search_sessions(sessions.iter(), &std::collections::HashSet::new(), "match");
        assert_eq!(hits.len(), MAX_HITS_PER_GROUP);
    }

    /// Plans search is keyed off `Store::get_orchestrator_plan`, not the
    /// GUI's own lazily-populated `app.plan_docs` cache — an orchestrator
    /// with a registered plan the user hasn't opened this session must
    /// still be findable.
    #[test]
    fn search_plans_matches_orchestrator_name_and_requires_a_registered_plan() {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        store.register_orchestrator_plan("o1", "/plan.md", 0).unwrap();
        let orchestrators = [
            Orchestrator { id: "o1".into(), name: "Webhook rewrite".into(), created_at: 0 },
            Orchestrator { id: "o2".into(), name: "Webhook cleanup".into(), created_at: 0 }, // no registered plan
        ];
        let hits = search_plans(&store, orchestrators.iter(), "webhook");
        assert_eq!(hits, vec![GlobalSearchHit::Plan { orchestrator_id: "o1".into(), label: "Webhook rewrite".into() }]);
    }

    /// Brain hits show the readable H1 title (same as Brain List — never
    /// the raw filename-stem `name`) plus a dim human-readable kind label.
    #[test]
    fn search_brain_uses_the_readable_title_and_kind_label() {
        let brain_dir = tempfile::tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("decisions")).unwrap();
        std::fs::write(
            brain_dir.join("decisions/webhook-notes.md"),
            "# Webhook delivery notes\n\nBackground on the flaky webhook retry behavior.\n",
        ).unwrap();
        let brain = ninox_core::BrainIndex::open(&brain_dir).unwrap();
        brain.rebuild(None).unwrap();
        let brain = crate::app::BrainScope::Single(std::sync::Arc::new(brain));

        let hits = search_brain(&brain, "webhook");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].label(), "Webhook delivery notes", "must show the H1 title, not the filename stem");
        assert!(matches!(&hits[0], GlobalSearchHit::Brain { kind, .. } if !kind.is_empty()));
    }

    #[test]
    fn empty_query_matches_everything_up_to_the_cap() {
        let sessions = [session("a", "one", None), session("b", "two", None)];
        let hits = search_sessions(sessions.iter(), &std::collections::HashSet::new(), "");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn flat_orders_sessions_then_plans_then_brain() {
        let results = GlobalSearchResults {
            sessions: vec![GlobalSearchHit::Session { id: "s".into(), label: "s".into() }],
            plans: vec![GlobalSearchHit::Plan { orchestrator_id: "o".into(), label: "o".into() }],
            brain: vec![GlobalSearchHit::Brain { id: "b".into(), label: "b".into(), kind: "Notes".into() }],
        };
        let flat = results.flat();
        assert!(matches!(flat[0], GlobalSearchHit::Session { .. }));
        assert!(matches!(flat[1], GlobalSearchHit::Plan { .. }));
        assert!(matches!(flat[2], GlobalSearchHit::Brain { .. }));
    }

    #[test]
    fn hit_at_falls_back_to_the_first_hit_with_no_cursor() {
        let results = GlobalSearchResults {
            sessions: vec![GlobalSearchHit::Session { id: "s".into(), label: "s".into() }],
            ..Default::default()
        };
        assert_eq!(hit_at(&results, None), Some(GlobalSearchHit::Session { id: "s".into(), label: "s".into() }));
    }

    #[test]
    fn hit_at_uses_the_cursor_when_set() {
        let results = GlobalSearchResults {
            sessions: vec![GlobalSearchHit::Session { id: "s".into(), label: "s".into() }],
            plans: vec![GlobalSearchHit::Plan { orchestrator_id: "o".into(), label: "o".into() }],
            ..Default::default()
        };
        assert_eq!(hit_at(&results, Some(1)), Some(GlobalSearchHit::Plan { orchestrator_id: "o".into(), label: "o".into() }));
    }

    #[test]
    fn hit_at_is_none_with_nothing_to_open() {
        assert_eq!(hit_at(&GlobalSearchResults::default(), None), None);
    }
}
