//! The Fleet tab's opened-island detail panel (300px, right) + bottom
//! context-action row (62px) — MLOPS-5339 slice 4, `FleetFocused.dc.html`
//! lines 175-203 / `GUI-Inspect.dc.html`'s PR-review content, restyled to
//! v1 dark tokens. Neither renders unless something is selected inside an
//! opened island (`context` returns `None` otherwise) — brief's "Must not:
//! open the panel with nothing selected".
//!
//! The PR section reuses the same data (`app.prs`/`app.ci_status`/
//! `app.review_threads`/`app.diffs`) and actions (`Message::OpenUrl`,
//! `Message::DeleteAsk`, `Message::NavigateSession`) the old (pre-v1)
//! `info_panel`/`session_detail::diff_panel` already use — this is a new
//! rendering only, not a second review-data pipeline. Marginalia comment
//! bodies render as plain text rather than through `selectable_markdown`
//! (which is wired to the old `ColorScheme` theme, not the v1 token set) —
//! a deliberate simplification: copy/selection is a nice-to-have, not
//! "review logic".

use iced::{
    widget::{button, column, container, row, rich_text, scrollable, span, text, text_input, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use ninox_core::types::{Comment, Session};

use ninox_overview::display::display_brief;
use ninox_overview::edge::{EdgeKind, OverviewEdge};
use ninox_overview::islands::{Island, IslandMember};
use ninox_overview::nav::FleetNav;
use ninox_overview::status::MemberStatus;

use super::markdown_blocks;
use super::shell::tokens;
use crate::app::{App, Message, View};

pub const PANEL_WIDTH: f32 = 300.0;
pub const ACTION_ROW_HEIGHT: f32 = 62.0;

const WORKING_BADGE_BG: Color = Color { r: 0x10 as f32 / 255.0, g: 0x33 as f32 / 255.0, b: 0x31 as f32 / 255.0, a: 1.0 };
const DONE_BADGE_BG:    Color = Color { r: 0x15 as f32 / 255.0, g: 0x29 as f32 / 255.0, b: 0x1f as f32 / 255.0, a: 1.0 };
const DONE_BADGE_FG:    Color = Color { r: 0x7f as f32 / 255.0, g: 0xd8 as f32 / 255.0, b: 0xa8 as f32 / 255.0, a: 1.0 };
const WAITING_TEXT:     Color = Color { r: 0xf3 as f32 / 255.0, g: 0xc2 as f32 / 255.0, b: 0x7a as f32 / 255.0, a: 1.0 };

/// Whether the Fleet tab's detail panel/action row should render this
/// frame, and for which node — `None` whenever nothing is selected (no
/// island open, nothing selected inside it, or the selection is stale and
/// no longer a member of the open island — islands rebuild every frame
/// from live session state, and a selection that's fallen out of them would
/// otherwise render a misleading "Working" badge and a depth computed
/// against edges that don't contain it; code-review finding).
pub fn context<'a>(
    view: &View,
    fleet_nav: &'a FleetNav,
    islands: Option<&'a [Island]>,
) -> Option<(&'a Island, &'a str)> {
    if !matches!(view, View::Fleet) {
        return None;
    }
    let islands = islands?;
    let key = fleet_nav.open.as_ref()?;
    let island = islands.iter().find(|i| &i.key == key)?;
    let selected = fleet_nav.selected.as_deref()?;
    if !island.members.iter().any(|m| m.id == selected) {
        return None;
    }
    Some((island, selected))
}

/// The action row's target for a top-level card selected on the Fleet board
/// with no island open: that card's root orchestrator. `None` once an island
/// is open (then `context` governs), for an Unparented card, or when the
/// selected card has no root.
pub fn board_context<'a>(
    view: &View,
    fleet_nav: &'a FleetNav,
    islands: Option<&'a [Island]>,
) -> Option<(&'a Island, &'a str)> {
    if !matches!(view, View::Fleet) || fleet_nav.open.is_some() {
        return None;
    }
    let key = fleet_nav.selected_island.as_ref()?;
    let island = islands?.iter().find(|i| &i.key == key)?;
    Some((island, island.root.as_deref()?))
}

/// Hops from `root` up to `id` along hierarchy edges — "depth n of m" in the
/// panel's breadcrumb. Stops (rather than looping) if `id` isn't actually a
/// descendant of `root` reachable via `edges` — which `context`'s own
/// island-membership check (this module's doc comment) rules out for
/// `selected`, but `root` itself is only ever this function's exact
/// starting point, so that case still returns 0 indistinguishably from a
/// genuine root; a known, narrow limitation rather than a `root` this
/// island would ever actually pass in. Bounded to at most `edges.len()`
/// hops — one hierarchy edge can supply at most one hop, so no reachable
/// chain is ever longer than that.
fn depth_of(edges: &[OverviewEdge], root: &str, id: &str) -> usize {
    let mut current = id;
    let mut depth = 0;
    while current != root && depth < edges.len() {
        match edges.iter().find(|e| e.kind() == EdgeKind::Hierarchy && e.to() == current) {
            Some(edge) => {
                current = edge.from();
                depth += 1;
            }
            None => break,
        }
    }
    depth
}

fn status_badge_parts(status: MemberStatus) -> (&'static str, Color, Color) {
    match status {
        MemberStatus::NeedsYou   => ("NEEDS YOU", tokens::AMBER_BADGE_BG, tokens::AMBER_BADGE_FG),
        MemberStatus::Working    => ("WORKING", WORKING_BADGE_BG, tokens::TEAL),
        MemberStatus::WaitingOnPr => ("WAITING ON PR", tokens::PANEL2, tokens::DIM),
        MemberStatus::Done       => ("DONE", DONE_BADGE_BG, DONE_BADGE_FG),
        MemberStatus::Stopped    => ("STOPPED", tokens::PANEL2, tokens::DIM),
        MemberStatus::Queued     => ("QUEUED", tokens::PANEL2, tokens::DIM),
    }
}

/// Reuses `fleet_focus::badge` (the same `.badge`-style pill `FleetFocused.
/// dc.html`'s type/harness badges use — square-ish, 4px radius) rather than
/// a second copy of the same container-building code (code-review finding).
fn status_badge<'a>(status: MemberStatus) -> Element<'a, Message> {
    let (label, bg, fg) = status_badge_parts(status);
    super::fleet_focus::badge(label.to_string(), bg, fg)
}

pub(crate) fn field_label<'a>(label: &'static str) -> Element<'a, Message> {
    text(label.to_uppercase()).size(9.5).font(crate::style::SANS).color(tokens::DIM2).into()
}

pub(crate) fn field_box<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    container(content)
        .width(Length::Fill)
        .padding([8, 10])
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
}

fn field<'a>(label: &'static str, value: String) -> Vec<Element<'a, Message>> {
    vec![
        field_label(label),
        field_box(text(value).size(11.5).font(crate::style::SANS).color(tokens::DIM).into()),
    ]
}

/// `Artifacts` field content: branch (derived from the workspace path's own
/// last path segment — this codebase's worktrees always live at
/// `.claude/worktrees/<branch>`, and `Session` carries no separate branch
/// field of its own) and PR state, joined with " · " when more than one
/// part is known.
fn artifacts_line(session: &Session, pr_url: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(branch) = super::session_detail::branch_from_workspace(session) {
        parts.push(format!("branch {branch}"));
    }
    match (session.pr_number, pr_url) {
        (Some(n), _) => parts.push(format!("PR #{n}")),
        (None, _) => parts.push("no PR yet".to_string()),
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

fn worker_fields<'a>(app: &'a App, session: &'a Session) -> Vec<Element<'a, Message>> {
    let mut items = Vec::new();

    if session.activity == ninox_core::types::ActivityState::Blocked {
        if let Some(waiting) = ninox_overview::waiting_on::waiting_on_text(session.activity_note.as_deref()) {
            items.push(field_label("Waiting on"));
            items.push(field_box(text(waiting).size(11.5).font(crate::style::SANS).color(WAITING_TEXT).into()));
        }
    }

    if let Some(brief) = session.summary.as_deref().filter(|b| !b.trim().is_empty()) {
        let brief = display_brief(brief);
        if !brief.is_empty() {
            items.extend(field("Brief", brief));
        }
    }

    items.extend(field(
        "Worker type · harness · model",
        format!(
            "{} · {} · {}",
            session.worker_type.as_deref().unwrap_or("Worker"),
            session.agent_type,
            session.model_label().as_deref().unwrap_or("—"),
        ),
    ));

    if !session.success_criteria.is_empty() {
        items.push(field_label("Definition of done"));
        let lines: Vec<Element<Message>> = ninox_overview::completion::definition_of_done(session).into_iter()
            .map(|(done, label)| {
                let mark = if done { "✓" } else { "○" };
                text(format!("{mark} {label}")).size(11.5).font(crate::style::SANS).color(tokens::DIM).into()
            })
            .collect();
        items.push(field_box(column(lines).spacing(4).into()));
    }

    if let Some(outcome) = session.completion_outcome.as_ref() {
        items.push(field_label("Outcome"));
        // First line is always the summary (never a link); the rest are
        // evidence entries, rendered as a link when they parse as a URL.
        let lines: Vec<Element<Message>> = ninox_overview::completion::outcome_lines(outcome).into_iter().enumerate()
            .map(|(i, line)| {
                let is_evidence_url = i > 0 && (line.starts_with("http://") || line.starts_with("https://"));
                if is_evidence_url {
                    rich_text![
                        span(line.clone())
                            .color(tokens::VIOLET_BADGE_FG)
                            .underline(true)
                            .link(Message::OpenUrl(line))
                    ]
                    .size(11)
                    .into()
                } else {
                    text(line).size(11.5).font(crate::style::SANS).color(tokens::DIM).into()
                }
            })
            .collect();
        items.push(field_box(column(lines).spacing(4).into()));
    }

    let pr_url = crate::app::pr_url_for_session(&app.prs, session);
    items.extend(field("Artifacts", artifacts_line(session, pr_url.as_deref())));

    items
}

/// Orchestrator selected: counts + a needs-you list, instead of worker
/// fields (brief item 1). Reuses the whole opened island's own
/// `IslandSummary`/members rather than re-deriving a sub-island scoped to
/// just this orchestrator's descendants — correct for the common case
/// (the island's own root orchestrator), and a reasonable approximation for
/// a nested sub-orchestrator (out of scope: island-model changes).
fn orchestrator_fields<'a>(island: &Island, app: &'a App) -> Vec<Element<'a, Message>> {
    let summary = &island.summary;
    let mut items = vec![
        field_label("Summary"),
        field_box(
            text(format!(
                "{} active · {} done · {} stopped",
                summary.active(), summary.done, summary.stopped,
            ))
            .size(11.5)
            .font(crate::style::SANS)
            .color(tokens::DIM)
            .into(),
        ),
    ];

    let needs_you: Vec<&IslandMember> = island.members.iter()
        .filter(|m| m.status == MemberStatus::NeedsYou)
        .collect();
    if !needs_you.is_empty() {
        items.push(Space::new(0, 6).into());
        items.push(field_label("Needs you"));
        for member in needs_you {
            let name = app.sessions.get(&member.id).map(|s| s.name.as_str()).unwrap_or(&member.id);
            items.push(
                button(text(name.to_string()).size(11.5).font(crate::style::SANS).color(tokens::AMBER_BADGE_FG))
                    .on_press(Message::FleetSelectNode(member.id.clone()))
                    .padding(0)
                    .style(|_t, _status| button::Style { background: None, ..Default::default() })
                    .into(),
            );
        }
    }

    items
}

pub(crate) fn ci_summary_color(ci: &ninox_core::types::CIStatus) -> Color {
    if ci.failing > 0 { tokens::RED } else if ci.pending > 0 { tokens::AMBER } else { tokens::GREEN }
}

const MAX_DIFF_LINES: usize = 300;

pub(crate) fn diff_line_color(line: &str) -> Color {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff --git") || line.starts_with("index ") {
        tokens::DIM2
    } else if line.starts_with('+') {
        tokens::GREEN
    } else if line.starts_with('-') {
        tokens::RED
    } else if line.starts_with("@@") {
        tokens::VIOLET
    } else {
        tokens::DIM
    }
}

fn diff_section<'a>(app: &'a App, session_id: &str) -> Element<'a, Message> {
    let body: Element<Message> = match app.diffs.get(session_id) {
        None => text("Loading diff…").size(10.5).font(crate::style::MONO).color(tokens::DIM2).into(),
        Some(None) => text("No changes yet").size(10.5).font(crate::style::MONO).color(tokens::DIM2).into(),
        Some(Some(diff)) => {
            let all_lines: Vec<&str> = diff.lines().collect();
            let shown = all_lines.len().min(MAX_DIFF_LINES);
            let mut rows: Vec<Element<Message>> = all_lines[..shown].iter().map(|line| {
                text(line.to_string()).size(10).font(crate::style::MONO).color(diff_line_color(line)).into()
            }).collect();
            if all_lines.len() > MAX_DIFF_LINES {
                rows.push(text(format!("… {} more lines truncated", all_lines.len() - MAX_DIFF_LINES))
                    .size(9.5).font(crate::style::MONO).color(tokens::DIM2).into());
            }
            scrollable(column(rows)).height(Length::Fixed(160.0)).into()
        }
    };
    container(body)
        .width(Length::Fill)
        .padding(10)
        .style(|_| container::Style { background: Some(Background::Color(tokens::BG)), border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() }, ..Default::default() })
        .into()
}

fn markdown_block_style(kind: markdown_blocks::BlockKind) -> (f32, iced::Font, Color) {
    use markdown_blocks::BlockKind;
    match kind {
        BlockKind::Heading(1) => (13.0, crate::style::SANS_BOLD, tokens::TEXT),
        BlockKind::Heading(2) => (12.0, crate::style::SANS_BOLD, tokens::TEXT),
        BlockKind::Heading(_) => (11.0, crate::style::SANS_BOLD, tokens::TEXT),
        BlockKind::Code => (10.0, crate::style::MONO, tokens::DIM),
        BlockKind::Paragraph | BlockKind::ListItem { .. } => (10.5, crate::style::SANS, tokens::DIM),
    }
}

fn markdown_block_prefix(kind: markdown_blocks::BlockKind) -> String {
    use markdown_blocks::BlockKind;
    match kind {
        BlockKind::ListItem { checked: Some(true), .. } => "☑ ".into(),
        BlockKind::ListItem { checked: Some(false), .. } => "☐ ".into(),
        BlockKind::ListItem { ordinal: Some(n), .. } => format!("{n}. "),
        BlockKind::ListItem { .. } => "• ".into(),
        _ => String::new(),
    }
}

/// `Message::OpenUrl` shells out to `open <url>` (or the platform
/// equivalent) with no scheme check of its own — safe for links this app
/// itself generates, but a Marginalia comment's links come from whoever
/// wrote the comment (any GitHub contributor on this public repo, or a
/// bot), so a `file://`/`smb://`/custom-scheme link must never become a
/// clickable chip from this untrusted content (code-review finding).
fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn markdown_link_chip<'a>(label: &str, url: &str) -> Element<'a, Message> {
    button(
        row![
            text("↗").font(crate::style::GLYPH).size(10).color(tokens::VIOLET_BADGE_FG),
            text(format!(" {label}")).size(10).font(crate::style::SANS).color(tokens::VIOLET_BADGE_FG),
        ],
    )
    .on_press(Message::OpenUrl(url.to_string()))
    .padding(0)
    .style(|_t, _status| button::Style { background: None, ..Default::default() })
    .into()
}

/// One rendered markdown block — block-level styling only (no inline
/// bold/italic spans): a deliberate simplification matching this file's own
/// documented trade-off for Marginalia cards (copy/selection and full
/// inline fidelity are nice-to-haves, not "review logic"). Links surface as
/// clickable chips beneath the block, same convention as
/// `plan_panel::block_widget` (a `text_editor` — and this plain `text` even
/// more so — has no click-through-link mechanism).
fn markdown_block_element<'a>(block: &markdown_blocks::Block) -> Element<'a, Message> {
    let (size, font, color) = markdown_block_style(block.kind);
    let prefix = markdown_block_prefix(block.kind);
    let body: Element<Message> = if crate::style::glyph_font_for(&prefix) == crate::style::GLYPH_ALT {
        text(format!("{prefix}{}", block.text)).size(size).font(font).color(color).into()
    } else {
        // ☐/☑ aren't in Inter: give them their own glyph-font span (rich
        // text always uses Advanced shaping, so the fallback applies).
        iced::widget::rich_text![
            iced::widget::span(prefix.clone()).font(crate::style::glyph_font_for(&prefix)),
            iced::widget::span(block.text.clone()).font(font),
        ]
        .size(size)
        .color(color)
        .into()
    };
    let chips: Vec<Element<Message>> = block.links.iter()
        .filter(|(_, url)| is_web_url(url))
        .map(|(label, url)| markdown_link_chip(label, url))
        .collect();
    if chips.is_empty() {
        body
    } else {
        column![body, row(chips).spacing(8).wrap()].spacing(2).into()
    }
}

/// Renders `markdown` (a Marginalia/PR comment body) as markdown — headings,
/// lists, code, links — using the v1 dark tokens, after dropping HTML
/// comments and collapsing any `<details>` block to its summary (brief item
/// 1). Replaces the old `text(comment.body.clone())` verbatim dump, which
/// showed literal `<!-- linear-linkback -->`/`<details>`/`## Why` markup.
fn markdown_body<'a>(markdown: &str) -> Element<'a, Message> {
    let cleaned = ninox_overview::comment_markdown::clean_comment_markdown(markdown);
    let blocks = markdown_blocks::parse_blocks(&cleaned);
    column(blocks.iter().map(markdown_block_element).collect::<Vec<Element<Message>>>())
        .spacing(5)
        .into()
}

fn marginalia_comment<'a>(comment: &'a Comment) -> Element<'a, Message> {
    let location = match (&comment.path, comment.line) {
        (Some(path), Some(line)) => format!("{path}:{line}"),
        (Some(path), None) => path.clone(),
        _ => String::new(),
    };
    container(
        column![
            row![
                text(comment.author.clone()).size(10.5).font(crate::style::SANS).color(tokens::VIOLET_BADGE_FG),
                if location.is_empty() { Space::new(0, 0).into() } else {
                    Element::from(row![Space::new(6, 0), text(location).size(9.5).font(crate::style::MONO).color(tokens::DIM2)])
                },
            ].align_y(Alignment::Center),
            Space::new(0, 4),
            markdown_body(&comment.body),
        ],
    )
    .width(Length::Fill)
    .padding(10)
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL2)),
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .into()
}

/// Every PR the session is responsible for (open first), then — for the
/// tracked PR — a compact diff and Marginalia comments.
fn pr_section<'a>(app: &'a App, session: &'a Session) -> Vec<Element<'a, Message>> {
    let tracked = session.pr_id.and_then(|id| app.prs.get(&id));
    let list = app.session_prs.get(&session.id).map(Vec::as_slice).filter(|l| !l.is_empty());
    if tracked.is_none() && list.is_none() {
        return Vec::new();
    }

    let mut items: Vec<Element<Message>> = vec![
        Space::new(0, 10).into(),
        text(super::pr_list::heading(list.map_or(1, <[_]>::len)))
            .size(9.5).font(crate::style::SANS).color(tokens::DIM2).into(),
        Space::new(0, 4).into(),
    ];
    match (list, tracked) {
        (Some(list), _) => items.extend(super::pr_list::rows(list, tokens::VIOLET_BADGE_FG, 12.0, 10.5)),
        (None, Some(pr)) => {
            items.push(
                rich_text![
                    span(format!("#{} {}", pr.number, pr.title))
                        .color(tokens::VIOLET_BADGE_FG)
                        .underline(true)
                        .link(Message::OpenUrl(pr.url.clone()))
                ]
                .size(12)
                .into(),
            );
            if let Some(ci) = app.ci_status.get(&pr.id) {
                items.push(Space::new(0, 6).into());
                items.push(
                    text(format!("{}/{} checks passing", ci.passing, ci.total))
                        .size(10.5)
                        .font(crate::style::SANS)
                        .color(ci_summary_color(ci))
                        .into(),
                );
            }
        }
        (None, None) => {}
    }
    let Some(pr) = tracked else {
        return items;
    };

    items.push(Space::new(0, 8).into());
    items.push(diff_section(app, &session.id));

    let all_comments = app.review_threads.get(&pr.id).map(Vec::as_slice).unwrap_or_default();
    let comments = ninox_overview::comments::visible_comments(all_comments);
    if !comments.is_empty() {
        items.push(Space::new(0, 10).into());
        items.push(text(format!("MARGINALIA · {}", comments.len())).size(9.5).font(crate::style::SANS).color(tokens::DIM2).into());
        for comment in comments {
            items.push(Space::new(0, 6).into());
            items.push(marginalia_comment(comment));
        }
    }

    items
}

fn breadcrumb_meta(island: &Island, edges: &[OverviewEdge], max_depth: usize, selected: &str, name: &str) -> String {
    let depth = island.root.as_deref().map(|root| depth_of(edges, root, selected)).unwrap_or(0);
    format!("{} › {} (depth {} of {})", island.name, name, depth, max_depth.max(depth))
}

/// The 300px detail panel. `edges` is the opened island's own hierarchy
/// edges (`fleet_focus::nodes_for_island`'s second element) — threaded in
/// rather than recomputed here so there is exactly one place that builds
/// them.
pub fn detail_panel<'a>(app: &'a App, island: &Island, edges: &[OverviewEdge], selected: &str) -> Element<'a, Message> {
    let is_orchestrator = app.orchestrators.iter().any(|o| o.id == selected);
    let status = island.members.iter().find(|m| m.id == selected).map(|m| m.status).unwrap_or(MemberStatus::Working);
    let name = app.sessions.get(selected).map(|s| s.name.as_str()).unwrap_or(selected);
    let max_depth = app.config.nested_orchestration.max_depth as usize;

    let mut items: Vec<Element<Message>> = vec![
        text("DETAIL — SELECTED NODE").size(9.5).font(crate::style::SANS).color(tokens::DIM2).into(),
        Space::new(0, 10).into(),
        status_badge(status),
        Space::new(0, 8).into(),
        text(name.to_string()).size(15).font(crate::style::SANS).color(tokens::TEXT).into(),
        text(breadcrumb_meta(island, edges, max_depth, selected, name)).size(9.5).font(crate::style::SANS).color(tokens::DIM2).into(),
        Space::new(0, 10).into(),
    ];

    if is_orchestrator {
        items.extend(orchestrator_fields(island, app));
        if let Some(session) = app.sessions.get(selected) {
            items.push(Space::new(0, 6).into());
            items.push(field_label("Brains"));
            items.push(field_box(
                text(ninox_core::brain_set::session_brains_line(session, &app.config))
                    .size(11.5).font(crate::style::SANS).color(tokens::DIM).into(),
            ));
        }
    } else if let Some(session) = app.sessions.get(selected) {
        items.extend(worker_fields(app, session));
        items.extend(pr_section(app, session));
    }

    container(
        scrollable(container(column(items).spacing(4)).width(Length::Fill).padding(16))
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .width(Length::Fixed(PANEL_WIDTH))
    .height(Length::Fill)
    .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL)), ..Default::default() })
    .into()
}

/// A 1px vertical divider between the centre column and the detail panel —
/// `iced::Border` strokes are uniform on all four sides, so a single-side
/// "border-left" is drawn as its own thin filler element instead (same
/// convention as `fleet_islands::tick`/the tree rail's divider).
pub fn divider<'a>() -> Element<'a, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(1.0))
        .height(Length::Fill)
        .style(|_| container::Style { background: Some(Background::Color(tokens::BORDER)), ..Default::default() })
        .into()
}

fn action_btn<'a>(label: &'static str, msg: Message, primary: bool, danger: bool) -> Element<'a, Message> {
    let text_color = if danger { tokens::RED } else { tokens::TEXT };
    button(text(label).size(12).font(crate::style::SANS).color(text_color))
        .on_press(msg)
        .padding([7, 14])
        .style(move |_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            let bg = if primary {
                Color { r: 0x1f as f32 / 255.0, g: 0x3a as f32 / 255.0, b: 0x6e as f32 / 255.0, a: 1.0 }
            } else if hovered {
                tokens::PANEL2
            } else {
                tokens::PANEL
            };
            button::Style {
                background: Some(Background::Color(bg)),
                border: Border { color: if danger { Color { r: 0x5a as f32/255.0, g: 0x2c as f32/255.0, b: 0x38 as f32/255.0, a: 1.0 } } else { tokens::BORDER }, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }
        })
        .into()
}

fn composer<'a>(app: &'a App, target: &str) -> Element<'a, Message> {
    let draft = app.fleet_message_draft.as_ref()
        .filter(|(id, _)| id == target)
        .map(|(_, text)| text.as_str())
        .unwrap_or("");
    row![
        text_input("message…", draft)
            .on_input(Message::FleetSendMessageChanged)
            .on_submit(Message::FleetSendMessageSubmit)
            .size(12.5)
            .width(Length::Fill)
            .style(|_t, _status| text_input::Style {
                background: Background::Color(tokens::PANEL2),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
                icon: tokens::DIM,
                placeholder: tokens::DIM2,
                value: tokens::TEXT,
                selection: tokens::VIOLET,
            }),
        action_btn("Send", Message::FleetSendMessageSubmit, true, false),
        action_btn("Cancel", Message::FleetSendMessageCancel, false, false),
    ]
    .align_y(Alignment::Center)
    .spacing(10)
    .into()
}

/// The 62px bottom action row — context-sensitive to the current
/// selection, hidden entirely with no selection (brief item 4).
pub fn action_row<'a>(app: &'a App, island: &Island, selected: &str) -> Element<'a, Message> {
    if app.fleet_message_draft.as_ref().is_some_and(|(id, _)| id == selected) {
        return container(composer(app, selected))
            .width(Length::Fill)
            .height(Length::Fixed(ACTION_ROW_HEIGHT))
            .padding(Padding { top: 0.0, right: 20.0, bottom: 0.0, left: 20.0 })
            .align_y(Alignment::Center)
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
                ..Default::default()
            })
            .into();
    }

    let is_orchestrator = app.orchestrators.iter().any(|o| o.id == selected);
    let status = island.members.iter().find(|m| m.id == selected).map(|m| m.status).unwrap_or(MemberStatus::Working);
    let name = app.sessions.get(selected).map(|s| s.name.as_str()).unwrap_or(selected);

    let mut buttons: Vec<Element<Message>> = Vec::new();
    if is_orchestrator {
        buttons.push(action_btn("Message orchestrator", Message::FleetSendMessageOpen(selected.to_string()), false, false));
        buttons.push(action_btn("Connect", Message::NavigateSession(selected.to_string()), false, false));
        buttons.push(action_btn("Delete…", Message::DeleteAsk(selected.to_string()), false, true));
    } else {
        // A needs-you worker gets both: "Answer in pane" primary (matching
        // `FleetFocused.dc.html`'s mock, which shows both side by side) and
        // the plain "Connect" every other worker gets — both wire to the
        // same `NavigateSession` attach path; there is no second distinct
        // mechanism to give the primary button.
        if status == MemberStatus::NeedsYou {
            buttons.push(action_btn("Answer in pane", Message::NavigateSession(selected.to_string()), true, false));
        }
        buttons.push(action_btn("Connect", Message::NavigateSession(selected.to_string()), false, false));
        buttons.push(action_btn("Send message", Message::FleetSendMessageOpen(selected.to_string()), false, false));
        if let Some(url) = app.sessions.get(selected).and_then(|s| crate::app::pr_url_for_session(&app.prs, s)) {
            buttons.push(action_btn("Open PR", Message::OpenUrl(url), false, false));
        }
        buttons.push(action_btn("Delete…", Message::DeleteAsk(selected.to_string()), false, true));
    }

    let label = if is_orchestrator { "Fleet actions" } else { "Worker actions" };

    let mut row_items = buttons;
    row_items.push(Space::new(Length::Fill, 0).into());
    row_items.push(text(format!("{label} · selection: {name}")).size(10.5).font(crate::style::SANS).color(tokens::DIM2).into());

    container(row(row_items).align_y(Alignment::Center).spacing(10))
        .width(Length::Fill)
        .height(Length::Fixed(ACTION_ROW_HEIGHT))
        .padding(Padding { top: 0.0, right: 20.0, bottom: 0.0, left: 20.0 })
        .align_y(Alignment::Center)
        .style(|_| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn checkbox_prefixes_route_through_the_glyph_font() {
        use markdown_blocks::BlockKind;
        for checked in [true, false] {
            let prefix = markdown_block_prefix(BlockKind::ListItem { ordinal: None, checked: Some(checked) });
            assert_eq!(crate::style::glyph_font_for(&prefix), crate::style::GLYPH, "{prefix:?}");
        }
        let bullet = markdown_block_prefix(BlockKind::ListItem { ordinal: None, checked: None });
        assert_eq!(crate::style::glyph_font_for(&bullet), crate::style::GLYPH_ALT);
    }

    use super::*;

    fn island(key: ninox_overview::islands::IslandKey, root: Option<&str>, members: Vec<(&str, MemberStatus)>) -> Island {
        Island {
            key, name: "x".into(), root: root.map(String::from),
            members: members.into_iter().map(|(id, status)| IslandMember { id: id.into(), status }).collect(),
            summary: ninox_overview::islands::IslandSummary::default(), machine: None, pending_request: false,
        }
    }

    #[test]
    fn context_is_none_off_the_fleet_tab() {
        let mut nav = FleetNav::default();
        nav.open_island(
            ninox_overview::islands::IslandKey::Root("root".into()),
            &[island(ninox_overview::islands::IslandKey::Root("root".into()), Some("root"), vec![("root", MemberStatus::Working)])],
        );
        assert_eq!(context(&View::Board { scope: None }, &nav, None), None);
    }

    #[test]
    fn context_is_none_with_nothing_selected() {
        let islands = vec![island(ninox_overview::islands::IslandKey::Root("root".into()), Some("root"), vec![("root", MemberStatus::Working)])];
        let mut nav = FleetNav::default();
        nav.open_island(ninox_overview::islands::IslandKey::Root("root".into()), &islands);
        nav.clear_selection();
        assert_eq!(context(&View::Fleet, &nav, Some(&islands)), None);
    }

    #[test]
    fn context_is_none_when_no_island_is_open() {
        let islands = vec![island(ninox_overview::islands::IslandKey::Root("root".into()), Some("root"), vec![("root", MemberStatus::Working)])];
        let nav = FleetNav::default();
        assert_eq!(context(&View::Fleet, &nav, Some(&islands)), None);
    }

    #[test]
    fn context_is_some_with_an_island_open_and_a_node_selected() {
        let islands = vec![island(ninox_overview::islands::IslandKey::Root("root".into()), Some("root"), vec![("root", MemberStatus::Working), ("leaf", MemberStatus::Working)])];
        let mut nav = FleetNav::default();
        nav.open_island(ninox_overview::islands::IslandKey::Root("root".into()), &islands);
        nav.select("leaf");
        let (island, selected) = context(&View::Fleet, &nav, Some(&islands)).expect("should resolve");
        assert_eq!(island.key, ninox_overview::islands::IslandKey::Root("root".into()));
        assert_eq!(selected, "leaf");
    }

    #[test]
    fn context_is_none_when_the_selection_is_no_longer_a_member_of_the_open_island() {
        // Islands rebuild every frame from live session state — a stale
        // selection that's fallen out of the open island's members must
        // hide the panel rather than render a misleading badge/depth for a
        // node this island doesn't actually have.
        let islands = vec![island(ninox_overview::islands::IslandKey::Root("root".into()), Some("root"), vec![("root", MemberStatus::Working)])];
        let mut nav = FleetNav::default();
        nav.open_island(ninox_overview::islands::IslandKey::Root("root".into()), &islands);
        nav.select("gone");
        assert_eq!(context(&View::Fleet, &nav, Some(&islands)), None);
    }

    #[test]
    fn breadcrumb_meta_shows_the_display_name_not_the_raw_session_id() {
        let island = island(ninox_overview::islands::IslandKey::Root("clean-up-approved-prs".into()), Some("clean-up-approved-prs"), vec![("clean-up-approved-prs", MemberStatus::Working)]);
        let meta = breadcrumb_meta(&island, &[], 3, "clean-up-approved-prs", "Clean up approved PRs");
        assert!(meta.contains("Clean up approved PRs"), "meta: {meta}");
        assert!(!meta.contains("clean-up-approved-prs"), "raw session id must not leak into the display label: {meta}");
    }

    #[test]
    fn depth_of_counts_hops_from_root() {
        let nodes = vec![];
        let edges = vec![
            OverviewEdge::new(EdgeKind::Hierarchy, "root".into(), "mid".into(), &nodes).unwrap(),
            OverviewEdge::new(EdgeKind::Hierarchy, "mid".into(), "leaf".into(), &nodes).unwrap(),
        ];
        assert_eq!(depth_of(&edges, "root", "root"), 0);
        assert_eq!(depth_of(&edges, "root", "mid"), 1);
        assert_eq!(depth_of(&edges, "root", "leaf"), 2);
    }

    #[test]
    fn depth_of_stops_rather_than_looping_for_an_unreachable_id() {
        let edges: Vec<OverviewEdge> = vec![];
        assert_eq!(depth_of(&edges, "root", "nowhere"), 0);
    }

    #[test]
    fn artifacts_line_shows_branch_from_the_workspace_path() {
        let session = Session {
            id: "s".into(), orchestrator_id: Some("o".into()), name: "s-name".into(), repo: "o/r".into(),
            status: ninox_core::types::SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some(".claude/worktrees/v1-mlops-5339-gui-detail".into()),
            pid: None, model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None, activity: ninox_core::types::ActivityState::Idle,
            activity_note: None, activity_since: None, machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        assert_eq!(artifacts_line(&session, None), "branch v1-mlops-5339-gui-detail · no PR yet");
    }

    #[test]
    fn artifacts_line_shows_the_pr_number_when_present() {
        let mut session = Session {
            id: "s".into(), orchestrator_id: Some("o".into()), name: "s-name".into(), repo: "o/r".into(),
            status: ninox_core::types::SessionStatus::PrOpen, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: Some(42), pr_id: None, workspace_path: None,
            pid: None, model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None, activity: ninox_core::types::ActivityState::Idle,
            activity_note: None, activity_since: None, machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        };
        assert_eq!(artifacts_line(&session, None), "PR #42");
        session.workspace_path = Some("/abs/path/my-branch".into());
        assert_eq!(artifacts_line(&session, None), "branch my-branch · PR #42");
    }

    #[test]
    fn diff_line_color_flags_additions_and_removals() {
        assert_eq!(diff_line_color("+added"), tokens::GREEN);
        assert_eq!(diff_line_color("-removed"), tokens::RED);
        assert_eq!(diff_line_color("+++ b/file"), tokens::DIM2);
        assert_eq!(diff_line_color("context line"), tokens::DIM);
    }

    #[test]
    fn pr_section_s_comment_source_is_filtered_through_visible_comments() {
        let comments = vec![
            Comment { id: 1, pr_id: 1, author: "octocat".into(), body: "lgtm".into(), path: None, line: None, created_at: 0 },
            Comment { id: 2, pr_id: 1, author: "linear[bot]".into(), body: "synced".into(), path: None, line: None, created_at: 0 },
        ];
        let visible = ninox_overview::comments::visible_comments(&comments);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].author, "octocat");
    }

    #[test]
    fn is_web_url_accepts_only_http_and_https() {
        assert!(is_web_url("https://linear.app/x"));
        assert!(is_web_url("http://example.com"));
        assert!(!is_web_url("file:///etc/passwd"));
        assert!(!is_web_url("smb://server/share"));
        assert!(!is_web_url("mailto:a@b.com"));
        assert!(!is_web_url("some-app-scheme://payload"));
    }

    #[test]
    fn status_badge_parts_covers_every_status() {
        for status in [
            MemberStatus::Working, MemberStatus::NeedsYou, MemberStatus::WaitingOnPr,
            MemberStatus::Done, MemberStatus::Stopped,
        ] {
            let (label, ..) = status_badge_parts(status);
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn board_context_resolves_a_selected_card_to_its_root() {
        use ninox_overview::islands::IslandKey;
        let key = IslandKey::Root("root".into());
        let islands = vec![island(key.clone(), Some("root"), vec![("root", MemberStatus::Working), ("w", MemberStatus::Working)])];
        let nav = FleetNav { selected_island: Some(key), ..Default::default() };
        let (isl, target) = board_context(&View::Fleet, &nav, Some(&islands)).expect("card selected");
        assert_eq!((isl.name.as_str(), target), ("x", "root"));
    }

    #[test]
    fn board_context_is_none_when_nothing_is_selected_an_island_is_open_or_off_fleet() {
        use ninox_overview::islands::IslandKey;
        let key = IslandKey::Root("root".into());
        let islands = vec![island(key.clone(), Some("root"), vec![("root", MemberStatus::Working)])];
        let mut nav = FleetNav::default();
        assert_eq!(board_context(&View::Fleet, &nav, Some(&islands)), None);
        nav.selected_island = Some(key.clone());
        assert_eq!(board_context(&View::Board { scope: None }, &nav, Some(&islands)), None);
        nav.open_island(key, &islands);
        assert_eq!(board_context(&View::Fleet, &nav, Some(&islands)), None);
    }

    #[test]
    fn board_context_is_none_for_a_card_without_a_root() {
        use ninox_overview::islands::IslandKey;
        let islands = vec![island(IslandKey::Unparented, None, vec![("w", MemberStatus::Working)])];
        let nav = FleetNav { selected_island: Some(IslandKey::Unparented), ..Default::default() };
        assert_eq!(board_context(&View::Fleet, &nav, Some(&islands)), None);
    }
}
