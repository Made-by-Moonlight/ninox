//! The v1 shell: nav bar, collapsible tree, centre content + always-on
//! ninox prompt, detail panel, and the bottom context-action row. Replaces
//! the removed `components::sidebar` cream masthead/TOC — see
//! `docs/superpowers/plans/2026-10-27-v1-gui-shell.md` for the full PR plan
//! and the design tokens' provenance.

pub mod nav;
pub mod owl_mark;
pub mod prompt_bar;
pub mod reply_drawer;
pub mod tokens;
pub mod tree;

use iced::{
    widget::{column, container, row, stack},
    Background, Element, Length,
};

use crate::app::{App, Message};
use crate::components::fleet_detail;

/// Wraps `centre` (the active tab's main content) with the shell chrome:
/// nav, tree, the ninox prompt docked under `centre`, and — only on the
/// Fleet tab with an opened island's node selected (`fleet_detail::
/// context`) — the detail panel (right) and the bottom context-action row
/// (full width, below everything else). Neither renders with nothing
/// selected (brief: "Must not open the panel with nothing selected").
/// Used for every tab reachable from the nav bar; `View::SessionDetail`/
/// `View::Settings` are navigated to some other way (the tree, or the gear
/// icon) and keep today's full-takeover rendering instead of going through
/// this wrapper — see `app::iced_view`.
///
/// `islands`: the Fleet tab's current islands (`None` on every other tab) —
/// computed once by `app::iced_view` and threaded through to the tree (and
/// now the detail panel/action row) so an orchestrator row can highlight
/// its island without a second `fleet_islands::build_fleet_islands` call
/// (MLOPS-5339 slice 2 round-1 feedback, item 3). `edges`: the open
/// island's hierarchy edges (`fleet_focus::nodes_for_island`'s second
/// element) when one is open — likewise computed once by `iced_view` and
/// threaded through, rather than called again here, since `fleet_focus`
/// needs the exact same edges to draw the opened island's own cards and
/// would otherwise redo the same per-member work a second time every frame
/// (code-review finding).
pub fn shell<'a>(
    app: &'a App,
    centre: Element<'a, Message>,
    islands: Option<&[ninox_overview::islands::Island]>,
    edges: Option<&[ninox_overview::edge::OverviewEdge]>,
) -> Element<'a, Message> {
    // The opened island's name, if the Fleet tab has one open — scopes the
    // ninox prompt's placeholder (`prompt_bar`'s own doc comment).
    let open_island_name = app.fleet_nav.open.as_ref()
        .and_then(|key| islands.and_then(|is| is.iter().find(|i| &i.key == key)))
        .map(|i| i.name.as_str());

    let detail_ctx = fleet_detail::context(&app.view, &app.fleet_nav, islands);

    let centre: Element<Message> = container(centre).width(Length::Fill).height(Length::Fill).style(|_| {
        container::Style {
            background: Some(Background::Color(tokens::BG)),
            ..Default::default()
        }
    }).into();
    // The reply drawer rises over the bottom of the current view rather
    // than displacing it (`PromptReply.dc.html`).
    let centre = match reply_drawer::reply_drawer(app) {
        Some(drawer) => stack![centre, drawer].into(),
        None => centre,
    };
    let mut centre_column: Vec<Element<Message>> = vec![centre];
    if let Some(indicator) = reply_drawer::collapsed_indicator(app) {
        centre_column.push(indicator);
    }
    centre_column.push(prompt_bar::prompt_bar(app, open_island_name));

    let mut body_items: Vec<Element<Message>> = vec![
        tree::tree(app, islands),
        column(centre_column).width(Length::Fill).height(Length::Fill).into(),
    ];
    // A selected ghost replaces the session detail on Fleet and Plans.
    let ghost_detail = matches!(app.view, crate::app::View::Fleet | crate::app::View::Plans)
        .then(|| crate::components::ghost_plans::detail_panel(app))
        .flatten();
    let ghost_detail_open = ghost_detail.is_some();
    let detail_ctx = if ghost_detail_open { None } else { detail_ctx };
    let ghost_actions = ghost_detail.is_some().then(|| crate::components::ghost_plans::action_row(app)).flatten();
    if let Some(panel) = ghost_detail {
        body_items.push(fleet_detail::divider());
        body_items.push(panel);
    }
    if let Some((island, selected)) = detail_ctx {
        body_items.push(fleet_detail::divider());
        body_items.push(fleet_detail::detail_panel(app, island, edges.unwrap_or_default(), selected));
    }
    let body = row(body_items).height(Length::Fill);

    let mut outer: Vec<Element<Message>> = vec![nav::nav(app)];
    if let Some(warning) = &app.config_warning {
        outer.push(crate::components::config_banner::config_banner(warning));
    }
    if let Some(banner) = crate::components::config_banner::server_banner(app) {
        outer.push(banner);
    }
    outer.push(container(body).height(Length::Fill).into());
    if let Some(actions) = ghost_actions {
        outer.push(actions);
    }
    let action_ctx = detail_ctx.or_else(|| {
        if ghost_detail_open { None } else { fleet_detail::board_context(&app.view, &app.fleet_nav, islands) }
    });
    if let Some((island, selected)) = action_ctx {
        outer.push(fleet_detail::action_row(app, island, selected));
    }
    column(outer).into()
}
