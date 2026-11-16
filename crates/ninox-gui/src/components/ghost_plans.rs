//! Ghost plans in the GUI (`PromptReply.dc.html`, `FleetFocused.dc.html`):
//! dashed ghost islands and nodes in Fleet, their detail panel, the Plans
//! tab, and the reply drawer's ghost cards. Accept and discard always go
//! through an inline confirm, then `ninox_ui_shared::ghost_accept` as the
//! user.

use iced::widget::canvas::{self, Frame, Geometry, LineDash, Path, Stroke};
use iced::widget::{button, column, container, mouse_area, row, scrollable, stack, text, Space};
use iced::{mouse, Alignment, Background, Border, Color, Element, Length, Padding, Rectangle, Renderer, Theme};

use ninox_core::ghost_plan::{NodeRow, PlanStatus, PlanWithNodes};
use ninox_ui_shared::ghost_view;

use super::fleet_focus::badge;
use super::shell::tokens;
use crate::app::{App, Message};

const GHOST_BG: Color = tokens::hex(0x151327);
const GHOST_DASH: Color = tokens::hex(0x6f64c9);
const CARD_WIDTH: f32 = 260.0;

#[derive(Debug, Default)]
pub struct GhostUi {
    pub plans: Vec<PlanWithNodes>,
    pub selected: Option<GhostSelection>,
    pub confirm: Option<GhostConfirm>,
    /// The plan whose accept/discard is running.
    pub busy: Option<String>,
    /// The last decision's outcome, shown on the Plans tab and the detail panel.
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostSelection {
    pub plan_id: String,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhostConfirm {
    Accept(String),
    Discard(String),
}

impl GhostUi {
    pub fn plan(&self, id: &str) -> Option<&PlanWithNodes> {
        self.plans.iter().find(|p| p.plan.id == id)
    }

    pub fn selected_plan(&self) -> Option<(&PlanWithNodes, Option<&NodeRow>)> {
        let sel = self.selected.as_ref()?;
        let plan = self.plan(&sel.plan_id)?;
        Some((plan, sel.node_id.as_deref().and_then(|n| plan.node(n))))
    }
}

/// A dashed rounded outline drawn over its content (`iced::Border` has no
/// dash style).
struct Dashed {
    color: Color,
    width: f32,
}

impl canvas::Program<Message> for Dashed {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let inset = self.width / 2.0;
        let path = Path::rounded_rectangle(
            iced::Point::new(inset, inset),
            iced::Size::new(bounds.width - self.width, bounds.height - self.width),
            8.0.into(),
        );
        frame.stroke(
            &path,
            Stroke {
                line_dash: LineDash { segments: &[5.0, 4.0], offset: 0 },
                ..Stroke::default().with_color(self.color).with_width(self.width)
            },
        );
        vec![frame.into_geometry()]
    }
}

fn dashed<'a>(content: Element<'a, Message>, selected: bool) -> Element<'a, Message> {
    let (color, width) = if selected { (tokens::VIOLET, 2.0) } else { (GHOST_DASH, 1.0) };
    let body = container(content).style(|_| container::Style {
        background: Some(Background::Color(GHOST_BG)),
        border: Border { radius: 8.0.into(), ..Default::default() },
        ..Default::default()
    });
    stack![body, canvas::Canvas::new(Dashed { color, width }).width(Length::Fill).height(Length::Fill)].into()
}

fn small(s: impl Into<String>, color: Color) -> Element<'static, Message> {
    text(s.into()).size(10.5).font(crate::style::SANS).color(color).into()
}

fn ghost_badge(node: &NodeRow) -> Element<'static, Message> {
    badge(ghost_view::badge(node), tokens::VIOLET_BADGE_BG, tokens::VIOLET_BADGE_FG)
}

fn is_selected(app: &App, plan: &PlanWithNodes, node: Option<&NodeRow>) -> bool {
    app.ghost.selected.as_ref().is_some_and(|s| s.plan_id == plan.plan.id && s.node_id.as_deref() == node.map(|n| n.node_id.as_str()))
}

/// One ghost node as a card: GHOST badge, name, brief, dependencies.
pub fn ghost_node_card<'a>(app: &'a App, plan: &'a PlanWithNodes, node: &'a NodeRow) -> Element<'a, Message> {
    let mut lines: Vec<Element<Message>> = vec![
        ghost_badge(node),
        text(node.name.clone()).size(12.5).font(crate::style::SANS).color(tokens::TEXT).into(),
        small(ninox_overview::display::display_brief(&node.brief), tokens::DIM),
    ];
    if let Some(deps) = ghost_view::dependency_line(plan, node) {
        lines.push(small(deps, tokens::DIM2));
    }
    let card = container(dashed(container(column(lines).spacing(5)).padding([10, 12]).into(), is_selected(app, plan, Some(node))))
        .width(Length::Fixed(CARD_WIDTH));
    mouse_area(card)
        .on_press(Message::GhostSelect { plan_id: plan.plan.id.clone(), node_id: Some(node.node_id.clone()) })
        .into()
}

/// A pending plan of new top-level orchestrators as its own dashed island
/// in the Fleet grid.
pub fn ghost_island<'a>(app: &'a App, plan: &'a PlanWithNodes) -> Element<'a, Message> {
    let mut lines: Vec<Element<Message>> = vec![
        badge("GHOST".into(), tokens::VIOLET_BADGE_BG, tokens::VIOLET_BADGE_FG),
        text(ghost_view::plan_title(plan)).size(13).font(crate::style::SANS).color(tokens::TEXT).into(),
        small(ghost_view::plan_status_line(plan), tokens::DIM),
    ];
    for node in ghost_view::ghost_nodes(plan) {
        let indent = 12.0 * ghost_view::depth(plan, node) as f32;
        lines.push(
            mouse_area(
                row![
                    Space::with_width(Length::Fixed(indent)),
                    column![
                        text(node.name.clone()).size(12).font(crate::style::SANS).color(tokens::TEXT),
                        text(node.worker_type.clone()).size(10).font(crate::style::MONO).color(tokens::VIOLET),
                    ]
                    .spacing(2),
                ],
            )
            .on_press(Message::GhostSelect { plan_id: plan.plan.id.clone(), node_id: Some(node.node_id.clone()) })
            .into(),
        );
    }
    lines.push(small(ghost_view::NOTHING_SPAWNS, tokens::DIM2));
    let island = dashed(container(column(lines).spacing(7)).padding(14).width(Length::Fill).into(), is_selected(app, plan, None));
    mouse_area(island).on_press(Message::GhostSelect { plan_id: plan.plan.id.clone(), node_id: None }).into()
}

/// Ghost nodes proposed under the orchestrators in an opened island, one
/// block per plan, below the island's own tree.
pub fn ghosts_for_island<'a>(app: &'a App, orchestrators: &[&str]) -> Option<Element<'a, Message>> {
    let plans: Vec<&PlanWithNodes> = orchestrators.iter().flat_map(|o| ghost_view::ghosts_under(&app.ghost.plans, o)).collect();
    if plans.is_empty() {
        return None;
    }
    let blocks: Vec<Element<Message>> = plans
        .into_iter()
        .map(|plan| {
            let cards: Vec<Element<Message>> = ghost_view::ghost_nodes(plan).into_iter().map(|n| ghost_node_card(app, plan, n)).collect();
            column![
                row![
                    badge("GHOST PLAN".into(), tokens::VIOLET_BADGE_BG, tokens::VIOLET_BADGE_FG),
                    text(format!("{} · under {}", ghost_view::plan_title(plan), plan.plan.target_parent.as_deref().unwrap_or("?")))
                        .size(11.5)
                        .font(crate::style::SANS)
                        .color(tokens::DIM),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
                row(cards).spacing(14).wrap(),
                small(ghost_view::NOTHING_SPAWNS, tokens::DIM2),
            ]
            .spacing(10)
            .into()
        })
        .collect();
    Some(column(blocks).spacing(18).into())
}

fn btn<'a>(label: String, msg: Option<Message>, primary: bool, danger: bool) -> Element<'a, Message> {
    let color = if danger { tokens::RED } else if primary { tokens::hex(0xbff3ec) } else { tokens::TEXT };
    button(text(label).size(12).font(crate::style::SANS).color(color))
        .on_press_maybe(msg)
        .padding([6, 12])
        .style(move |_t, status| {
            let hovered = matches!(status, button::Status::Hovered);
            let (bg, border) = if primary {
                (tokens::hex(0x123f3b), tokens::hex(0x2a7d75))
            } else {
                (if hovered { tokens::PANEL2 } else { tokens::PANEL }, tokens::BORDER)
            };
            button::Style {
                background: Some(Background::Color(bg)),
                border: Border { color: border, width: 1.0, radius: 6.0.into() },
                ..Default::default()
            }
        })
        .into()
}

/// Accept / (Open in Plans) / Discard for a pending plan, or the inline
/// confirm when one is armed for it.
pub fn plan_actions<'a>(app: &'a App, plan: &'a PlanWithNodes, with_open: bool) -> Element<'a, Message> {
    let id = plan.plan.id.clone();
    if plan.plan.status != PlanStatus::Pending {
        return small(ghost_view::plan_status_line(plan), tokens::DIM);
    }
    if app.ghost.busy.as_deref() == Some(id.as_str()) {
        return small("working…", tokens::DIM);
    }
    match &app.ghost.confirm {
        Some(GhostConfirm::Accept(p)) if *p == id => row![
            text(ghost_view::confirm_text(plan)).size(12.5).font(crate::style::SANS).color(tokens::TEXT),
            btn("Spawn".into(), Some(Message::GhostConfirmed), true, false),
            btn("Cancel".into(), Some(Message::GhostConfirmCancel), false, false),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into(),
        Some(GhostConfirm::Discard(p)) if *p == id => row![
            text(ghost_view::discard_confirm_text(plan)).size(12.5).font(crate::style::SANS).color(tokens::TEXT),
            btn("Discard".into(), Some(Message::GhostConfirmed), false, true),
            btn("Cancel".into(), Some(Message::GhostConfirmCancel), false, false),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into(),
        _ => {
            let mut r = row![btn(ghost_view::accept_label(plan), Some(Message::GhostAcceptRequest(id.clone())), true, false)].spacing(10);
            if with_open {
                r = r.push(btn("Open in Plans".into(), Some(Message::GhostOpenInPlans(id.clone())), false, false));
            }
            r.push(btn("Discard".into(), Some(Message::GhostDiscardRequest(id)), false, false)).align_y(Alignment::Center).into()
        }
    }
}

fn field(label: &'static str, value: String) -> Element<'static, Message> {
    column![
        text(label).size(9.5).font(crate::style::SANS).color(tokens::DIM2),
        text(value).size(11.5).font(crate::style::SANS).color(tokens::TEXT),
    ]
    .spacing(3)
    .into()
}

/// The detail panel for a selected ghost plan or node.
pub fn detail_panel<'a>(app: &'a App) -> Option<Element<'a, Message>> {
    let (plan, node) = app.ghost.selected_plan()?;
    let mut items: Vec<Element<Message>> = vec![
        text("DETAIL — GHOST PLAN").size(9.5).font(crate::style::SANS).color(tokens::DIM2).into(),
        Space::with_height(8).into(),
    ];
    match node {
        Some(n) => {
            items.push(ghost_badge(n));
            items.push(text(n.name.clone()).size(15).font(crate::style::SANS).color(tokens::TEXT).into());
            items.push(small(format!("{} · node {}", plan.plan.id, n.node_id), tokens::DIM2));
            items.push(Space::with_height(6).into());
            items.push(field("Brief", n.brief.clone()));
            items.push(field("Worker type", n.worker_type.clone()));
            items.push(field("Repo", n.workspace.clone().unwrap_or_else(|| "—".into())));
            items.push(field("Dependencies", ghost_view::dependency_line(plan, n).unwrap_or_else(|| "none".into())));
            if let Some(m) = &n.model {
                items.push(field("Model", m.clone()));
            }
            if let Some(b) = &n.brain {
                items.push(field("Brain write", b.write.display().to_string()));
            }
            if let Some(l) = &n.linear_ref {
                items.push(field("Linear", l.clone()));
            }
            items.push(field("State", ghost_view::node_state_line(n)));
        }
        None => {
            items.push(badge("GHOST".into(), tokens::VIOLET_BADGE_BG, tokens::VIOLET_BADGE_FG));
            items.push(text(ghost_view::plan_title(plan)).size(15).font(crate::style::SANS).color(tokens::TEXT).into());
            items.push(small(plan.plan.id.clone(), tokens::DIM2));
            items.push(Space::with_height(6).into());
            for n in plan.top_down() {
                items.push(field(
                    "Node",
                    format!("{}{} · {} — {}", "  ".repeat(ghost_view::depth(plan, n)), n.name, n.worker_type, ghost_view::node_state_line(n)),
                ));
            }
        }
    }
    items.push(Space::with_height(8).into());
    items.push(small(ghost_view::plan_status_line(plan), tokens::DIM));
    if let Some(notice) = &app.ghost.notice {
        items.push(small(notice.clone(), tokens::TEAL));
    }
    Some(
        container(scrollable(container(column(items).spacing(6)).padding(16).width(Length::Fill)).height(Length::Fill))
            .width(Length::Fixed(super::fleet_detail::PANEL_WIDTH))
            .height(Length::Fill)
            .style(|_| container::Style { background: Some(Background::Color(tokens::PANEL)), ..Default::default() })
            .into(),
    )
}

/// The bottom action row for a selected ghost.
pub fn action_row<'a>(app: &'a App) -> Option<Element<'a, Message>> {
    let (plan, _) = app.ghost.selected_plan()?;
    Some(
        container(row![plan_actions(app, plan, false), Space::with_width(Length::Fill), small("ghost actions · selection: plan", tokens::DIM2)].align_y(Alignment::Center))
            .width(Length::Fill)
            .height(Length::Fixed(62.0))
            .padding(Padding { top: 0.0, right: 20.0, bottom: 0.0, left: 20.0 })
            .center_y(Length::Fixed(62.0))
            .style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
                ..Default::default()
            })
            .into(),
    )
}

/// The Plans tab: every pending plan, then recent decided/expired ones.
pub fn plans_view(app: &App) -> Element<'_, Message> {
    let mut items: Vec<Element<Message>> = vec![
        text("Plans").size(18).font(crate::style::SANS_BOLD).color(tokens::TEXT).into(),
        small(
            "Orchestrators' registered plan docs, and ghost plans proposed by the Ninox prompt, orchestrators or `ninox plan propose`. Nothing spawns until you accept.",
            tokens::DIM,
        ),
    ];
    items.extend(super::orchestrator_plans::section(app));
    items.push(Space::with_height(8).into());
    let pending = app.ghost.plans.iter().filter(|p| p.plan.status == PlanStatus::Pending).count();
    items.push(
        row![
            crate::style::micro_label("Ghost plans", tokens::DIM2),
            text(format!("{pending} pending")).size(11).font(crate::style::SANS).color(tokens::DIM),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .into(),
    );
    if let Some(notice) = &app.ghost.notice {
        items.push(small(notice.clone(), tokens::TEAL));
    }
    if app.ghost.plans.is_empty() {
        items.push(small("No ghost plans yet. Ask ninox to propose some sessions.", tokens::DIM));
    }
    for plan in &app.ghost.plans {
        let mut lines: Vec<Element<Message>> = vec![
            row![
                badge(plan.plan.status.as_str().to_uppercase(), tokens::VIOLET_BADGE_BG, tokens::VIOLET_BADGE_FG),
                text(ghost_view::plan_title(plan)).size(14).font(crate::style::SANS).color(tokens::TEXT),
                text(plan.plan.id.clone()).size(10.5).font(crate::style::MONO).color(tokens::DIM2),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            small(ghost_view::plan_status_line(plan), tokens::DIM),
        ];
        for n in plan.top_down() {
            let indent = 14.0 * ghost_view::depth(plan, n) as f32;
            lines.push(
                mouse_area(
                    row![
                        Space::with_width(Length::Fixed(indent)),
                        text(n.worker_type.clone()).size(10).font(crate::style::MONO).color(tokens::VIOLET),
                        text(n.name.clone()).size(12).font(crate::style::SANS).color(tokens::TEXT),
                        text(ghost_view::node_state_line(n)).size(10.5).font(crate::style::SANS).color(tokens::DIM2),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                )
                .on_press(Message::GhostSelect { plan_id: plan.plan.id.clone(), node_id: Some(n.node_id.clone()) })
                .into(),
            );
        }
        lines.push(plan_actions(app, plan, false));
        let selected = app.ghost.selected.as_ref().is_some_and(|s| s.plan_id == plan.plan.id);
        let card = container(column(lines).spacing(7)).padding(14).width(Length::Fill);
        let card: Element<Message> = if plan.plan.status == PlanStatus::Pending {
            dashed(card.into(), selected)
        } else {
            card.style(|_| container::Style {
                background: Some(Background::Color(tokens::PANEL)),
                border: Border { color: tokens::BORDER, width: 1.0, radius: 8.0.into() },
                ..Default::default()
            })
            .into()
        };
        items.push(mouse_area(card).on_press(Message::GhostSelect { plan_id: plan.plan.id.clone(), node_id: None }).into());
    }
    if pending > 0 {
        items.push(small(ghost_view::NOTHING_SPAWNS, tokens::DIM2));
    }
    match super::orchestrator_plans::detail(app) {
        Some(detail) => row![
            container(scrollable(container(column(items).spacing(12)).padding(18).width(Length::Fill)).height(Length::Fill))
                .width(Length::Fixed(420.0))
                .height(Length::Fill)
                .style(|_| container::Style {
                    border: Border { color: tokens::BORDER, width: 1.0, radius: 0.0.into() },
                    ..Default::default()
                }),
            container(detail).width(Length::Fill).height(Length::Fill),
        ]
        .into(),
        None => scrollable(container(column(items).spacing(12).max_width(900)).padding(24).width(Length::Fill).center_x(Length::Fill))
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
    }
}

/// The reply drawer's ghost cards for the plans proposed in one turn.
pub fn drawer_cards<'a>(app: &'a App, plan_ids: &[String]) -> Option<Element<'a, Message>> {
    let plans: Vec<&PlanWithNodes> = plan_ids.iter().filter_map(|id| app.ghost.plan(id)).collect();
    if plans.is_empty() {
        return None;
    }
    let blocks: Vec<Element<Message>> = plans
        .into_iter()
        .map(|plan| {
            let nodes: Vec<&NodeRow> = if plan.plan.status == PlanStatus::Pending { ghost_view::ghost_nodes(plan) } else { plan.top_down() };
            let cards: Vec<Element<Message>> = nodes.into_iter().map(|n| ghost_node_card(app, plan, n)).collect();
            let mut parts: Vec<Element<Message>> = vec![row(cards).spacing(10).wrap().into(), plan_actions(app, plan, true)];
            if plan.plan.status == PlanStatus::Pending {
                parts.push(small(ghost_view::NOTHING_SPAWNS, tokens::DIM2));
            }
            column(parts).spacing(10).into()
        })
        .collect();
    Some(column(blocks).spacing(14).into())
}
