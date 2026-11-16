//! The v1 shell's top nav bar: brand, six tabs, search box, a primary
//! Spawn button, and a Settings icon button. Markup/styling follows
//! `FleetAtScale.dc.html` (text-only tabs) rather than the older
//! `Main.dc.html` (icon tabs, missing the Board tab) — see the shell PR's
//! plan doc for why. Notifications live in a bell in the right-hand group;
//! the Fleet tab's needs-you count is a passive pill inside the tab.

use iced::{
    widget::{button, column, container, row, text, Space},
    Alignment, Background, Border, Element, Length, Padding,
};

use super::tokens;
use crate::app::{App, Message, View};

pub(crate) const NAV_HEIGHT: f32 = 44.0;

/// `Main.dc.html`'s `.nav` left padding. On macOS the traffic lights sit
/// over the bar (titlebar-transparent, fullsize content — `main.rs`), so
/// the brand starts this far past where `titlebar::place_traffic_lights`
/// measured them ending.
const LEADING_PAD: f32 = 16.0;
/// Until the traffic lights have been measured (and in tests): where
/// AppKit's standard buttons end, plus [`LEADING_PAD`].
#[cfg(target_os = "macos")]
const UNMEASURED_INSET: f32 = 78.0;
#[cfg(not(target_os = "macos"))]
const UNMEASURED_INSET: f32 = LEADING_PAD;

pub(crate) fn leading_inset(app: &App) -> f32 {
    app.traffic_lights_end.map_or(UNMEASURED_INSET, |end| end + LEADING_PAD)
}

/// How much of the top bar fits in the window, widest first. The search
/// field gives way first, then the tab labels; `+ Spawn` and the settings
/// dot never do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavFit {
    Full,
    SearchIcon,
    TabNumbers,
}

/// Window widths below which the search field collapses to an icon and the
/// tab labels to their numbers — the bar's natural width at each step plus
/// a little slack. `the_top_bar_never_pushes_spawn_or_settings_out_of_the_window`
/// sweeps them; bump them if the bar gains content.
const SEARCH_ICON_BELOW: f32 = 1150.0;
const TAB_NUMBERS_BELOW: f32 = 970.0;

// The bell with a badge and its spacer is folded into both thresholds.

/// `chip`: the leftover-workspaces chip's label, and `badge`: the
/// pending-requests count, when each shows — they take their width out of
/// what the rest of the bar has.
pub(crate) fn nav_fit(window_width: f32, chip: Option<&str>, badge: Option<&str>) -> NavFit {
    let window_width = window_width
        - chip.map_or(0.0, chip_width_estimate)
        - badge.map_or(0.0, badge_width_estimate);
    if window_width >= SEARCH_ICON_BELOW {
        NavFit::Full
    } else if window_width >= TAB_NUMBERS_BELOW {
        NavFit::SearchIcon
    } else {
        NavFit::TabNumbers
    }
}

struct Tab {
    label: &'static str,
    key: &'static str,
    active: bool,
    count: Option<usize>,
    on_press: Message,
}

/// The same bucket the Fleet tab's own islands total (MLOPS-5339 slice 2)
/// sums as `Island::summary.needs_you` — every non-orchestrator session
/// `ninox_overview::status::classify_member` buckets as `NeedsYou` — so this
/// badge can't drift from the Fleet tab's own count. Deliberately NOT
/// `fleet_islands::build_fleet_islands`: `nav()` renders on every shell view
/// (not just the Fleet tab), and that path clones every session into a
/// `FleetSnapshot`, hits the store for machine health, and rebuilds the
/// whole island graph — real cost to pay on every keystroke/terminal frame
/// while looking at, say, Brain or a session detail. This stays the cheap,
/// zero-I/O `O(n)` scan the badge always was.
fn needs_you_count<'a>(
    sessions: impl Iterator<Item = &'a ninox_core::types::Session>,
    orchestrator_ids: &std::collections::HashSet<&str>,
) -> usize {
    sessions
        .filter(|s| !orchestrator_ids.contains(s.id.as_str()))
        .filter(|s| !ninox_core::worker_type::is_prompt_agent_session(s))
        .filter(|s| {
            ninox_overview::status::classify_member(s, false)
                == ninox_overview::status::MemberStatus::NeedsYou
        })
        .count()
}

pub fn nav(app: &App) -> Element<'_, Message> {
    let orchestrator_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let needs_you = needs_you_count(app.sessions.values(), &orchestrator_ids);

    let tabs = [
        Tab {
            label: "Fleet", key: "1",
            active: matches!(app.view, View::Fleet),
            count: (needs_you > 0).then_some(needs_you),
            on_press: Message::NavigateFleet,
        },
        Tab {
            label: "Board", key: "2",
            active: matches!(app.view, View::Board { .. }),
            count: None,
            on_press: Message::NavigateBoard { scope: None },
        },
        Tab {
            label: "Plans", key: "3",
            active: matches!(app.view, View::Plans),
            count: None,
            on_press: Message::NavigatePlans,
        },
        Tab {
            label: "Brain", key: "4",
            active: matches!(app.view, View::Brain),
            count: None,
            on_press: Message::NavigateBrain,
        },
        Tab {
            // U+00A0 (non-breaking space), not a plain space: this is the
            // only two-word tab label, and `Wrapping::None` alone did not
            // stop it from breaking at the regular space between "Worker"
            // and "Types" (round-2 review feedback, item 1) — a
            // non-breaking space removes the break opportunity outright,
            // regardless of wrap-mode nuances.
            label: "Worker\u{a0}Types", key: "5",
            active: matches!(app.view, View::WorkerTypes),
            count: None,
            on_press: Message::NavigateWorkerTypes,
        },
        Tab {
            label: "Remotes", key: "6",
            active: matches!(app.view, View::Remotes),
            count: None,
            on_press: Message::NavigateRemotes,
        },
    ];

    let numbers_only = numbers_only(app);
    // `.nav-brand`: 8px gap, 14px before a full-height 1px rule, 6px after.
    // The word goes when the tabs collapse to numbers: the owl alone still
    // brands the bar, and the digits get the room.
    let mut brand = row![super::owl_mark::owl_mark()].spacing(8).align_y(Alignment::Center);
    if !numbers_only {
        brand = brand.push(text("ninox").size(14).font(crate::style::SANS_BOLD).color(tokens::TEXT));
    }
    let mut bar: Vec<Element<Message>> = vec![
        container(brand)
        .padding(Padding { top: 0.0, right: 14.0, bottom: 0.0, left: 0.0 })
        .into(),
        container(Space::new(1, 0))
            .width(Length::Fixed(1.0))
            .height(Length::Fill)
            .style(|_| container::Style { background: Some(Background::Color(tokens::BORDER)), ..Default::default() })
            .into(),
        Space::new(6, 0).into(),
    ];

    let chip_label = workspaces_chip_label(app.workspaces_summary);
    let badge = crate::app::pending_requests_badge(app.pending_user_requests.len());
    let fit = nav_fit(app.window_width, chip_label.as_deref(), badge.as_ref().map(|(count, _)| count.as_str()));
    let search_open = search_shown(app, fit);
    for tab in tabs {
        bar.push(nav_link(tab, numbers_only));
    }

    if search_open {
        bar.push(Space::new(10, 0).into());
        bar.push(crate::components::global_search::field(app, Length::Fill));
    } else {
        bar.push(Space::new(Length::Fill, 0).into());
        bar.push(match fit {
            NavFit::Full => crate::components::global_search::field(app, Length::Fixed(240.0)),
            _ => search_icon_button(numbers_only),
        });
    }
    bar.push(Space::new(10, 0).into());
    bar.push(
        button(text("+ Spawn").size(11.5).color(tokens::BG))
            .on_press(Message::SpawnSession)
            .padding([7, 12])
            .style(|_t, status| button::Style {
                background: Some(Background::Color(
                    if matches!(status, button::Status::Hovered) { tokens::TEXT } else { tokens::TEAL },
                )),
                text_color: tokens::BG,
                border: Border { radius: 6.0.into(), ..Default::default() },
                ..Default::default()
            })
            .into(),
    );
    if let Some(label) = chip_label {
        // Just the count once the tab labels have collapsed; the modal it
        // opens has the rest.
        let label = if numbers_only { app.workspaces_summary.0.to_string() } else { label };
        bar.push(Space::new(6, 0).into());
        bar.push(workspaces_chip(label));
    }
    if let Some((count, hint)) = badge {
        bar.push(Space::new(6, 0).into());
        bar.push(pending_requests_badge_button(count, hint));
    }
    bar.push(Space::new(6, 0).into());
    bar.push(bell_button(app.notifications.len(), app.sidebar.show_notifications, numbers_only));
    // The narrowest bar has nothing left to collapse; the server state is
    // still in Settings and the alert banner.
    if !numbers_only {
        bar.push(server_dot(app));
    }
    bar.push(settings_icon_button());

    // `.nav`'s 1px bottom border, as its own strip: iced borders are
    // uniform on all four sides.
    let bottom_rule = container(Space::new(0, 0))
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(|_| container::Style { background: Some(Background::Color(tokens::BORDER)), ..Default::default() });
    container(column![
        row(bar)
            .align_y(Alignment::Center)
            .spacing(2)
            .height(Length::Fill)
            .padding(Padding { top: 0.0, right: 12.0, bottom: 0.0, left: leading_inset(app) }),
        bottom_rule,
    ])
    .width(Length::Fill)
    .height(Length::Fixed(NAV_HEIGHT))
    .style(|_| container::Style {
        background: Some(Background::Color(tokens::PANEL)),
        ..Default::default()
    })
    .into()
}

/// `.nav-link` / `.nav-active`: 13px side padding, 7px label-to-key gap,
/// 12.5px semibold `#a3aabb`; active is `#1b2236` with a 2px teal strip
/// along the bottom — a separate strip, since `Border` is uniform on all
/// four sides in this iced version (round-1 review feedback, item B5).
fn nav_link(tab: Tab, numbers_only: bool) -> Element<'static, Message> {
    let active = tab.active;
    // `.nav-link`'s 13px, until the bar collapses to numbers: six tabs at
    // that width are what pushed `+ Spawn` out of a narrow window.
    let side_pad = if numbers_only { 7.0 } else { 13.0 };

    // Labels take the button's `text_color`, so hover can brighten them.
    let mut label_items: Vec<Element<Message>> = if numbers_only {
        vec![text(tab.key).size(12.5).font(crate::style::MONO).into()]
    } else {
        vec![
            text(tab.label)
                .size(12.5)
                .font(crate::style::SANS_SEMIBOLD)
                .wrapping(iced::widget::text::Wrapping::None)
                .into(),
            Space::new(7, 0).into(),
            text(tab.key).size(10).font(crate::style::MONO).color(tokens::DIM).into(),
        ]
    };

    if let Some(n) = tab.count {
        label_items.push(Space::new(6, 0).into());
        label_items.push(
            container(text(n.to_string()).size(10).color(tokens::AMBER_BADGE_FG))
                .padding([1, 5])
                .style(|_| container::Style {
                    background: Some(Background::Color(tokens::AMBER_BADGE_BG)),
                    border: Border { radius: 9.0.into(), ..Default::default() },
                    ..Default::default()
                })
                .into(),
        );
    }

    // The row is wrapped in a `container` with an explicit `Fill` height
    // and `align_y(Center)` — `Button` has no content-alignment method of
    // its own in this iced version, so without this the label sits top-
    // aligned in the 44px bar (round-1 feedback, item B5 / round-2
    // feedback, item 1).
    let link_button = button(
        container(row(label_items).align_y(Alignment::Center))
            .height(Length::Fill)
            .align_y(iced::alignment::Vertical::Center),
    )
    .on_press(tab.on_press)
    .padding(Padding {
        top: 0.0,
        right: side_pad,
        bottom: 0.0,
        left: side_pad,
    })
    .height(Length::Fixed(NAV_HEIGHT - 3.0))
    .style(move |_t, status| link_style(active, status));

    let tab_row: Vec<Element<Message>> = vec![link_button.into()];

    // A `stack!`, not the `column![button_row, Length::Fill strip]` this
    // replaced: a `Length::Fill` width inside a `Column` fills the LIMIT the
    // column itself was handed by its parent row — not the column's own
    // (otherwise-Shrink) resolved width — so every tab's indicator strip
    // was claiming the row's entire remaining width, and the row then
    // divided that oversubscribed total roughly evenly across all six tabs
    // (the full-bar-width spreading this fixes). A `Stack`'s first/base
    // layer dictates its resolved size, and later layers are laid out
    // against THAT already-resolved size — so the indicator here correctly
    // fills exactly the base layer's (the button's) natural width instead.
    let content = column![row(tab_row).align_y(Alignment::Center), Space::new(0, 2)];
    let indicator = container(
        container(Space::new(0, 0)).width(Length::Fill).height(Length::Fixed(2.0)).style(move |_| {
            container::Style {
                background: Some(Background::Color(if active { tokens::TEAL } else { iced::Color::TRANSPARENT })),
                ..Default::default()
            }
        }),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_y(iced::alignment::Vertical::Bottom);

    iced::widget::stack![content, indicator].into()
}

fn link_style(active: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    button::Style {
        background: match (active, hovered) {
            (true, _) => Some(Background::Color(tokens::NAV_ACTIVE)),
            (false, true) => Some(Background::Color(tokens::NAV_HOVER)),
            (false, false) => None,
        },
        text_color: if active || hovered { tokens::TEXT } else { tokens::NAV_LINK },
        border: Border::default(),
        ..Default::default()
    }
}

/// The always-visible pending-user-requests badge: a standalone pill (not
/// nested in a tab, since it isn't one) between the Spawn button and
/// Settings, rendered on every shell view regardless of active tab or the
/// (now-vestigial, see `app::SidebarState`) collapsed-sidebar state — the
/// spec's "badge/count always visible in the chrome" requirement. Clicking
/// it opens the pending-requests dropdown (`App::iced_view`'s
/// `show_pending_requests` branch), mirroring the Fleet tab's own
/// needs-you badge opening the notifications dropdown.
fn pending_requests_badge_button(count: String, hint: String) -> Element<'static, Message> {
    use iced::widget::tooltip;

    let badge = button(
        container(text(count).size(10).color(tokens::AMBER_BADGE_FG))
            .padding([1, 5])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::AMBER_BADGE_BG)),
                border: Border { radius: 9.0.into(), ..Default::default() },
                ..Default::default()
            }),
    )
    .on_press(Message::TogglePendingRequests)
    .padding(4)
    .style(|_t, status| button::Style {
        background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    });

    tooltip(
        badge,
        container(text(hint).size(11).color(tokens::TEXT))
            .padding([4, 8])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            }),
        tooltip::Position::Bottom,
    )
    .into()
}

/// Distance from the window's right edge to the bell's right edge: the bar's
/// right padding, the settings button, the server dot (absent on the
/// narrowest bar) and their spacers.
pub(crate) fn bell_right_inset(app: &App) -> f32 {
    let dot = if numbers_only(app) { 0.0 } else { 24.0 + 2.0 };
    12.0 + 32.0 + 2.0 + dot
}

pub(crate) fn bell_badge_text(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=99 => Some(count.to_string()),
        _ => Some("99+".to_string()),
    }
}

/// U+F0F3 `nf-fa-bell`, from the bundled Symbols Nerd Font.
const BELL_GLYPH: &str = "\u{f0f3}";

fn bell_button(count: usize, open: bool, compact: bool) -> Element<'static, Message> {
    use iced::widget::tooltip;

    let mut content = row![text(BELL_GLYPH).size(14).font(crate::style::NERD).color(if open { tokens::TEXT } else { tokens::DIM })]
        .spacing(4)
        .align_y(Alignment::Center);
    if let Some(label) = bell_badge_text(count) {
        content = content.push(
            container(text(label).size(10).color(tokens::AMBER_BADGE_FG))
                .padding([1, 5])
                .style(|_| container::Style {
                    background: Some(Background::Color(tokens::AMBER_BADGE_BG)),
                    border: Border { radius: 9.0.into(), ..Default::default() },
                    ..Default::default()
                }),
        );
    }
    let bell = button(content)
        .on_press(Message::ToggleNotifications)
        .padding(if compact { [7, 4] } else { [7, 8] })
        .style(move |_t, status| button::Style {
            background: (open || matches!(status, button::Status::Hovered)).then_some(Background::Color(tokens::PANEL2)),
            border: Border { radius: 6.0.into(), ..Default::default() },
            ..Default::default()
        });
    tooltip(
        bell,
        container(text("Notifications").size(11).color(tokens::TEXT))
            .padding([4, 8])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            }),
        tooltip::Position::Bottom,
    )
    .into()
}

/// The search field collapsed for a narrow bar: opening it shows the field
/// (see `nav`), and `⌘K` still works from anywhere.
fn search_icon_button(compact: bool) -> Element<'static, Message> {
    let mut content = row![text("⌕").size(13).font(crate::style::GLYPH).color(tokens::DIM)]
        .spacing(6)
        .align_y(Alignment::Center);
    // The narrowest bar gives up the shortcut hint to make room for the bell.
    if !compact {
        content = content.push(text("⌘K").size(10).font(crate::style::glyph_font_for("⌘")).color(tokens::DIM2));
    }
    button(content)
    .on_press(Message::GlobalSearchFocus)
    .padding([5, 8])
    .style(|_t, status| button::Style {
        background: Some(Background::Color(if matches!(status, button::Status::Hovered) {
            tokens::PANEL2
        } else {
            tokens::PANEL
        })),
        text_color: tokens::DIM,
        border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
        ..Default::default()
    })
    .into()
}

/// An open search on a narrow bar shows the field in place of the icon,
/// taking the room the tab labels give up.
fn search_shown(app: &App, fit: NavFit) -> bool {
    app.global_search.open && fit != NavFit::Full
}

/// Tabs shrink to their numbers, and the server dot steps aside.
pub(crate) fn numbers_only(app: &App) -> bool {
    let badge = crate::app::pending_requests_badge(app.pending_user_requests.len());
    let fit = nav_fit(
        app.window_width,
        workspaces_chip_label(app.workspaces_summary).as_deref(),
        badge.as_ref().map(|(count, _)| count.as_str()),
    );
    fit == NavFit::TabNumbers || search_shown(app, fit)
}

/// The server's state at a glance: green when it answers and needs
/// nothing, amber with an alert or while restarting, red when nothing
/// answers. Its tooltip says which, and a click opens Settings with the
/// restart confirm.
fn server_dot(app: &App) -> Element<'_, Message> {
    let (color, tip) = server_dot_state(&app.server);
    let dot = button(
        container(Space::new(0, 0))
            .width(Length::Fixed(8.0))
            .height(Length::Fixed(8.0))
            .style(move |_| container::Style {
                background: Some(Background::Color(color)),
                border: Border { radius: 4.0.into(), ..Default::default() },
                ..Default::default()
            }),
    )
    .on_press(Message::OpenServerRestart)
    .padding(8)
    .style(|_t, status| button::Style {
        background: matches!(status, button::Status::Hovered).then_some(Background::Color(tokens::PANEL2)),
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    });
    iced::widget::tooltip(
        dot,
        container(text(tip).size(11).color(tokens::TEXT))
            .padding([4, 8])
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL2)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
                ..Default::default()
            }),
        iced::widget::tooltip::Position::Bottom,
    )
    .into()
}

pub(crate) fn server_dot_state(server: &crate::app::ServerPanel) -> (iced::Color, String) {
    if server.restarting {
        return (tokens::AMBER, "Restarting server…".to_string());
    }
    match (&server.info, server.alerts.first()) {
        (None, _) => (tokens::RED, "Server not answering — click to restart".to_string()),
        (Some(_), Some(alert)) => (tokens::AMBER, format!("{} — click to restart", alert.line())),
        (Some(info), None) => (tokens::GREEN, format!("Server {} · pid {} — click to restart", info.label(), info.pid)),
    }
}

/// Minimal icon-style button: a plain ring, not a text label (round-1
/// review feedback, item B9) and not a Unicode glyph — this codebase only
/// trusts a small verified dingbat set (`⬡ ⌕ ✦ ☰ ⚑`, see `style::GLYPH`'s
/// doc comment) to render via the bundled symbol font, none of which read as
/// "settings", and an unverified codepoint (⚙) risks a missing glyph.
fn settings_icon_button() -> Element<'static, Message> {
    button(
        container(Space::new(0, 0))
            .width(Length::Fixed(12.0))
            .height(Length::Fixed(12.0))
            .style(|_| container::Style {
                border: Border { color: tokens::DIM, width: 1.5, radius: 6.0.into() },
                ..Default::default()
            }),
    )
    .on_press(Message::NavigateSettings)
    .padding(10)
    .style(|_t, status| button::Style {
        background: matches!(status, button::Status::Hovered)
            .then_some(Background::Color(tokens::PANEL2)),
        border: Border { radius: 6.0.into(), ..Default::default() },
        ..Default::default()
    })
    .into()
}

/// `None` when there's nothing leftover (chip is hidden entirely, not just
/// greyed out) — `(count, total_bytes)` is `app.workspaces_summary`, kept
/// fresh by a background task piggybacking on the same periodic tick that
/// already refreshes session state (see `App::update`'s `PollSessions` arm).
/// Roughly what `workspaces_chip` lays out at for `label` (11px text, 10px
/// padding each side, plus its spacer).
fn chip_width_estimate(label: &str) -> f32 {
    label.chars().count() as f32 * 6.5 + 32.0
}

/// The badge's count plus its pill and button padding and leading spacer.
fn badge_width_estimate(count: &str) -> f32 {
    count.chars().count() as f32 * 6.5 + 24.0
}

fn workspaces_chip_label(summary: (usize, u64)) -> Option<String> {
    let (count, bytes) = summary;
    if count == 0 {
        return None;
    }
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let plural = if count == 1 { "" } else { "s" };
    Some(format!("{count} leftover workspace{plural} · {gb:.1} GB"))
}

/// Opens `workspaces_modal` (see `components::workspaces_modal`) — a plain
/// text pill, not an icon button, since its whole point is surfacing the
/// count/size at a glance.
fn workspaces_chip(label: String) -> Element<'static, Message> {
    button(text(label).size(11).color(tokens::DIM))
        .on_press(Message::WorkspacesModalOpen)
        .padding([6, 10])
        .style(|_t, status| button::Style {
            background: matches!(status, button::Status::Hovered)
                .then_some(Background::Color(tokens::PANEL2)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, Session, SessionStatus};
    use std::collections::HashSet;

    fn session(id: &str, status: SessionStatus, activity: ActivityState) -> Session {
        Session {
            id: id.into(), orchestrator_id: None, name: format!("{id}-name"),
            repo: "o/r".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None,
            summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity, activity_note: None, activity_since: None, machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn needs_you_count_matches_classify_members_needs_you_bucket() {
        let sessions = [
            session("a", SessionStatus::CiFailed, ActivityState::Working),
            session("b", SessionStatus::Working, ActivityState::Blocked),
            session("c", SessionStatus::Working, ActivityState::Working),
        ];
        assert_eq!(needs_you_count(sessions.iter(), &HashSet::new()), 2);
    }

    #[test]
    fn needs_you_is_zero_when_nothing_needs_attention() {
        let sessions = [
            session("a", SessionStatus::Working, ActivityState::Working),
            session("b", SessionStatus::Done, ActivityState::Working),
        ];
        assert_eq!(needs_you_count(sessions.iter(), &HashSet::new()), 0);
    }

    /// An orchestrator's own bookkeeping session must not inflate the
    /// badge — mirrors `Island::summary` excluding orchestrators (see
    /// `ninox_overview::islands`'s module doc comment).
    #[test]
    fn needs_you_count_excludes_orchestrator_sessions() {
        let sessions = [session("orch", SessionStatus::CiFailed, ActivityState::Working)];
        let orchestrator_ids: HashSet<&str> = ["orch"].into_iter().collect();
        assert_eq!(needs_you_count(sessions.iter(), &orchestrator_ids), 0);
    }

    /// The prompt-agent singleton (MLOPS-5415) is never a fleet member — it
    /// must never contribute to the "needs you" badge either.
    #[test]
    fn needs_you_count_excludes_the_prompt_agent_session() {
        let mut prompt = session(
            ninox_core::worker_type::PROMPT_SESSION_ID,
            SessionStatus::CiFailed,
            ActivityState::Working,
        );
        prompt.worker_type = Some(ninox_core::worker_type::PROMPT_WORKER_TYPE.to_string());
        assert_eq!(needs_you_count([prompt].iter(), &HashSet::new()), 0);
    }

    #[test]
    fn a_pending_requests_badge_collapses_the_bar_sooner() {
        assert_eq!(nav_fit(SEARCH_ICON_BELOW, None, None), NavFit::Full);
        assert_eq!(nav_fit(SEARCH_ICON_BELOW, None, Some("3")), NavFit::SearchIcon);
        assert_eq!(nav_fit(TAB_NUMBERS_BELOW, None, Some("99+")), NavFit::TabNumbers);
    }

    #[test]
    fn bell_badge_text_hides_at_zero_and_caps_at_99() {
        assert_eq!(bell_badge_text(0), None);
        assert_eq!(bell_badge_text(7).as_deref(), Some("7"));
        assert_eq!(bell_badge_text(99).as_deref(), Some("99"));
        assert_eq!(bell_badge_text(100).as_deref(), Some("99+"));
    }

    #[test]
    fn workspaces_chip_label_is_none_with_zero_leftover() {
        assert_eq!(workspaces_chip_label((0, 0)), None);
    }

    #[test]
    fn workspaces_chip_label_shows_count_and_size_with_leftovers() {
        let label = workspaces_chip_label((3, 2 * 1024 * 1024 * 1024)).unwrap();
        assert_eq!(label, "3 leftover workspaces · 2.0 GB");
    }

    #[test]
    fn workspaces_chip_label_singular_for_one() {
        let label = workspaces_chip_label((1, 0)).unwrap();
        assert!(label.starts_with("1 leftover workspace ·"), "{label}");
    }
}
