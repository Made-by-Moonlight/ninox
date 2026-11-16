//! Leftover-workspaces modal — opened from the nav bar's "N leftover
//! workspaces · X GB" chip (see `shell::nav::workspaces_chip`). Mirrors
//! `ninox workspaces list` plus a two-click Prune (preview, then confirm),
//! kept deliberately minimal per the brief (no `--include-unmerged` toggle
//! here — that stays a CLI-only lever).

use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length,
};

use std::path::PathBuf;

use ninox_core::workspace::{OrphanStatus, Ownership, WorkspaceAction, WorkspaceFacts, WorkspaceRow};

use super::shell::tokens;
use crate::{
    app::{App, Message},
    style::{hline, MONO, SANS, SANS_BOLD},
};

/// A `Debug + Clone` projection of `ninox_core::workspace::WorkspaceRow`
/// (which derives neither) — needed to carry rows inside `Message`, which
/// derives both. All fields are plain copies/clones of the real struct's.
#[derive(Debug, Clone)]
pub struct WorkspaceRowView {
    pub path: PathBuf,
    pub session_id: Option<String>,
    pub status: OrphanStatus,
    pub facts: WorkspaceFacts,
    pub size_bytes: u64,
    pub action: WorkspaceAction,
}

impl From<WorkspaceRow> for WorkspaceRowView {
    fn from(row: WorkspaceRow) -> Self {
        Self {
            path: row.path,
            session_id: row.session_id,
            status: row.status,
            facts: row.facts,
            size_bytes: row.size_bytes,
            action: row.action,
        }
    }
}

/// State for the modal opened from the nav chip. `None` on `App` means
/// closed — same shape as `spawn_modal`/`catalogue_modal`.
#[derive(Default)]
pub struct WorkspacesModalState {
    pub rows: Vec<WorkspaceRowView>,
    pub loading: bool,
    /// First Prune click sets this (shows the preview); a second click with
    /// this already `true` is the actual confirm.
    pub confirming_prune: bool,
    pub status: Option<String>,
}

/// Every row worth telling a human about — anything the sweeper would
/// either clean or flag, never a `Live`/`NotOrphanYet`/`NotOurs` row. Shared
/// by the modal's list and the nav chip's count/size.
pub fn leftover_rows(rows: &[WorkspaceRowView]) -> Vec<&WorkspaceRowView> {
    rows.iter().filter(|r| !matches!(r.action, WorkspaceAction::None)).collect()
}

/// `(count, total_bytes)` across every leftover row — what the nav chip
/// shows, and what it's hidden entirely when `count == 0`.
pub fn leftover_summary(rows: &[WorkspaceRowView]) -> (usize, u64) {
    let leftover = leftover_rows(rows);
    (leftover.len(), leftover.iter().map(|r| r.size_bytes).sum())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn action_label(action: WorkspaceAction) -> &'static str {
    match action {
        WorkspaceAction::None => "none",
        WorkspaceAction::WouldClean { unmerged_with_content: true, .. } => "would clean (unmerged)",
        WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false } => "would clean (worktree + branch)",
        WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: false } => "would clean (worktree)",
        WorkspaceAction::NeedsAttention(_) => "needs attention",
    }
}

fn status_label(status: OrphanStatus) -> &'static str {
    match status {
        OrphanStatus::Live => "live",
        OrphanStatus::NotOrphanYet => "pending retention",
        OrphanStatus::NotOurs => "not ours",
        OrphanStatus::Orphan(Ownership::Tracked) => "orphan",
        OrphanStatus::Orphan(Ownership::LegacyMatch) => "orphan (legacy)",
        OrphanStatus::Orphan(Ownership::NotOurs) => "orphan (not ours)",
    }
}

/// Column widths (`FillPortion`) for a row that has at least one session id
/// among the leftover rows. The session column is omitted entirely (see
/// [`row_widths`]) when none do — no row in a given repo scan carries one,
/// so showing a column of bare `—`s would just be noise.
const WIDTHS_WITH_SESSION: [u16; 5] = [3, 2, 2, 1, 3];
/// Same columns with the session slot dropped.
const WIDTHS_WITHOUT_SESSION: [u16; 4] = [3, 2, 1, 3];

pub fn workspaces_modal<'a>(_state: &'a App, modal: &'a WorkspacesModalState) -> Element<'a, Message> {
    let leftover = leftover_rows(&modal.rows);
    let any_session = leftover.iter().any(|r| r.session_id.is_some());

    let header = container(
        text("Leftover workspaces").size(13).font(SANS).color(tokens::TEXT),
    )
    .padding([14, 20])
    .width(Length::Fill)
    .style(|_theme| container::Style { background: Some(Background::Color(tokens::PANEL2)), ..Default::default() });

    let rows_list: Element<Message> = if leftover.is_empty() {
        text(if modal.loading { "Loading…" } else { "No leftover workspaces." })
            .size(12)
            .color(tokens::DIM)
            .into()
    } else {
        let mut col = column![].spacing(6);
        for row_data in &leftover {
            let name = row_data
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| row_data.path.display().to_string());
            let dirty_marker = if row_data.facts.dirty { " (dirty)" } else { "" };

            let mut cells: Vec<Element<Message>> =
                vec![text(name).size(12).font(MONO).color(tokens::TEXT).into()];
            if any_session {
                cells.push(
                    text(row_data.session_id.as_deref().unwrap_or("—").to_string())
                        .size(11).font(MONO).color(tokens::DIM).into(),
                );
            }
            cells.push(text(status_label(row_data.status)).size(11).color(tokens::DIM).into());
            cells.push(
                container(text(human_size(row_data.size_bytes)).size(11).font(MONO).color(tokens::DIM))
                    .width(Length::Fill)
                    .align_x(Alignment::End)
                    .into(),
            );
            cells.push(
                text(format!("{}{dirty_marker}", action_label(row_data.action)))
                    .size(11).color(tokens::DIM).into(),
            );

            let widths: &[u16] = if any_session { &WIDTHS_WITH_SESSION } else { &WIDTHS_WITHOUT_SESSION };
            let mut line = row![].align_y(Alignment::Center).spacing(8);
            for (cell, width) in cells.into_iter().zip(widths) {
                line = line.push(container(cell).width(Length::FillPortion(*width)));
            }
            col = col.push(line);
        }
        scrollable(col).height(Length::Fixed(240.0)).into()
    };

    let prunable = leftover.iter().filter(|r| ninox_core::workspace::is_default_prune_candidate(r.action)).count();

    let status_line: Option<Element<Message>> =
        modal.status.as_ref().map(|msg| text(msg.clone()).size(11).color(tokens::DIM).into());

    let cancel_button = button(text("Close").size(11).font(SANS_BOLD).color(tokens::DIM))
        .on_press(Message::WorkspacesModalClose)
        .padding([9, 18])
        .style(|_theme, status| button::Style {
            background: None,
            text_color: tokens::DIM,
            border: Border {
                color: if status == button::Status::Hovered { tokens::TEXT } else { tokens::BORDER },
                width: 1.0,
                radius: 4.0.into(),
            },
            ..Default::default()
        });

    let prune_label = if modal.confirming_prune {
        format!("Confirm prune ({prunable})")
    } else {
        format!("Prune ({prunable})")
    };
    let can_prune = prunable > 0;
    let prune_button = button(
        text(prune_label).size(12).font(SANS_BOLD).color(if can_prune { tokens::BG } else { tokens::DIM2 }),
    )
    .on_press_maybe(can_prune.then_some(Message::WorkspacesPruneClicked))
    .padding([9, 20])
    .style(move |_theme, status| {
        let hovered = can_prune && status == button::Status::Hovered;
        button::Style {
            background: Some(Background::Color(if can_prune {
                if hovered { Color { a: 0.85, ..tokens::TEAL } } else { tokens::TEAL }
            } else {
                tokens::PANEL2
            })),
            text_color: if can_prune { tokens::BG } else { tokens::DIM2 },
            border: Border { color: tokens::BORDER, width: 1.0, radius: 4.0.into() },
            ..Default::default()
        }
    });

    let footer = row![Space::new(Length::Fill, 0), cancel_button, prune_button]
        .spacing(12)
        .align_y(Alignment::Center);

    let mut body = column![rows_list].padding([20, 24]).spacing(14);
    if let Some(line) = status_line {
        body = body.push(line);
    }
    body = body.push(Space::new(0, 8)).push(footer);

    let modal_box = container(column![header, hline(tokens::BORDER, 1.0), body])
        .width(Length::Fixed(560.0))
        .style(|_theme| container::Style {
            background: Some(Background::Color(tokens::PANEL)),
            border: Border { color: tokens::BORDER, width: 1.0, radius: 6.0.into() },
            ..Default::default()
        });

    container(modal_box)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(Color { a: 0.6, ..Color::BLACK })),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::workspace::SkipReason;

    fn facts() -> WorkspaceFacts {
        WorkspaceFacts {
            dirty: false,
            has_branch: true,
            has_upstream: true,
            unpushed_commit_count: 0,
            commits_beyond_base: 0,
            merged: true,
            commits_since_merge: 0,
            interrupted: false,
            live_session_row: false,
        }
    }

    fn row(action: WorkspaceAction) -> WorkspaceRowView {
        WorkspaceRowView {
            path: PathBuf::from("/repo/.claude/worktrees/w1"),
            session_id: Some("w1".into()),
            status: OrphanStatus::Orphan(Ownership::Tracked),
            facts: facts(),
            size_bytes: 1_073_741_824, // 1 GiB
            action,
        }
    }

    #[test]
    fn human_size_uses_a_space_before_the_unit() {
        assert_eq!(human_size(220), "220 B");
        assert_eq!(human_size(2048), "2.0 KB");
    }

    #[test]
    fn column_widths_match_cell_counts_with_and_without_a_session_column() {
        assert_eq!(WIDTHS_WITH_SESSION.len(), 5, "name, session, status, size, action");
        assert_eq!(WIDTHS_WITHOUT_SESSION.len(), 4, "name, status, size, action — session dropped");
    }

    #[test]
    fn leftover_summary_is_zero_for_no_rows() {
        assert_eq!(leftover_summary(&[]), (0, 0));
    }

    #[test]
    fn leftover_summary_excludes_none_action_rows() {
        let rows = vec![row(WorkspaceAction::None)];
        assert_eq!(leftover_summary(&rows), (0, 0));
    }

    #[test]
    fn leftover_summary_counts_would_clean_and_needs_attention_rows() {
        let rows = vec![
            row(WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false }),
            row(WorkspaceAction::NeedsAttention(SkipReason::Dirty)),
            row(WorkspaceAction::None),
        ];
        let (count, bytes) = leftover_summary(&rows);
        assert_eq!(count, 2);
        assert_eq!(bytes, 2 * 1_073_741_824);
    }

    #[test]
    fn action_label_covers_every_variant_without_panicking() {
        assert_eq!(action_label(WorkspaceAction::None), "none");
        assert_eq!(
            action_label(WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false }),
            "would clean (worktree + branch)",
        );
        assert_eq!(
            action_label(WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: false }),
            "would clean (worktree)",
        );
        assert_eq!(
            action_label(WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: true }),
            "would clean (unmerged)",
        );
        assert_eq!(action_label(WorkspaceAction::NeedsAttention(SkipReason::Dirty)), "needs attention");
    }

    #[test]
    fn status_label_covers_every_variant_without_panicking() {
        for status in [
            OrphanStatus::Live,
            OrphanStatus::NotOrphanYet,
            OrphanStatus::NotOurs,
            OrphanStatus::Orphan(Ownership::Tracked),
            OrphanStatus::Orphan(Ownership::LegacyMatch),
            OrphanStatus::Orphan(Ownership::NotOurs),
        ] {
            let _ = status_label(status);
        }
    }
}
