//! The v1 shell's collapsible session tree (left column). Ported from the
//! removed `components::sidebar`'s tree data/expand-state logic, restyled
//! with v1 tokens, and grouped by machine per the fleet-overview-ui-vision
//! spec's "Screen skeleton" section ("This machine", then one group per
//! remote machine with its connection state).

use iced::{
    widget::{button, column, container, row, scrollable, text, tooltip, Space},
    Alignment, Background, Border, Element, Length, Padding,
};

use ninox_core::types::{MachineHealth, OrchestratorId, SessionStatus};

use super::tokens;
use crate::components::pr_chip;
use crate::{
    app::{App, Message, View},
    components::lifecycle_status,
    style::MONO,
};

pub(crate) const EXPANDED_WIDTH: f32 = 238.0;
pub(crate) const RAIL_WIDTH: f32 = 44.0;

/// Sessions belonging to no orchestrator and not an orchestrator's own
/// bookkeeping session — the tree's "Standalone" section. Never the
/// prompt-agent singleton (MLOPS-5415) either: same exclusion as Fleet's
/// "Unparented sessions" island and the Board's counts
/// (`worker_type::is_prompt_agent_session`), since this is the identical
/// "unparented" predicate applied to a different view of the same data.
pub(crate) fn standalone_sessions(app: &App) -> impl Iterator<Item = &ninox_core::types::Session> {
    app.sessions
        .values()
        .filter(|w| w.orchestrator_id.is_none() && !app.orchestrators.iter().any(|o| o.id == w.id))
        .filter(|w| !ninox_core::worker_type::is_prompt_agent_session(w))
}

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Clips `s` to at most `max_chars`, whole, with an ellipsis — a tree row
/// pre-truncates its name instead of relying on `Length::Fill` + `clip` +
/// `Wrapping::None` alone to keep it single-line: a long, unbroken
/// orchestrator name (no hyphens to wrap at) still soft-wrapped across
/// several lines in the EXPANDED_WIDTH-wide tree despite `Wrapping::None`
/// (round-1 feedback, item 3), so the row's own natural text width needs to
/// already be small rather than depending on layout alone to clip it.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max_chars).collect();
        format!("{clipped}…")
    }
}

/// Bounds the row's right-hand meta text (repo name, worker count, …) to a
/// fixed char budget — #254 review carry-over (slice 4 brief item 5c): an
/// unbounded `right` value could grow long enough to squeeze the `Length::
/// Fill` name column below the width its own `truncate_chars(name, 26)` +
/// ellipsis assumed, so the row's `clip(true)` cropped the *already*
/// truncated name and its trailing "…" along with it — reading as a plain,
/// un-ellipsized cut mid-word rather than a clean truncation.
const RIGHT_LABEL_MAX_CHARS: usize = 18;

fn right_label(s: &str) -> String {
    truncate_chars(s, RIGHT_LABEL_MAX_CHARS)
}

/// Approximate advance of one 12px SANS glyph / one 10px MONO glyph, used to
/// budget a row's width without measuring text.
const NAME_CHAR_PX: f32 = 5.8;
const RIGHT_CHAR_PX: f32 = 6.0;
const RETENTION_CHAR_PX: f32 = 5.8;
const SECONDARY_GAP_PX: f32 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionForm {
    Full,
    Compact,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LabelPlan {
    show_right: bool,
    retention: RetentionForm,
    name_max_chars: usize,
}

/// "Removing in 4d" -> "4d"; "Removing shortly" -> "<1m".
fn compact_retention(full: &str) -> String {
    full.strip_prefix("Removing in ").map(str::to_string).unwrap_or_else(|| "<1m".to_string())
}

/// Splits a row's `row_px` between the name and its secondary labels. The
/// name has priority: secondaries are dropped in order (full retention,
/// compact retention, right label) until the whole name fits, and the name
/// is only truncated once nothing else is left to drop.
fn row_label_plan(row_px: f32, name: &str, right: Option<&str>, retention: Option<&str>) -> LabelPlan {
    let name_chars = name.chars().count();
    let right_px = right.map_or(0.0, |r| right_label(r).chars().count() as f32 * RIGHT_CHAR_PX + SECONDARY_GAP_PX);
    let candidates: &[(bool, RetentionForm)] = if retention.is_some() {
        &[
            (true, RetentionForm::Full),
            (true, RetentionForm::Compact),
            (true, RetentionForm::Hidden),
            (false, RetentionForm::Hidden),
        ]
    } else {
        &[(true, RetentionForm::Hidden), (false, RetentionForm::Hidden)]
    };
    let mut plan = LabelPlan { show_right: false, retention: RetentionForm::Hidden, name_max_chars: 0 };
    for &(want_right, form) in candidates {
        let show_right = want_right && right.is_some();
        let retention_px = match (form, retention) {
            (RetentionForm::Full, Some(r)) => r.chars().count() as f32 * RETENTION_CHAR_PX + SECONDARY_GAP_PX,
            (RetentionForm::Compact, Some(r)) => {
                compact_retention(r).chars().count() as f32 * RETENTION_CHAR_PX + SECONDARY_GAP_PX
            }
            _ => 0.0,
        };
        let avail = row_px - retention_px - if show_right { right_px } else { 0.0 };
        plan = LabelPlan { show_right, retention: form, name_max_chars: (avail / NAME_CHAR_PX).floor().max(0.0) as usize };
        if name_chars <= plan.name_max_chars {
            break;
        }
    }
    plan
}

const SIDEBAR_LABEL_CHARS: usize = 14;

/// How many PR chips a sidebar row can carry (2, then 1, then 0) without
/// truncating the name. 0 means the row folds to a count chip.
fn pr_chip_budget(row_px: f32, name: &str, prs: &[ninox_core::session_prs::SessionPr]) -> usize {
    if prs.is_empty() {
        return 0;
    }
    let name_px = name.chars().count() as f32 * NAME_CHAR_PX;
    pr_chip::plan(prs, name_px, row_px, 2, SIDEBAR_LABEL_CHARS, None).max_chips()
}

/// Truncates to `max_chars` including the ellipsis, so the result never
/// exceeds the budget.
fn name_for_budget(name: &str, max_chars: usize) -> String {
    pr_chip::ellipsize(name, max_chars)
}

/// A worker row inside its orchestrator's tree skips a repo label that only
/// repeats the orchestrator's own name.
fn own_tree_right<'a>(repo: &'a str, orchestrator_name: &str) -> &'a str {
    if repo.eq_ignore_ascii_case(orchestrator_name) { "" } else { repo }
}

fn worker_count_label(count: usize) -> String {
    if count == 1 { "1 worker".to_string() } else { format!("{count} workers") }
}

fn unread_badge_text(count: u64) -> String {
    count.min(99).to_string()
}

fn connection_label(health: Option<MachineHealth>) -> &'static str {
    match health {
        Some(MachineHealth::Connected) | None => "connected",
        Some(MachineHealth::Reconnecting) => "reconnecting…",
        Some(MachineHealth::Unreachable) => "unreachable",
    }
}

fn connection_dot_color(health: Option<MachineHealth>) -> iced::Color {
    match health {
        Some(MachineHealth::Connected) | None => tokens::GREEN,
        Some(MachineHealth::Reconnecting) => tokens::AMBER,
        Some(MachineHealth::Unreachable) => tokens::RED,
    }
}

/// One machine's orchestrators, for grouping — `None` machine id = local.
struct MachineGroup<'a> {
    label: String,
    health: Option<MachineHealth>,
    orchestrators: Vec<&'a ninox_core::types::Orchestrator>,
}

fn group_by_machine(app: &App) -> Vec<MachineGroup<'_>> {
    let health = app.engine.store.machine_health().unwrap_or_default();
    let mut local = MachineGroup { label: "This machine".to_string(), health: None, orchestrators: Vec::new() };
    let mut remote: Vec<MachineGroup> = Vec::new();

    for orch in &app.orchestrators {
        match app.sessions.get(orch.id.as_str()).and_then(|s| s.machine_id.clone()) {
            None => local.orchestrators.push(orch),
            Some(machine_id) => {
                let label = app
                    .config
                    .remote_machines
                    .machines
                    .iter()
                    .find(|m| m.id == machine_id)
                    .map(|m| m.label.clone())
                    .unwrap_or(machine_id.clone());
                let group = match remote.iter_mut().find(|g: &&mut MachineGroup| g.label == label) {
                    Some(g) => g,
                    None => {
                        remote.push(MachineGroup {
                            label,
                            health: health.get(&machine_id).copied(),
                            orchestrators: Vec::new(),
                        });
                        remote.last_mut().expect("just pushed")
                    }
                };
                group.orchestrators.push(orch);
            }
        }
    }

    let mut groups = vec![local];
    groups.extend(remote);
    groups
}

pub fn tree<'a>(app: &'a App, islands: Option<&[ninox_overview::islands::Island]>) -> Element<'a, Message> {
    if app.fleet_nav.tree_collapsed {
        rail(app, islands)
    } else {
        expanded(app, islands)
    }
}

/// The `.rail-btn` icon from `FleetFocused.dc.html`/`FleetAtScale.dc.html`:
/// a panel outline with a divider and a chevron pointing the way the tree
/// will move. Drawn from plain shapes, per `style::GLYPH`'s note on
/// unverified codepoints.
fn panel_icon<'a>(show: bool) -> Element<'a, Message> {
    container(
        row![
            Space::new(4, 0),
            container(Space::new(0, 0)).width(Length::Fixed(1.4)).height(Length::Fill).style(|_| container::Style {
                background: Some(Background::Color(RAIL_BTN_FG)),
                ..Default::default()
            }),
            container(text(if show { "›" } else { "‹" }).size(11).font(crate::style::SANS).color(RAIL_BTN_FG))
                .width(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        ]
        .height(Length::Fill),
    )
    .width(Length::Fixed(14.0))
    .height(Length::Fixed(13.0))
    .style(|_| container::Style {
        border: Border { color: RAIL_BTN_FG, width: 1.4, radius: 2.0.into() },
        ..Default::default()
    })
    .into()
}

/// The board's `.rail-btn` icon colour (`#a3aabb`).
const RAIL_BTN_FG: iced::Color = iced::Color { r: 0xa3 as f32 / 255.0, g: 0xaa as f32 / 255.0, b: 0xbb as f32 / 255.0, a: 1.0 };
const RAIL_BTN_SIZE: f32 = 26.0;

/// The `[` shortcut lives only here, never as loose text in the tree.
pub(crate) fn rail_tooltip(collapsed: bool) -> &'static str {
    if collapsed { "Show session tree  [" } else { "Hide session tree  [" }
}

/// The collapsed rail's amber badge: none when nothing needs you, capped at "9+".
pub(crate) fn needs_you_badge(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=9 => Some(count.to_string()),
        _ => Some("9+".to_string()),
    }
}

/// "N trees · M workers" for the expanded header.
pub(crate) fn header_counts(trees: usize, workers: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") };
    format!("{} · {}", plural(trees, "tree", "trees"), plural(workers, "worker", "workers"))
}

/// The 26px bordered `.rail-btn` that hides or shows the tree, with its
/// shortcut in the tooltip and, collapsed, an amber needs-you badge.
fn rail_button<'a>(collapsed: bool, needs_you: usize) -> Element<'a, Message> {
    let btn = button(container(panel_icon(collapsed)).center_x(Length::Fill).center_y(Length::Fill))
        .on_press(Message::ToggleTreeCollapsed)
        .padding(0)
        .width(Length::Fixed(RAIL_BTN_SIZE))
        .height(Length::Fixed(RAIL_BTN_SIZE))
        .style(|_t, status| button::Style {
            background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        });
    let tip = container(text(rail_tooltip(collapsed)).size(11).font(crate::style::SANS).color(tokens::TEXT))
        .padding([5, 8])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        });
    let position = if collapsed { tooltip::Position::Right } else { tooltip::Position::Bottom };
    let with_tip: Element<Message> = tooltip(btn, tip, position).gap(6).into();
    let Some(badge) = needs_you_badge(needs_you).filter(|_| collapsed) else { return with_tip };
    let badge = container(text(badge).size(9).font(crate::style::SANS_BOLD).color(tokens::BG))
        .padding([0, 4])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::AMBER)),
            border: Border { radius: 7.0.into(), ..Default::default() },
            ..Default::default()
        });
    iced::widget::stack![
        container(with_tip).padding(Padding { top: 5.0, right: 6.0, bottom: 0.0, left: 0.0 }),
        container(badge).width(Length::Fill).align_x(Alignment::End),
    ]
    .into()
}

fn rail<'a>(app: &'a App, islands: Option<&[ninox_overview::islands::Island]>) -> Element<'a, Message> {
    let needs_you: usize = match islands {
        Some(islands) => islands.iter().map(|i| i.summary.needs_you).sum(),
        None => crate::components::fleet_islands::build_fleet_islands(app).iter().map(|i| i.summary.needs_you).sum(),
    };
    container(column![rail_button(true, needs_you)].align_x(Alignment::Center).padding(Padding { top: 10.0, right: 0.0, bottom: 0.0, left: 0.0 }))
        .width(Length::Fixed(RAIL_WIDTH))
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

/// An orchestrator row's click target: opens its island into the full-card
/// view (MLOPS-5339 slice 3 entry point "click an orchestrator row in the
/// tree"), switching to the Fleet tab if needed. Falls back to
/// `NavigateSession` if the orchestrator can't be found in any island
/// (shouldn't happen — every orchestrator belongs to its own or a parent's
/// island).
fn orchestrator_select_message(islands: Option<&[ninox_overview::islands::Island]>, id: &OrchestratorId) -> Message {
    islands
        .and_then(|islands| ninox_overview::nav::owning_island(islands, id))
        .map(Message::FleetOpenIsland)
        .unwrap_or_else(|| Message::NavigateSession(id.clone()))
}

/// A worker (or standalone session) row's click target: opens its owning
/// island with that node selected (MLOPS-5339 slice 3 entry point "click a
/// worker row in the tree"). Every session belongs to some island — its
/// orchestrator's, or the shared Unparented group for a standalone one
/// (`FleetNav::open_node` resolves both, see its own tests) — so this only
/// falls back to `NavigateSession` if `islands` itself hasn't been computed
/// (we're not on a view that threads it through).
fn node_select_message(islands: Option<&[ninox_overview::islands::Island]>, id: &str) -> Message {
    match islands.and_then(|islands| ninox_overview::nav::owning_island(islands, id)) {
        Some(_) => Message::FleetOpenNode(id.to_string()),
        None => Message::NavigateSession(id.to_string()),
    }
}

fn expanded<'a>(app: &'a App, fleet_islands: Option<&[ninox_overview::islands::Island]>) -> Element<'a, Message> {
    let trees = app.orchestrators.len();
    let workers = app
        .sessions
        .values()
        .filter(|s| s.orchestrator_id.is_some() && !app.orchestrators.iter().any(|o| o.id == s.id))
        .count();
    let mut items: Vec<Element<Message>> = vec![
        row![
            text("SESSIONS").size(10.5).font(crate::style::SANS_BOLD).color(tokens::DIM2),
            Space::new(Length::Fill, 0),
            text(header_counts(trees, workers)).size(10).font(crate::style::SANS).color(tokens::DIM2),
            rail_button(false, 0),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        // Lines up with the tree rows' own left inset ("This machine").
        .padding(Padding { top: 0.0, right: 0.0, bottom: 0.0, left: 10.0 })
        .into(),
        Space::new(0, 10).into(),
    ];

    for group in group_by_machine(app) {
        let is_remote = group.health.is_some() || !matches!(group.label.as_str(), "This machine");
        if is_remote {
            items.push(
                row![
                    container(Space::new(0, 0))
                        .width(Length::Fixed(6.0))
                        .height(Length::Fixed(6.0))
                        .style(move |_| container::Style {
                            background: Some(Background::Color(connection_dot_color(group.health))),
                            border: Border { radius: 3.0.into(), ..Default::default() },
                            ..Default::default()
                        }),
                    Space::new(6, 0),
                    text(format!("REMOTE · {}", group.label)).size(9.5).font(crate::style::SANS).color(tokens::VIOLET_BADGE_FG),
                    Space::new(6, 0),
                    text(connection_label(group.health)).size(9).color(tokens::DIM2),
                ]
                .align_y(Alignment::Center)
                .padding(Padding { top: 10.0, right: 10.0, bottom: 4.0, left: 10.0 })
                .into(),
            );
        } else if group.orchestrators.is_empty() {
            continue;
        } else {
            items.push(
                container(text(group.label.clone()).size(10).color(tokens::DIM2))
                    .padding(Padding { top: 10.0, right: 10.0, bottom: 4.0, left: 10.0 })
                    .into(),
            );
        }

        for orch in &group.orchestrators {
            let is_expanded = app.sidebar.expanded_orchestrators.contains(orch.id.as_str());
            let worker_count = app
                .sessions
                .values()
                .filter(|w| w.orchestrator_id.as_deref() == Some(orch.id.as_str()))
                .count();
            // "Selecting an orchestrator row highlights it" (round-1
            // feedback, item 3) — true once its island is the selected one,
            // which also covers Esc-return-to-Fleet preserving the
            // selection (`FleetNav`'s own doc comment).
            let fleet_selected = fleet_islands
                .and_then(|islands| ninox_overview::nav::owning_island(islands, &orch.id))
                .is_some_and(|key| app.fleet_nav.selected_island.as_ref() == Some(&key));
            items.push(tree_row(
                app,
                &orch.id,
                &orch.name,
                &worker_count_label(worker_count),
                app.sessions.get(orch.id.as_str()).map(|se| &se.status),
                app.unread_messages.get(orch.id.as_str()).copied().unwrap_or(0),
                true,
                false,
                Some((orch.id.clone(), is_expanded)),
                Some(Message::DeleteAsk(orch.id.clone())),
                orchestrator_select_message(fleet_islands, &orch.id),
                fleet_selected,
            ));
            if is_expanded {
                let mut workers: Vec<_> = app
                    .sessions
                    .values()
                    .filter(|w| w.orchestrator_id.as_deref() == Some(orch.id.as_str()))
                    .collect();
                workers.sort_by(|a, b| a.name.cmp(&b.name));
                for w in workers {
                    items.push(tree_row(
                        app, &w.id, &w.name, own_tree_right(repo_short(&w.repo), &orch.name),
                        Some(&w.status), 0, false, true, None,
                        Some(Message::DeleteAsk(w.id.clone())),
                        node_select_message(fleet_islands, &w.id),
                        false,
                    ));
                }
            }
        }
    }

    let mut standalone: Vec<_> = standalone_sessions(app).collect();
    standalone.sort_by(|a, b| a.name.cmp(&b.name));
    if !standalone.is_empty() {
        items.push(
            container(text("Standalone").size(10).color(tokens::DIM2))
                .padding(Padding { top: 10.0, right: 10.0, bottom: 4.0, left: 10.0 })
                .into(),
        );
    }
    for w in standalone {
        items.push(tree_row(
            app, &w.id, &w.name, repo_short(&w.repo),
            Some(&w.status), 0, false, false, None,
            Some(Message::DeleteAsk(w.id.clone())),
            node_select_message(fleet_islands, &w.id),
            false,
        ));
    }

    container(scrollable(column(items).width(Length::Fill)).height(Length::Fill))
        .width(Length::Fixed(EXPANDED_WIDTH))
        .height(Length::Fill)
        .padding(Padding { top: 6.0, right: 0.0, bottom: 0.0, left: 6.0 })
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

pub(crate) fn hover_tip<'a>(content: Element<'a, Message>, label: String) -> Element<'a, Message> {
    let tip = container(text(label).size(11).font(crate::style::SANS).color(tokens::TEXT))
        .padding([5, 8])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        });
    tooltip(content, tip, tooltip::Position::Bottom).gap(6).into()
}

fn name_tooltip<'a>(
    name_cell: container::Container<'a, Message>,
    full_name: String,
    truncated: bool,
) -> Element<'a, Message> {
    if truncated {
        hover_tip(name_cell.into(), full_name)
    } else {
        name_cell.into()
    }
}

#[allow(clippy::too_many_arguments)]
fn tree_row<'a>(
    app: &'a App,
    id: &str,
    name: &'a str,
    right: &str,
    status: Option<&'a SessionStatus>,
    unread: u64,
    bold: bool,
    indented: bool,
    chevron_toggle: Option<(OrchestratorId, bool)>,
    remove: Option<Message>,
    on_select: Message,
    fleet_selected: bool,
) -> Element<'a, Message> {
    let is_active = fleet_selected
        || matches!(&app.view, View::SessionDetail { session_id, .. } if session_id == id);
    let dot: Element<Message> = match status {
        Some(st) => {
            let hollow = matches!(st, SessionStatus::Done | SessionStatus::Terminated);
            let base = container(Space::new(0, 0))
                .width(Length::Fixed(8.0))
                .height(Length::Fixed(8.0))
                .style(move |_| container::Style {
                    background: (!hollow).then_some(Background::Color(tokens::status_color(st))),
                    border: Border { color: tokens::status_color(st), width: 1.5, radius: 4.0.into() },
                    ..Default::default()
                })
                .into();
            match app.sessions.get(id) {
                Some(session) => lifecycle_status::with_gate_tooltip(session, base),
                None => base,
            }
        }
        None => Space::new(8, 0).into(),
    };

    let retention_full: Option<String> = status
        .filter(|st| matches!(st, SessionStatus::Done | SessionStatus::Terminated))
        .and_then(|_| app.sessions.get(id))
        .and_then(|session| session.terminal_at)
        .and_then(|terminal_at| {
            lifecycle_status::retention_label(
                terminal_at,
                app.config.session_retention.retention_millis(),
                lifecycle_status::now_millis(),
            )
        });

    // `bold` drives the active/orchestrator-row colour below; the name
    // itself stays regular weight.
    let name_font = crate::style::SANS;
    let left_pad = if indented { 34.0 } else { 14.0 };

    // Everything in the row that isn't the name or its secondary labels:
    // container insets, dot, remove ×, chevron, unread badge, scrollbar.
    let fixed_px = 6.0 + 8.0 + left_pad + 16.0 + 10.0
        + if remove.is_some() { 24.0 } else { 0.0 }
        + if chevron_toggle.is_some() { 26.0 } else { 0.0 }
        + if unread > 0 { 24.0 } else { 0.0 };
    let right_opt = Some(right).filter(|r| !r.is_empty());
    let session_prs = app.session_prs.get(id).map(Vec::as_slice).unwrap_or_default();
    let chip_max = pr_chip_budget(EXPANDED_WIDTH - fixed_px, name, session_prs);
    // A session with PRs always shows a chip: when two or one don't fit beside
    // the name, it folds to a count chip and the name truncates instead.
    let chips_px = match (session_prs.is_empty(), chip_max) {
        (true, _) => 0.0,
        (false, 0) => pr_chip::count_width_estimate(session_prs) + 6.0,
        (false, max) => pr_chip::width_estimate(session_prs, max, SIDEBAR_LABEL_CHARS) + 6.0,
    };
    let plan = row_label_plan(EXPANDED_WIDTH - fixed_px - chips_px, name, right_opt, retention_full.as_deref());
    let shown_name = name_for_budget(name, plan.name_max_chars);
    let name_truncated = shown_name != name;

    let row_bg = move |hovered: bool| (is_active || hovered).then_some(Background::Color(tokens::PANEL2));

    let mut nav_row_items: Vec<Element<Message>> = vec![
        container(Space::new(0, 0)).width(3).height(Length::Fixed(18.0)).style(move |_| {
            container::Style {
                background: Some(Background::Color(if is_active { tokens::TEAL } else { iced::Color::TRANSPARENT })),
                ..Default::default()
            }
        }).into(),
        Space::new(left_pad - 3.0, 0).into(),
        dot,
        Space::new(8, 0).into(),
        name_tooltip(
            container(
                text(shown_name)
                    .size(12)
                    .font(name_font)
                    .color(if is_active || bold { tokens::TEXT } else { tokens::DIM })
                    .wrapping(iced::widget::text::Wrapping::None),
            )
        .width(Length::Fill)
        // Fixed height, not just `clip(true)` — a Shrink-height container
        // grows to fit a wrapped second line instead of cropping it, so
        // `clip` alone never engages (carry-over from slice 2 round-1
        // feedback item 3: rows still wrapped onto two lines despite
        // `Wrapping::None` + `clip(true)`). Pinning the height is what
        // actually forces a single line.
        .height(Length::Fixed(16.0))
        .align_y(iced::alignment::Vertical::Center)
        .clip(true),
            name.to_string(),
            name_truncated,
        ),
    ];
    if unread > 0 {
        nav_row_items.push(Space::new(6, 0).into());
        nav_row_items.push(
            container(text(unread_badge_text(unread)).size(8).font(crate::style::SANS).color(tokens::BG))
                .padding([1, 4])
                .style(|_| container::Style {
                    background: Some(Background::Color(tokens::TEAL)),
                    border: Border { radius: 7.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .into(),
        );
    }
    let chip = if chip_max > 0 {
        pr_chip::chips(id, session_prs, chip_max, SIDEBAR_LABEL_CHARS, false)
    } else {
        pr_chip::count_chip(id, session_prs)
    };
    if let Some(chip) = chip {
        nav_row_items.push(Space::new(6, 0).into());
        nav_row_items.push(chip);
    }
    if plan.show_right {
        nav_row_items.push(Space::new(6, 0).into());
        nav_row_items.push(
            text(right_label(right))
                .size(10)
                .font(MONO)
                .color(tokens::DIM2)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
        );
    }
    if let Some(full) = retention_full.as_deref() {
        let badge: Option<Element<Message>> = match plan.retention {
            RetentionForm::Full => {
                Some(text(full.to_string()).size(9.5).font(MONO).color(tokens::DIM2).into())
            }
            RetentionForm::Compact => Some(hover_tip(
                text(compact_retention(full)).size(9.5).font(MONO).color(tokens::DIM2).into(),
                full.to_string(),
            )),
            RetentionForm::Hidden => None,
        };
        if let Some(badge) = badge {
            nav_row_items.push(Space::new(6, 0).into());
            nav_row_items.push(badge);
        }
    }

    let navigate = button(row(nav_row_items).align_y(Alignment::Center))
        .on_press(on_select)
        .style(move |_t, status| button::Style {
            background: row_bg(matches!(status, button::Status::Hovered)),
            text_color: tokens::DIM,
            border: Border::default(),
            ..Default::default()
        })
        .padding(Padding { top: 3.0, right: 0.0, bottom: 3.0, left: 0.0 })
        .width(Length::Fill);

    let mut row_items: Vec<Element<Message>> = vec![navigate.into()];

    if let Some((toggle_id, is_open)) = chevron_toggle {
        row_items.push(Space::new(4, 0).into());
        row_items.push(
            button(text(if is_open { "▼" } else { "▶" }).size(13))
                .on_press(Message::SelectOrchestrator(toggle_id))
                .style(move |_t, status| {
                    let hovered = matches!(status, button::Status::Hovered);
                    button::Style {
                        background: row_bg(hovered),
                        text_color: if hovered { tokens::TEAL } else { tokens::DIM },
                        border: Border::default(),
                        ..Default::default()
                    }
                })
                .padding([2, 4])
                .into(),
        );
    }
    if let Some(remove_msg) = remove {
        row_items.push(
            button(text("×").size(13).color(tokens::DIM2))
                .on_press(remove_msg)
                .style(move |_t, status| button::Style {
                    background: row_bg(matches!(status, button::Status::Hovered)),
                    border: Border::default(),
                    ..Default::default()
                })
                .padding([2, 4])
                .into(),
        );
    }

    container(row(row_items).align_y(Alignment::Center))
        .padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 0.0 })
        .width(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn pr_chips_yield_to_the_name_in_a_narrow_row() {
        use ninox_core::{pr_events::CiVerdict, session_prs::{PrState, SessionPr}};
        let pr = |n| SessionPr {
            repo: "o/bellhop".into(), number: n, url: String::new(), title: String::new(), state: PrState::Open,
            draft: false, ci: CiVerdict::None, passing: 0, failing: 0, pending: 0,
            changes_requested: false, approved: false, tracked: false,
        };
        let prs = [pr(1), pr(2), pr(3)];
        assert_eq!(super::pr_chip_budget(1000.0, "worker", &prs), 2);
        assert_eq!(super::pr_chip_budget(1000.0, "worker", &[]), 0);
        assert_eq!(super::pr_chip_budget(90.0, "a-name-that-fills-the-row", &prs), 0);
        let one_fits = 6.0 * 5.8 + super::pr_chip::width_estimate(&prs, 1, super::SIDEBAR_LABEL_CHARS) + 6.0;
        assert_eq!(super::pr_chip_budget(one_fits, "worker", &prs), 1);
    }

    use super::*;

    #[test]
    fn the_shortcut_lives_only_in_the_rail_buttons_tooltip() {
        assert_eq!(rail_tooltip(true), "Show session tree  [");
        assert_eq!(rail_tooltip(false), "Hide session tree  [");
    }

    #[test]
    fn the_needs_you_badge_is_absent_at_zero_and_capped() {
        assert_eq!(needs_you_badge(0), None);
        assert_eq!(needs_you_badge(3).as_deref(), Some("3"));
        assert_eq!(needs_you_badge(42).as_deref(), Some("9+"));
    }

    #[test]
    fn the_header_counts_trees_and_workers() {
        assert_eq!(header_counts(19, 69), "19 trees · 69 workers");
        assert_eq!(header_counts(1, 1), "1 tree · 1 worker");
    }

    #[test]
    fn right_label_ellipsizes_a_long_value_instead_of_letting_it_grow_unbounded() {
        let long = "a-very-long-repository-path-that-would-otherwise-squeeze-the-name-column";
        let label = right_label(long);
        assert!(label.chars().count() <= RIGHT_LABEL_MAX_CHARS + 1, "got: {label}");
        assert!(label.ends_with('…'), "got: {label}");
    }

    #[test]
    fn right_label_leaves_a_short_value_alone() {
        assert_eq!(right_label("ninox"), "ninox");
    }

    #[test]
    fn worker_count_label_pluralizes_correctly() {
        assert_eq!(worker_count_label(0), "0 workers");
        assert_eq!(worker_count_label(1), "1 worker");
        assert_eq!(worker_count_label(2), "2 workers");
    }

    #[test]
    fn unread_badge_text_caps_at_two_digits() {
        assert_eq!(unread_badge_text(1), "1");
        assert_eq!(unread_badge_text(150), "99");
    }

    #[test]
    fn connection_dot_colors_match_legend() {
        assert_eq!(connection_dot_color(Some(MachineHealth::Connected)), tokens::GREEN);
        assert_eq!(connection_dot_color(Some(MachineHealth::Reconnecting)), tokens::AMBER);
        assert_eq!(connection_dot_color(Some(MachineHealth::Unreachable)), tokens::RED);
        assert_eq!(connection_dot_color(None), tokens::GREEN);
    }

    #[test]
    fn connection_label_text() {
        assert_eq!(connection_label(Some(MachineHealth::Reconnecting)), "reconnecting…");
        assert_eq!(connection_label(Some(MachineHealth::Unreachable)), "unreachable");
        assert_eq!(connection_label(Some(MachineHealth::Connected)), "connected");
    }

    fn island_with(key: ninox_overview::islands::IslandKey, root: Option<&str>, member_ids: Vec<&str>) -> ninox_overview::islands::Island {
        use ninox_overview::islands::{Island, IslandMember, IslandSummary};
        use ninox_overview::status::MemberStatus;
        Island {
            key, name: "x".into(), root: root.map(String::from),
            members: member_ids.into_iter().map(|id| IslandMember { id: id.into(), status: MemberStatus::Working }).collect(),
            summary: IslandSummary::default(), machine: None, pending_request: false,
        }
    }

    #[test]
    fn orchestrator_select_message_opens_its_island() {
        let islands = vec![island_with(ninox_overview::islands::IslandKey::Root("o".into()), Some("o"), vec!["o", "w"])];
        let msg = orchestrator_select_message(Some(&islands), &"o".to_string());
        assert!(matches!(msg, Message::FleetOpenIsland(key) if key == ninox_overview::islands::IslandKey::Root("o".into())));
    }

    #[test]
    fn orchestrator_select_message_falls_back_without_an_island() {
        let msg = orchestrator_select_message(None, &"o".to_string());
        assert!(matches!(msg, Message::NavigateSession(id) if id == "o"));
    }

    #[test]
    fn node_select_message_opens_the_owning_island_for_a_known_worker() {
        let islands = vec![island_with(ninox_overview::islands::IslandKey::Root("o".into()), Some("o"), vec!["o", "w"])];
        let msg = node_select_message(Some(&islands), "w");
        assert!(matches!(msg, Message::FleetOpenNode(id) if id == "w"));
    }

    #[test]
    fn node_select_message_resolves_the_unparented_group_too() {
        let islands = vec![island_with(ninox_overview::islands::IslandKey::Unparented, None, vec!["solo"])];
        let msg = node_select_message(Some(&islands), "solo");
        assert!(matches!(msg, Message::FleetOpenNode(id) if id == "solo"));
    }

    #[test]
    fn node_select_message_falls_back_to_navigate_session_without_islands() {
        let msg = node_select_message(None, "w");
        assert!(matches!(msg, Message::NavigateSession(id) if id == "w"));
    }

    const DEFAULT_ROW_PX: f32 = EXPANDED_WIDTH - 90.0;
    const MIN_ROW_PX: f32 = 150.0 - 90.0;
    const LONG_NAMES: [&str; 3] = [
        "v1-orchestrator-pid-identity",
        "v1-mlops-5339-gui-parity-b",
        "2026-q4-lo5-r-d-multi-node-workloads-can-be-distributed-across-regions",
    ];

    #[test]
    fn a_short_name_is_never_truncated_and_keeps_the_repo_label() {
        let plan = row_label_plan(DEFAULT_ROW_PX, "v1-ci", Some("ninox"), Some("Removing in 4d"));
        assert!(plan.show_right);
        assert!("v1-ci".len() <= plan.name_max_chars);
    }

    #[test]
    fn with_no_secondaries_the_full_label_shows_when_it_fits() {
        let plan = row_label_plan(400.0, "v1-ci", Some("ninox"), Some("Removing in 4d"));
        assert_eq!((plan.show_right, plan.retention), (true, RetentionForm::Full));
    }

    #[test]
    fn secondaries_drop_before_the_name_is_truncated() {
        for name in LONG_NAMES {
            let plan = row_label_plan(DEFAULT_ROW_PX, name, Some("ninox"), Some("Removing in 4d"));
            let all_dropped = row_label_plan(DEFAULT_ROW_PX, name, None, None);
            if name.chars().count() <= all_dropped.name_max_chars {
                assert!(name.chars().count() <= plan.name_max_chars, "{name} truncated while labels remained");
            } else {
                assert!(!plan.show_right && plan.retention == RetentionForm::Hidden, "{name}");
                assert_eq!(plan.name_max_chars, all_dropped.name_max_chars);
            }
        }
    }

    #[test]
    fn retention_goes_compact_before_it_goes_away() {
        let name = "v1-mlops-5339-gui";
        let plan = row_label_plan(DEFAULT_ROW_PX, name, None, Some("Removing in 4d"));
        assert_ne!(plan.retention, RetentionForm::Full);
        assert!(name.chars().count() <= plan.name_max_chars);
    }

    #[test]
    fn a_name_that_fits_exactly_at_minimum_width_gets_no_ellipsis() {
        let budget = row_label_plan(MIN_ROW_PX, "x", None, None).name_max_chars;
        let name: String = "a".repeat(budget);
        let plan = row_label_plan(MIN_ROW_PX, &name, None, None);
        assert_eq!(name_for_budget(&name, plan.name_max_chars), name);
        let one_more = format!("{name}a");
        let plan = row_label_plan(MIN_ROW_PX, &one_more, None, None);
        let shown = name_for_budget(&one_more, plan.name_max_chars);
        assert!(shown.ends_with('…'));
        assert!(shown.chars().count() <= plan.name_max_chars);
    }

    #[test]
    fn the_longest_name_ends_in_an_ellipsis_at_minimum_width() {
        let name = LONG_NAMES[2];
        let plan = row_label_plan(MIN_ROW_PX, name, Some("ninox"), Some("Removing in 4d"));
        assert!(!plan.show_right && plan.retention == RetentionForm::Hidden);
        assert!(name_for_budget(name, plan.name_max_chars).ends_with('…'));
    }

    #[test]
    fn the_repo_label_outlasts_the_retention_label() {
        let (name, right, retention) = ("v1-mlops-5339-gui", Some("ninox"), Some("Removing in 4d"));
        let plan = row_label_plan(DEFAULT_ROW_PX, name, right, retention);
        assert!(plan.show_right || plan.retention == RetentionForm::Hidden, "repo dropped while retention remained: {plan:?}");
    }

    #[test]
    fn compact_retention_strips_the_prefix() {
        assert_eq!(compact_retention("Removing in 4d"), "4d");
        assert_eq!(compact_retention("Removing shortly"), "<1m");
    }

    #[test]
    fn own_tree_rows_skip_a_repo_label_that_repeats_the_orchestrator() {
        assert_eq!(own_tree_right("ninox", "Ninox"), "");
        assert_eq!(own_tree_right("enact", "Ninox"), "enact");
    }
}
