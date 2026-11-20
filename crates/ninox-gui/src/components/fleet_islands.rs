//! The Fleet tab's islands view (MLOPS-5339 slice 2) — one card per root
//! orchestrator tree (plus one "Unparented sessions" card), matching
//! `FleetAtScale.dc.html`. Structure (membership, bucketing, sort order,
//! machine grouping) is drawn entirely from `ninox_overview::islands` —
//! this module only turns that into widgets and fills in per-member label/
//! chip text from the full `Session` the island model deliberately doesn't
//! carry (see `member_chip`'s doc comment). A click only selects; opening
//! an island (double-click/Enter) renders `components::fleet_focus`
//! instead (slice 3) — the detail panel (slice 4) still isn't built.
//! Several helpers here (`member_status_color`, `member_chip`, `chip_pill`,
//! `fold_expanded`, `seg_control`) are `pub(crate)` so `fleet_focus` can
//! reuse the same classification/styling rather than re-deriving it.

use std::collections::HashSet;

use iced::{
    widget::{button, column, container, mouse_area, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use ninox_core::fleet::{FleetMember, FleetSnapshot, Role};
use ninox_core::store::FleetRecord;
use ninox_core::types::{ActivityState, Session, SessionStatus};

use ninox_overview::islands::{
    build_islands, group_by_machine, sort_islands, Island, IslandKey, IslandMember, IslandSummary,
};
use ninox_overview::status::MemberStatus;

use super::pr_chip::{self, ChipPlan};
use super::shell::tokens;
use crate::app::{App, FleetMachineFilter, Message};

const ISLAND_SELECTED_BORDER: Color = Color { r: 0x2f as f32 / 255.0, g: 0x6f as f32 / 255.0, b: 0x6a as f32 / 255.0, a: 1.0 };
const ISLAND_REMOTE_BORDER:   Color = Color { r: 0x3b as f32 / 255.0, g: 0x33 as f32 / 255.0, b: 0x66 as f32 / 255.0, a: 1.0 };
const MACHINE_BAND_BG:        Color = Color { r: 0x1c as f32 / 255.0, g: 0x18 as f32 / 255.0, b: 0x33 as f32 / 255.0, a: 1.0 };
const BAR_EMPTY_BG:           Color = Color { r: 0x1d as f32 / 255.0, g: 0x22 as f32 / 255.0, b: 0x30 as f32 / 255.0, a: 1.0 };
const RAIL_LINE:              Color = Color { r: 0x34 as f32 / 255.0, g: 0x3c as f32 / 255.0, b: 0x50 as f32 / 255.0, a: 1.0 };
const CHIP_OPEN_BG:  Color = Color { r: 0x1e as f32 / 255.0, g: 0x27 as f32 / 255.0, b: 0x40 as f32 / 255.0, a: 1.0 };
const CHIP_OPEN_FG:  Color = Color { r: 0x9d as f32 / 255.0, g: 0xb4 as f32 / 255.0, b: 0xff as f32 / 255.0, a: 1.0 };
const CHIP_MERGE_BG: Color = Color { r: 0x15 as f32 / 255.0, g: 0x29 as f32 / 255.0, b: 0x1f as f32 / 255.0, a: 1.0 };
const CHIP_MERGE_FG: Color = Color { r: 0x7f as f32 / 255.0, g: 0xd8 as f32 / 255.0, b: 0xa8 as f32 / 255.0, a: 1.0 };
const CHIP_FAIL_BG:  Color = Color { r: 0x3a as f32 / 255.0, g: 0x1f as f32 / 255.0, b: 0x26 as f32 / 255.0, a: 1.0 };
const CHIP_FAIL_FG:  Color = Color { r: 0xf5 as f32 / 255.0, g: 0x8a as f32 / 255.0, b: 0xa0 as f32 / 255.0, a: 1.0 };
const SEG_ACTIVE_BG: Color = Color { r: 0x1f as f32 / 255.0, g: 0x3a as f32 / 255.0, b: 0x6e as f32 / 255.0, a: 1.0 };
const SEG_ACTIVE_FG: Color = Color { r: 0xcf as f32 / 255.0, g: 0xe0 as f32 / 255.0, b: 0xff as f32 / 255.0, a: 1.0 };

const ROW_HEIGHT: f32 = 20.0;

/// Builds the `FleetSnapshot` `build_islands` needs from the state the GUI
/// already holds in memory — no store/workspace I/O. `build_islands` only
/// reads `members[].session`/`.role` and `open_requests`, so every other
/// `FleetMember` field is a cheap default; `FleetSnapshot::assemble` only
/// recomputes anomalies (no I/O either).
pub fn fleet_snapshot(app: &App) -> FleetSnapshot {
    let orchestrator_ids: HashSet<&str> = app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let members: Vec<FleetMember> = app.sessions.values()
        // The prompt-agent singleton (MLOPS-5415) is never a fleet member —
        // same exclusion `ninox_core::fleet::snapshot::FleetSnapshot::load`
        // applies, but this in-memory rebuild (from `app.sessions`, not a
        // fresh `Store::load`) needs its own copy of the filter, since it
        // never calls `load` at all.
        .filter(|session| !ninox_core::worker_type::is_prompt_agent_session(session))
        .map(|session| {
            let role = if orchestrator_ids.contains(session.id.as_str()) {
                Role::Orchestrator
            } else if session.orchestrator_id.is_some() {
                Role::Worker
            } else {
                Role::Standalone
            };
            FleetMember {
                session: session.clone(),
                role,
                record: FleetRecord::default(),
                pr: None,
                ci: None,
                pending_inbox: 0,
                can_resume: true,
                workspace: Default::default(),
                anomalies: Vec::new(),
            }
        }).collect();
    let open_requests = app.engine.store.open_work_requests(None).unwrap_or_default();
    FleetSnapshot::assemble(members, open_requests, Vec::new(), 0)
}

/// The single place that calls `build_islands` — computed once per render by
/// `App::iced_view` (only while the Fleet tab is active) and shared by both
/// this module's card grid and the expanded session tree's "select an
/// orchestrator highlights its island" behavior, instead of each querying
/// the store and rebuilding the graph on its own.
pub fn build_fleet_islands(app: &App) -> Vec<Island> {
    let snapshot = fleet_snapshot(app);
    let machine_health = app.engine.store.machine_health().unwrap_or_default();
    build_islands(&snapshot, &machine_health)
}

pub(crate) fn member_status_color(status: MemberStatus) -> Color {
    match status {
        MemberStatus::Working => tokens::TEAL,
        MemberStatus::NeedsYou => tokens::AMBER,
        MemberStatus::WaitingOnPr => tokens::GRAY,
        MemberStatus::Done => tokens::GREEN,
        MemberStatus::Stopped => tokens::GRAY,
        MemberStatus::Queued => tokens::GRAY,
    }
}

/// A member's precise chip label/colors, read straight from its `Session` —
/// `MemberStatus` only carries the coarse bucket (`NeedsYou` covers both a
/// CI failure and blocked activity), and the design calls for the specific
/// cause (`CI failed` vs `needs you`) plus the PR number, so this reads the
/// session directly instead of asking the island model to carry more than
/// its buckets. `None` for `Working`/`Done`/`Stopped` — no chip in the
/// design. Takes `status` (the member's own `classify_member` outcome, not
/// re-derived here) so a `Done`/`Stopped` member whose `Session` still
/// carries a stale `activity: Blocked` from before it stopped doesn't get a
/// "needs you" chip next to its green/gray dot — `classify_member` already
/// gives terminal status precedence over a blocked activity flag, and this
/// must agree with it rather than re-checking `activity` on its own.
#[cfg(test)]
pub(crate) fn member_chip(status: MemberStatus, session: &Session, queue_position: Option<usize>) -> Option<(String, Color, Color)> {
    member_chip_with_prs(status, session, queue_position, &[])
}

/// [`member_chip`] without the plain `PR #n` text when `prs` is non-empty:
/// those render as clickable chips (`pr_chip`) instead.
pub(crate) fn member_chip_with_prs(
    status: MemberStatus,
    session: &Session,
    queue_position: Option<usize>,
    prs: &[ninox_core::session_prs::SessionPr],
) -> Option<(String, Color, Color)> {
    use SessionStatus::*;
    if matches!(status, MemberStatus::Done | MemberStatus::Stopped) {
        return None;
    }
    match session.status {
        Queued => Some((ninox_ui_shared::session_queue::queued_badge(queue_position), tokens::PANEL2, tokens::DIM)),
        CiFailed => Some(("CI failed".to_string(), CHIP_FAIL_BG, CHIP_FAIL_FG)),
        Mergeable => Some(("mergeable".to_string(), CHIP_MERGE_BG, CHIP_MERGE_FG)),
        // Listed PRs are drawn as clickable chips by the caller.
        PrOpen if !prs.is_empty() => None,
        PrOpen => Some((
            session.pr_number.map(|n| format!("PR #{n}")).unwrap_or_else(|| "PR open".to_string()),
            CHIP_OPEN_BG, CHIP_OPEN_FG,
        )),
        ReviewPending => Some(("review".to_string(), CHIP_OPEN_BG, CHIP_OPEN_FG)),
        _ if session.activity == ActivityState::Blocked => {
            Some(("needs you".to_string(), tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG))
        }
        _ => None,
    }
}

fn island_meta(summary: &IslandSummary) -> String {
    let total = summary.total();
    let active = summary.active();
    if total == 0 {
        String::new()
    } else if total == active {
        active.to_string()
    } else {
        format!("{active} / {total}")
    }
}

fn machine_label(app: &App, machine_id: &str) -> String {
    app.config.remote_machines.machines.iter()
        .find(|m| m.id == machine_id)
        .map(|m| m.label.clone())
        .unwrap_or_else(|| machine_id.to_string())
}

fn dot<'a>(color: Color, size: f32) -> Element<'a, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: (size / 2.0).into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

fn tick<'a>() -> Element<'a, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(9.0))
        .height(Length::Fixed(1.0))
        .style(|_| container::Style { background: Some(Background::Color(RAIL_LINE)), ..Default::default() })
        .into()
}

pub(crate) fn chip_pill<'a>(label: String, bg: Color, fg: Color) -> Element<'a, Message> {
    container(text(label).size(9.5).font(crate::style::SANS).color(fg))
        .padding([1, 6])
        .style(move |_| container::Style {
            background: Some(Background::Color(bg)),
            border: Border { radius: 9.0.into(), ..Default::default() },
            ..Default::default()
        })
        .into()
}

// These take `app: &'a App` (the only borrow the returned `Element<'a, _>`
// actually needs to stay alive) alongside plain, separately-elided
// `&Island`/`&IslandMember`/`&IslandMachine` borrows — every piece of island
// data they read is copied into an owned `String`/`Color` before it reaches
// a widget, so the output never borrows from them. Tying both to the same
// `'a` instead would force the whole call chain's `Element` down to
// whichever of the two borrows is shorter-lived (the islands `Vec` that
// `fleet_islands` rebuilds on every render), which the pub `fleet_islands`
// function then can't return as `Element<'_, Message>` tied to `app` alone.
/// Label cap for a member's PR chips; the budget folds to fewer or a count chip first.
const ISLAND_ROW_LABEL_CHARS: usize = 14;
const ISLAND_ROW_SHORT_LABEL_CHARS: usize = 8;
const MEMBER_NAME_SIZE: f32 = 11.0;
const HEAD_NAME_SIZE: f32 = 12.5;
/// The island head's context meter bar.
const METER_BAR_PX: f32 = 34.0;
/// Card insets: 18px board padding each side, 14px between columns.
const BOARD_PAD_PX: f32 = 36.0;
const COLUMN_GAP_PX: f32 = 14.0;
/// Head: 11px card padding each side plus the status dot and its gap.
const HEAD_FIXED_PX: f32 = 22.0 + 8.0 + 7.0 + CARD_BORDER_PX;
/// Member row inside the rail: 15 left + 1 line + 7 gap + 11 right padding,
/// plus the tick, status dot and their spacing.
const ROW_FIXED_PX: f32 = 34.0 + 9.0 + 7.0 + 12.0 + CARD_BORDER_PX;
/// The card's border, both sides, at its 2px selected width.
const CARD_BORDER_PX: f32 = 4.0;

fn board_columns(app: &App) -> usize {
    if app.fleet_nav.tree_collapsed { 5 } else { 4 }
}

/// Logical width of one island card, from the window width (which already
/// accounts for the zoom) less the sidebar and the board's insets.
pub(crate) fn island_card_px(app: &App) -> f32 {
    let sidebar = if app.fleet_nav.tree_collapsed { super::shell::tree::RAIL_WIDTH } else { super::shell::tree::EXPANDED_WIDTH };
    let cols = board_columns(app) as f32;
    // A selected ghost opens a detail panel (and its divider) beside the board;
    // a selected card only raises the bottom action row.
    let detail = if app.ghost.selected.is_some() {
        super::fleet_detail::PANEL_WIDTH + 1.0
    } else {
        0.0
    };
    ((app.window_width - sidebar - detail - BOARD_PAD_PX - COLUMN_GAP_PX * (cols - 1.0)) / cols).max(120.0)
}

/// A single-line name cell: the pinned height is what keeps it to one line
/// (the text otherwise re-wraps inside whatever width it is given).
fn name_cell<'a>(full: &str, shown: String, size: f32, line_px: f32) -> Element<'a, Message> {
    let truncated = shown != full;
    let cell = container(
        text(shown)
            .size(size)
            .font(crate::style::SANS)
            .color(tokens::TEXT)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(Length::Fill)
    .height(Length::Fixed(line_px))
    .align_y(iced::alignment::Vertical::Center)
    .clip(true);
    if truncated {
        super::shell::tree::hover_tip(cell.into(), full.to_string())
    } else {
        cell.into()
    }
}

/// The name as shown and the PR chip plan for a member row, from the row's
/// width budget.
pub(crate) fn member_row_parts(app: &App, member: &IslandMember) -> (String, Option<ChipPlan>) {
    let session = app.sessions.get(&member.id);
    let name = session.map(|s| s.name.as_str()).unwrap_or(member.id.as_str());
    let prs = app.session_prs.get(&member.id).map(Vec::as_slice).unwrap_or_default();
    let chip = session.and_then(|s| member_chip_with_prs(member.status, s, app.queue_positions.get(&member.id).copied(), prs));
    let status_px = chip.as_ref().map_or(0.0, |(label, ..)| 6.0 + pr_chip::text_px(label, 9.5) + 12.0);
    let avail = island_card_px(app) - ROW_FIXED_PX - status_px;
    let chip_plan = (!prs.is_empty()).then(|| {
        pr_chip::plan(prs, pr_chip::text_px(name, MEMBER_NAME_SIZE), avail, 2, ISLAND_ROW_LABEL_CHARS, Some(ISLAND_ROW_SHORT_LABEL_CHARS))
    });
    let name_px = avail - chip_plan.map_or(0.0, |p| p.width(prs));
    (pr_chip::fit_name(name, MEMBER_NAME_SIZE, name_px), chip_plan)
}

pub(crate) fn member_row<'a>(app: &'a App, island: &IslandKey, member: &IslandMember) -> Element<'a, Message> {
    let session = app.sessions.get(&member.id);
    let name = session.map(|s| s.name.as_str()).unwrap_or(member.id.as_str());
    let prs = app.session_prs.get(&member.id).map(Vec::as_slice).unwrap_or_default();
    let chip = session.and_then(|s| member_chip_with_prs(member.status, s, app.queue_positions.get(&member.id).copied(), prs));
    let (shown, chip_plan) = member_row_parts(app, member);

    let mut items: Vec<Element<Message>> = vec![
        tick(),
        dot(member_status_color(member.status), 7.0),
        name_cell(name, shown, MEMBER_NAME_SIZE, 16.0),
    ];
    if let Some((label, bg, fg)) = chip {
        items.push(chip_pill(label, bg, fg));
    }
    if let Some(chips) = chip_plan.and_then(|p| pr_chip::render(&member.id, prs, p, false)) {
        items.push(chips);
    }

    mouse_area(row(items).align_y(Alignment::Center).spacing(6))
        .on_press(Message::FleetIslandMemberPressed { island: island.clone(), id: member.id.clone() })
        .into()
}

fn fold_row<'a>(key: IslandKey, done: usize, stopped: usize) -> Element<'a, Message> {
    let mut parts = Vec::new();
    if done > 0 { parts.push(format!("{done} done")); }
    if stopped > 0 { parts.push(format!("{stopped} stopped")); }
    button(
        row![
            tick(),
            // Inter has no "▸"; Advanced shaping falls back per glyph.
            text("▸").size(10.5).color(tokens::DIM2).shaping(iced::widget::text::Shaping::Advanced),
            text(parts.join(" · ")).size(10.5).font(crate::style::SANS).color(tokens::DIM2),
        ]
        .align_y(Alignment::Center)
        .spacing(4),
    )
    .on_press(Message::FleetToggleFold(key))
    .padding(0)
    .style(|_t, _status| button::Style { background: None, text_color: tokens::DIM2, ..Default::default() })
    .into()
}

fn remote_band<'a>(app: &'a App, machine: &ninox_overview::islands::IslandMachine) -> Element<'a, Message> {
    let (health_label, health_color) = match machine.health {
        Some(ninox_core::types::MachineHealth::Connected) | None => ("connected", tokens::GREEN),
        Some(ninox_core::types::MachineHealth::Reconnecting) => ("reconnecting", tokens::AMBER),
        Some(ninox_core::types::MachineHealth::Unreachable) => ("unreachable", tokens::RED),
    };
    container(
        row![
            text(format!("REMOTE · {}", machine_label(app, &machine.machine_id)))
                .size(10)
                .font(crate::style::SANS)
                .color(tokens::VIOLET_BADGE_FG),
            Space::new(Length::Fill, 0),
            dot(health_color, 6.0),
            Space::new(4, 0),
            text(health_label).size(9.5).font(crate::style::SANS).color(tokens::DIM),
        ]
        .align_y(Alignment::Center),
    )
    .padding(Padding { top: 4.0, right: 11.0, bottom: 4.0, left: 11.0 })
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(MACHINE_BAND_BG)),
        border: Border { color: ISLAND_REMOTE_BORDER, width: 0.0, radius: 0.0.into() },
        ..Default::default()
    })
    .into()
}

fn status_bar<'a>(summary: &IslandSummary) -> Element<'a, Message> {
    let segs: Vec<(usize, Color)> = [
        (summary.working, tokens::TEAL),
        (summary.needs_you, tokens::AMBER),
        (summary.waiting_on_pr, tokens::GRAY),
        (summary.done, tokens::GREEN),
    ].into_iter().filter(|(n, _)| *n > 0).collect();

    let bar: Element<Message> = if segs.is_empty() {
        container(Space::new(0, 0)).width(Length::Fill).height(Length::Fill).style(|_| {
            container::Style { background: Some(Background::Color(BAR_EMPTY_BG)), ..Default::default() }
        }).into()
    } else {
        row(segs.into_iter().map(|(n, color)| {
            container(Space::new(0, 0))
                .width(Length::FillPortion(n as u16))
                .height(Length::Fill)
                .style(move |_| container::Style { background: Some(Background::Color(color)), ..Default::default() })
                .into()
        }).collect::<Vec<Element<Message>>>()).height(Length::Fill).into()
    };

    container(bar)
        .width(Length::Fill)
        .height(Length::Fixed(3.0))
        .padding(Padding { top: 0.0, right: 11.0, bottom: 0.0, left: 11.0 })
        .style(|_| container::Style { border: Border { radius: 2.0.into(), ..Default::default() }, ..Default::default() })
        .into()
}

/// The context meter for an island's head: only when its root is an
/// orchestrator (an island rooted at a standalone or worker session has no
/// context to reset).
pub(crate) fn island_meter(app: &App, island: &Island) -> Option<ninox_ui_shared::context_meter::ContextMeter> {
    let root = island.root.as_ref()?;
    if !app.orchestrators.iter().any(|o| &o.id == root) {
        return None;
    }
    let session = app.sessions.get(root)?;
    Some(ninox_ui_shared::context_meter::ContextMeter::for_session(session, &app.config.orchestrator_reset))
}

pub(crate) fn island_head<'a>(app: &App, island: &Island) -> Element<'a, Message> {
    use crate::components::pr_chip::{fit_name, text_px};
    let badge = island.needs_attention().then(|| format!("{} NEED YOU", island.summary.needs_you));
    let meta = Some(island_meta(&island.summary)).filter(|m| !m.is_empty());

    // The title keeps its room first; the meta is dropped before the title
    // is cut, and the badge is the last thing to give way.
    let badge_px = badge.as_ref().map_or(0.0, |b| 7.0 + text_px(b, 9.5) + 12.0);
    let meta_px = meta.as_ref().map_or(0.0, |m| 7.0 + text_px(m, 10.0) * 1.2);
    let avail = island_card_px(app) - HEAD_FIXED_PX;
    let name_px = text_px(&island.name, HEAD_NAME_SIZE);
    // The orchestrator's context meter; it goes after the meta, which is
    // dropped first when the head is short of room.
    let meter = island_meter(app, island);
    let meter_w = crate::components::context_meter::meter_px(METER_BAR_PX, 9.5) + 7.0;
    let show_meter = meter.is_some() && name_px + badge_px + meter_w <= avail;
    let meter_reserved = if show_meter { meter_w } else { 0.0 };
    let show_meta = meta.is_some() && name_px + badge_px + meter_reserved + meta_px <= avail;
    let reserved = badge_px + meter_reserved + if show_meta { meta_px } else { 0.0 };

    let mut items: Vec<Element<Message>> = vec![
        dot(tokens::TEAL, 8.0),
        name_cell(&island.name, fit_name(&island.name, HEAD_NAME_SIZE, avail - reserved), HEAD_NAME_SIZE, 18.0),
    ];
    if let Some(badge) = badge {
        items.push(chip_pill(badge, tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG));
    }
    if let Some(meta) = meta.filter(|_| show_meta) {
        items.push(text(meta).size(10).font(crate::style::MONO).color(tokens::DIM2).wrapping(iced::widget::text::Wrapping::None).into());
    }
    if let Some(meter) = meter.filter(|_| show_meter) {
        items.push(crate::components::context_meter::context_meter(meter, METER_BAR_PX, 9.5));
    }
    row(items).align_y(Alignment::Center).spacing(7)
        .padding(Padding { top: 9.0, right: 11.0, bottom: 7.0, left: 11.0 })
        .into()
}

/// Whether `key`'s folded done/stopped row is currently shown unfolded —
/// `fleet_compact`'s baseline (folded when Compact, unfolded when Full
/// cards) flipped per-island by a manual `FleetToggleFold`. Shared by
/// `member_rail` (what to actually render) and `toolbar` (the "N finished
/// folded" total) so the two can't disagree about which islands are folded.
pub(crate) fn fold_expanded(app: &App, key: &IslandKey) -> bool {
    !app.fleet_compact ^ app.fleet_expanded_folds.contains(key)
}

/// Everything below an island's head/status-bar: visible (non-folded)
/// member rows, then either the folded summary row or (if expanded) the
/// done/stopped rows it would otherwise hide, or an "orchestrator only"
/// empty note when the island has no members besides its own root.
fn member_rail<'a>(app: &'a App, island: &Island) -> Element<'a, Message> {
    let others: Vec<&IslandMember> = island.members.iter()
        .filter(|m| Some(&m.id) != island.root.as_ref())
        .collect();
    let (folded, visible): (Vec<&IslandMember>, Vec<&IslandMember>) = others.into_iter()
        .partition(|m| matches!(m.status, MemberStatus::Done | MemberStatus::Stopped));
    let done = folded.iter().filter(|m| m.status == MemberStatus::Done).count();
    let stopped = folded.len() - done;

    let expanded = fold_expanded(app, &island.key);

    let mut rows: Vec<Element<Message>> = visible.iter().map(|m| member_row(app, &island.key, m)).collect();
    if !folded.is_empty() {
        if expanded {
            rows.extend(folded.iter().map(|m| member_row(app, &island.key, m)));
        } else {
            rows.push(fold_row(island.key.clone(), done, stopped));
        }
    } else if rows.is_empty() {
        rows.push(row![tick(), text("no workers — orchestrator only").size(10.5).font(crate::style::SANS_ITALIC).color(tokens::DIM2)].spacing(6).into());
    }

    let strip_height = (rows.len() as f32 * ROW_HEIGHT).max(ROW_HEIGHT);
    row![
        container(Space::new(0, 0)).width(Length::Fixed(1.0)).height(Length::Fixed(strip_height))
            .style(|_| container::Style { background: Some(Background::Color(RAIL_LINE)), ..Default::default() }),
        Space::new(7, 0),
        column(rows).spacing(3).width(Length::Fill),
    ]
    .padding(Padding { top: 0.0, right: 11.0, bottom: 9.0, left: 15.0 })
    .into()
}

fn island_card<'a>(app: &'a App, island: &Island, selected: bool) -> Element<'a, Message> {
    let is_remote = island.machine.is_some();
    let key = island.key.clone();

    let mut sections: Vec<Element<Message>> = Vec::new();
    if let Some(machine) = &island.machine {
        sections.push(remote_band(app, machine));
    }
    sections.push(island_head(app, island));
    sections.push(status_bar(&island.summary));
    sections.push(member_rail(app, island));

    let card = container(column(sections))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border {
                color: if selected {
                    ISLAND_SELECTED_BORDER
                } else if is_remote {
                    ISLAND_REMOTE_BORDER
                } else {
                    tokens::BORDER
                },
                width: if selected { 2.0 } else { 1.0 },
                radius: 9.0.into(),
            },
            ..Default::default()
        });

    // `mouse_area`, not an outer `button` — the fold row inside is its own
    // `button`, and an iced `button` swallows presses on any button nested
    // inside it (same reasoning as `shell::nav`'s tab/badge split).
    iced::widget::mouse_area(card)
        .on_press(Message::FleetSelectIsland(key))
        .into()
}

/// Distributes `islands` into `columns` vertical lists, always placing the
/// next island into the currently-shortest column (estimated by member
/// count) — iced has no CSS multi-column layout to fill top-to-bottom per
/// column, so this approximates the same balanced, read-top-to-bottom-then-
/// across packing `FleetAtScale.dc.html`'s `column-count` gives for free.
fn pack_columns<'i>(islands: &[&'i Island], columns: usize) -> Vec<Vec<&'i Island>> {
    let mut cols: Vec<Vec<&Island>> = vec![Vec::new(); columns.max(1)];
    let mut heights = vec![0usize; columns.max(1)];
    for island in islands.iter().copied() {
        let weight = 4 + island.members.len();
        let idx = heights.iter().enumerate().min_by_key(|(_, h)| **h).map(|(i, _)| i).unwrap_or(0);
        cols[idx].push(island);
        heights[idx] += weight;
    }
    cols
}

/// A bordered, pill-grouped button row (`FleetAtScale.dc.html`'s `.seg`) —
/// used for the toolbar's Machine/Sort/density controls.
pub(crate) fn seg_control<'a>(options: Vec<(String, bool, Message)>) -> Element<'a, Message> {
    container(
        row(options.into_iter().map(|(label, active, msg)| {
            button(text(label).size(10.5).font(crate::style::SANS).color(if active { SEG_ACTIVE_FG } else { tokens::DIM }))
                .on_press(msg)
                .padding([5, 10])
                .style(move |_t, _status| button::Style {
                    background: Some(Background::Color(if active { SEG_ACTIVE_BG } else { tokens::PANEL2 })),
                    text_color: if active { SEG_ACTIVE_FG } else { tokens::DIM },
                    border: Border::default(),
                    ..Default::default()
                })
                .into()
        }).collect::<Vec<Element<Message>>>()).wrap(),
    )
    .style(|_| container::Style {
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .into()
}

/// `all_islands` (unfiltered) supplies the Machine filter's own options —
/// so switching to "Local" doesn't make the other machines' chips vanish —
/// while every other total is computed from `shown` (`all_islands` after the
/// current Machine filter), matching what the card grid below actually
/// renders.
fn toolbar<'a>(app: &'a App, all_islands: &[Island], shown: &[&Island]) -> Element<'a, Message> {
    let trees = shown.iter().filter(|i| matches!(i.key, IslandKey::Root(_))).count();
    let workers: usize = shown.iter().map(|i| i.summary.total()).sum();
    let needs_you: usize = shown.iter().map(|i| i.summary.needs_you).sum();
    let finished_folded: usize = shown.iter().map(|i| {
        if fold_expanded(app, &i.key) { 0 } else { i.summary.done + i.summary.stopped }
    }).sum();

    let mut totals = format!("{trees} orchestrator trees · {workers} workers");
    if needs_you > 0 {
        totals.push_str(&format!(" · {needs_you} need you"));
    }
    if finished_folded > 0 {
        totals.push_str(&format!(" · {finished_folded} finished folded"));
    }

    let machine_groups = group_by_machine(all_islands);
    let mut machine_options = vec![
        ("All".to_string(), app.fleet_machine_filter == FleetMachineFilter::All, Message::FleetFilterMachine(FleetMachineFilter::All)),
        ("Local".to_string(), app.fleet_machine_filter == FleetMachineFilter::Local, Message::FleetFilterMachine(FleetMachineFilter::Local)),
    ];
    for machine_id in machine_groups.keys().flatten() {
        machine_options.push((
            machine_label(app, machine_id),
            app.fleet_machine_filter == FleetMachineFilter::Machine(machine_id.clone()),
            Message::FleetFilterMachine(FleetMachineFilter::Machine(machine_id.clone())),
        ));
    }

    let sort_options = vec![
        ("Needs you first".to_string(), app.fleet_sort == ninox_overview::islands::IslandSort::NeedsYouFirst, Message::FleetSetSort(ninox_overview::islands::IslandSort::NeedsYouFirst)),
        ("Most active".to_string(), app.fleet_sort == ninox_overview::islands::IslandSort::MostActive, Message::FleetSetSort(ninox_overview::islands::IslandSort::MostActive)),
        ("A–Z".to_string(), app.fleet_sort == ninox_overview::islands::IslandSort::AToZ, Message::FleetSetSort(ninox_overview::islands::IslandSort::AToZ)),
    ];

    let density_options = vec![
        ("Full cards".to_string(), !app.fleet_compact, Message::FleetSetCompact(false)),
        ("Compact".to_string(), app.fleet_compact, Message::FleetSetCompact(true)),
    ];

    let reapable = super::reap_modal::reapable(&app.sessions, &app.orchestrators).len();
    let cleanup = (reapable > 0).then(|| cleanup_button(reapable));
    // Not a nav tab: the top bar has no width left for one.
    let idle_n = app.idle.report.total;
    let cleanup: Option<Element<'a, Message>> = match (cleanup, (idle_n > 0).then(|| idle_button(idle_n))) {
        (Some(c), Some(i)) => Some(row![c, i].spacing(12).align_y(Alignment::Center).into()),
        (c, i) => c.or(i),
    };
    // "6/6 workers running locally · 3 orchestrators · 2 queued" leads the totals.
    if !app.limits_headline.is_empty() {
        totals = format!("{} · {totals}", app.limits_headline);
    }

    let width = super::fleet_focus::canvas_width(app, all_islands);
    toolbar_row(width, totals, cleanup, machine_options, sort_options, density_options)
}

fn idle_button<'a>(n: usize) -> Element<'a, Message> {
    iced::widget::button(text(format!("Idle workers ({n})")).size(11).font(crate::style::SANS).color(tokens::AMBER))
        .on_press(Message::NavigateIdle)
        .padding([4, 10])
        .style(|_t, status| iced::widget::button::Style {
            background: Some(Background::Color(if matches!(status, iced::widget::button::Status::Hovered) { tokens::PANEL2 } else { tokens::PANEL })),
            text_color: tokens::AMBER,
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
}

fn cleanup_button<'a>(n: usize) -> Element<'a, Message> {
    iced::widget::button(text(format!("Clean up finished ({n})")).size(11).font(crate::style::SANS).color(tokens::TEXT))
        .on_press(Message::ReapFinishedOpen)
        .padding([4, 10])
        .style(|_t, status| iced::widget::button::Style {
            background: Some(Background::Color(if matches!(status, iced::widget::button::Status::Hovered) { tokens::PANEL2 } else { tokens::PANEL })),
            text_color: tokens::TEXT,
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
}

type SegOptions = Vec<(String, bool, Message)>;

/// Below this width the one-line toolbar squeezes its controls (the labels
/// collapse to a letter per line), so the controls move to their own
/// wrapping line under the title instead. Its natural one-line width with a
/// couple of remote machines; bump if the toolbar gains controls.
const ONE_LINE_TOOLBAR_MIN_WIDTH: f32 = 1230.0;

/// `width`: the canvas width the toolbar spans (`fleet_focus::canvas_width`).
fn toolbar_row<'a>(width: f32, totals: String, cleanup: Option<Element<'a, Message>>, machine: SegOptions, sort: SegOptions, density: SegOptions) -> Element<'a, Message> {
    let title = row![
        text("Fleet").size(12).font(crate::style::SANS).color(tokens::TEXT),
        text(totals).size(11).font(crate::style::SANS).color(tokens::DIM),
    ]
    .spacing(12)
    .align_y(Alignment::Center);
    let labelled = |label: &'static str, options: SegOptions| {
        row![text(label).size(10.5).font(crate::style::SANS).color(tokens::DIM2), seg_control(options)]
            .spacing(12)
            .align_y(Alignment::Center)
    };
    let controls = row![]
        .push_maybe(cleanup)
        .push(labelled("Machine", machine))
        .push(labelled("Sort", sort))
        .push(seg_control(density))
    .spacing(12)
    .align_y(Alignment::Center)
    .wrap();
    let padding = Padding { top: 10.0, right: 18.0, bottom: 10.0, left: 18.0 };
    if width >= ONE_LINE_TOOLBAR_MIN_WIDTH {
        row![title, Space::new(Length::Fill, 0), controls]
            .align_y(Alignment::Center)
            .spacing(12)
            .padding(padding)
            .into()
    } else {
        column![title, controls].spacing(8).padding(padding).into()
    }
}

pub fn fleet_islands<'a>(app: &'a App, islands: &[Island]) -> Element<'a, Message> {
    let mut islands = islands.to_vec();
    sort_islands(&mut islands, app.fleet_sort);

    let visible: Vec<&Island> = islands.iter().filter(|i| app.fleet_machine_filter.matches(i)).collect();

    let header = container(toolbar(app, &islands, &visible))
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::BG)),
            border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        });

    let ghosts = ninox_ui_shared::ghost_view::ghost_islands(&app.ghost.plans);
    let body: Element<Message> = if visible.is_empty() && ghosts.is_empty() {
        container(text("No fleet sessions yet.").size(13).font(crate::style::SANS).color(tokens::DIM))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
    } else {
        let columns = board_columns(app);
        let cols = pack_columns(&visible, columns.min(visible.len().max(1)).max(1));
        let mut cells: Vec<Vec<Element<Message>>> = cols.into_iter().map(|col_islands| {
            col_islands.into_iter().map(|island| {
                island_card(app, island, app.fleet_nav.selected_island.as_ref() == Some(&island.key))
            }).collect::<Vec<Element<Message>>>()
        }).collect();
        cells.resize_with(columns, Vec::new);
        // Proposed islands go after the real ones, into the shortest columns.
        for plan in ghosts {
            let shortest = (0..cells.len()).min_by_key(|&i| cells[i].len()).unwrap_or(0);
            cells[shortest].push(crate::components::ghost_plans::ghost_island(app, plan));
        }
        let col_elements: Vec<Element<Message>> = cells.into_iter().map(|col| {
            column(col).spacing(14).width(Length::FillPortion(1)).into()
        }).collect();

        let board = scrollable(
            row(col_elements).spacing(14).padding(Padding { top: 0.0, right: 18.0, bottom: 14.0, left: 18.0 }),
        )
        .direction(scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(4).scroller_width(4)))
        .style(|_t, _status| scrollable::Style {
            container: container::Style::default(),
            vertical_rail: scrollable::Rail {
                background: None,
                border: Border::default(),
                scroller: scrollable::Scroller { color: tokens::BORDER, border: Border::default() },
            },
            horizontal_rail: scrollable::Rail {
                background: None,
                border: Border::default(),
                scroller: scrollable::Scroller { color: tokens::BORDER, border: Border::default() },
            },
            gap: None,
        })
        .width(Length::Fill)
        .height(Length::Fill);
        // Cards and their buttons capture their own presses first, so this
        // only fires on empty board space.
        iced::widget::mouse_area(board).on_press(Message::FleetClearBoardSelection).into()
    };

    container(column![header, body].width(Length::Fill).height(Length::Fill))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::BG)), ..Default::default() })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_toolbar(width: f32) -> iced::advanced::layout::Node {
        layout_toolbar_with(width, &["All", "Local", "mac-studio"])
    }

    fn layout_toolbar_with(width: f32, machines: &[&str]) -> iced::advanced::layout::Node {
        use iced::advanced::layout::Limits;
        use iced::advanced::widget::Tree;
        use iced::{Font, Pixels, Size};
        let seg = |labels: &[&str]| -> SegOptions {
            labels.iter().enumerate().map(|(i, l)| (l.to_string(), i == 0, Message::Noop)).collect()
        };
        let bar = toolbar_row(
            width,
            "3 orchestrator trees · 12 workers · 2 need you · 4 finished folded".into(),
            Some(cleanup_button(12)),
            seg(machines),
            seg(&["Needs you first", "Most active", "A–Z"]),
            seg(&["Full cards", "Compact"]),
        );
        crate::style::load_bundled_fonts();
        let renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(Font::default(), Pixels(16.0)));
        let mut tree = Tree::new(bar.as_widget());
        bar.as_widget().layout(&mut tree, &renderer, &Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)))
    }

    fn leaves(node: &iced::advanced::layout::Node, out: &mut Vec<iced::Size>) {
        if node.children().is_empty() {
            out.push(node.size());
        }
        for child in node.children() {
            leaves(child, out);
        }
    }

    /// At ~700px (a 745px window less the 44px rail) the controls used to be
    /// squeezed until "Compact" rendered one letter per line. 1100 is the
    /// narrowest one-line layout, so it pins `ONE_LINE_TOOLBAR_MIN_WIDTH`.
    #[test]
    fn the_toolbar_never_squeezes_its_labels_at_any_width() {
        for width in [420.0, 700.0, ONE_LINE_TOOLBAR_MIN_WIDTH - 1.0, ONE_LINE_TOOLBAR_MIN_WIDTH, 1600.0] {
            let node = layout_toolbar(width);
            let mut sizes = Vec::new();
            leaves(&node, &mut sizes);
            let tallest = sizes.iter().map(|s| s.height).fold(0.0, f32::max);
            assert!(tallest < 40.0, "at {width}px a label wrapped into a column: {tallest}px tall");
            assert!(node.size().width <= width + 0.5, "at {width}px the toolbar overflows: {}", node.size().width);
        }
    }

    #[test]
    fn many_machines_wrap_inside_their_control_instead_of_squeezing() {
        let machines = ["All", "Local", "mac-studio", "build-box-eu", "gpu-rig-02", "office-mini", "laptop-old"];
        for width in [420.0, 700.0] {
            let node = layout_toolbar_with(width, &machines);
            let mut sizes = Vec::new();
            leaves(&node, &mut sizes);
            let tallest = sizes.iter().map(|s| s.height).fold(0.0, f32::max);
            assert!(tallest < 40.0, "at {width}px a machine label wrapped into a column: {tallest}px tall");
        }
    }

    #[test]
    fn a_wide_toolbar_stays_on_one_line() {
        let node = layout_toolbar(1600.0);
        assert!(node.size().height < 50.0, "{}", node.size().height);
    }

    #[test]
    fn island_meta_formats_active_over_total_only_when_they_differ() {
        assert_eq!(island_meta(&IslandSummary { working: 3, ..Default::default() }), "3");
        assert_eq!(island_meta(&IslandSummary { working: 2, done: 1, ..Default::default() }), "2 / 3");
        assert_eq!(island_meta(&IslandSummary::default()), "");
    }

    fn session_with(status: SessionStatus, activity: ActivityState) -> Session {
        Session {
            id: "s".into(), orchestrator_id: Some("o".into()), name: "s-name".into(),
            repo: "o/r".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None,
            summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity, activity_note: None, activity_since: None, machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    /// A worker that stopped (tmux pane gone) can still carry a stale
    /// `activity: Blocked` from right before it died — `classify_member`
    /// already gives `Stopped` precedence over that, and the chip must
    /// agree rather than showing "needs you" next to a stopped/done dot.
    #[test]
    fn member_chip_is_none_for_a_stopped_member_even_with_stale_blocked_activity() {
        let session = session_with(SessionStatus::Terminated, ActivityState::Blocked);
        assert_eq!(member_chip(MemberStatus::Stopped, &session, None), None);
        assert_eq!(member_chip(MemberStatus::Done, &session, None), None);
    }

    #[test]
    fn member_chip_drops_the_plain_pr_text_when_pr_chips_are_drawn() {
        use ninox_core::session_prs::{PrState, SessionPr};
        let pr = |n, state| SessionPr {
            repo: "o/bellhop".into(), number: n, url: String::new(), title: String::new(), state,
            draft: false, ci: ninox_core::pr_events::CiVerdict::None, passing: 0, failing: 0, pending: 0,
            changes_requested: false, approved: false, tracked: false,
        };
        let session = session_with(SessionStatus::PrOpen, ActivityState::Idle);
        let prs = [pr(1466, PrState::Open), pr(1473, PrState::Open), pr(900, PrState::Merged)];
        assert_eq!(member_chip_with_prs(MemberStatus::WaitingOnPr, &session, None, &prs), None);
        let (label, ..) = member_chip_with_prs(MemberStatus::WaitingOnPr, &session, None, &[]).unwrap();
        assert_eq!(label, "PR open");
    }

    #[test]
    fn member_chip_reports_needs_you_for_a_live_blocked_member() {
        let session = session_with(SessionStatus::Working, ActivityState::Blocked);
        let (label, ..) = member_chip(MemberStatus::NeedsYou, &session, None).expect("blocked member needs a chip");
        assert_eq!(label, "needs you");
    }
}
