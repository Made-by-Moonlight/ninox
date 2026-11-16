//! The Remotes tab's `W` panel: a read-only view of leftover agent
//! worktrees across every repo Ninox has spawned into (see
//! `ninox_core::workspace`). Mirrors `remotes.rs`'s shape — a display-only
//! row projection built by `collect()`, plus a small view-state struct for
//! selection. Never deletes anything; pruning is a CLI-only action
//! (`ninox workspaces prune`).

use ninox_core::config::AppConfig;
use ninox_core::store::Store;
use ninox_core::workspace::{self, OrphanStatus, Ownership, SkipReason, WorkspaceAction, WorkspaceRow};

pub struct WorkspaceRowView {
    pub path: String,
    pub session_id: String,
    pub status: String,
    pub size: String,
    pub action: String,
    /// Raw byte count behind `size` — needed for the `p` preview's total
    /// reclaimable size, which must sum real bytes, not re-parse the
    /// formatted string.
    pub size_bytes: u64,
    /// Whether the CLI's default (merged-or-empty, no `--include-unmerged`)
    /// prune would act on this row — drives the `p` preview summary. This
    /// panel is read-only regardless: nothing here ever calls `cleanup()`.
    pub prunable: bool,
}

fn status_str(status: OrphanStatus) -> &'static str {
    match status {
        OrphanStatus::Live => "live",
        OrphanStatus::NotOrphanYet => "not-orphan-yet",
        OrphanStatus::NotOurs => "not-ours",
        OrphanStatus::Orphan(Ownership::Tracked) => "orphan(tracked)",
        OrphanStatus::Orphan(Ownership::LegacyMatch) => "orphan(legacy)",
        OrphanStatus::Orphan(Ownership::NotOurs) => "orphan(not-ours)",
    }
}

fn skip_reason_str(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::LiveSessionRow => "live session",
        SkipReason::Interrupted => "interrupted",
        SkipReason::Dirty => "dirty",
        SkipReason::UnpushedCommits => "unpushed commits",
    }
}

fn action_str(action: WorkspaceAction) -> String {
    match action {
        WorkspaceAction::None => "none".to_string(),
        WorkspaceAction::WouldClean { unmerged_with_content: true, .. } => "would clean (unmerged)".to_string(),
        WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false } => {
            "would clean (worktree + branch)".to_string()
        }
        WorkspaceAction::WouldClean { delete_branch: false, unmerged_with_content: false } => {
            "would clean (worktree)".to_string()
        }
        WorkspaceAction::NeedsAttention(reason) => format!("needs attention: {}", skip_reason_str(reason)),
    }
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
        format!("{bytes}B")
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

/// Pure projection — kept separate from `collect()`'s IO so the row shape
/// itself is trivially testable against a fixture `WorkspaceRow`.
pub fn to_row_view(row: &WorkspaceRow) -> WorkspaceRowView {
    WorkspaceRowView {
        path: row.path.display().to_string(),
        session_id: row.session_id.clone().unwrap_or_else(|| "—".to_string()),
        status: status_str(row.status).to_string(),
        size: human_size(row.size_bytes),
        action: action_str(row.action),
        size_bytes: row.size_bytes,
        prunable: workspace::is_default_prune_candidate(row.action),
    }
}

/// Total count and reclaimable bytes among rows the CLI's default prune
/// would act on — what the `p` key's preview line shows. Never touches
/// anything; purely a readout over what `collect()` already gathered.
pub fn prune_preview(rows: &[WorkspaceRowView]) -> (usize, u64) {
    let prunable: Vec<&WorkspaceRowView> = rows.iter().filter(|r| r.prunable).collect();
    (prunable.len(), prunable.iter().map(|r| r.size_bytes).sum())
}

/// Ready-to-render text for the `p` preview — "nothing to prune" is its own
/// phrasing rather than "would prune 0", matching `ninox workspaces
/// prune`'s own CLI wording for the empty case.
pub fn preview_summary_line(rows: &[WorkspaceRowView]) -> String {
    let (count, bytes) = prune_preview(rows);
    if count == 0 {
        "nothing to prune".to_string()
    } else {
        format!("would prune {count} worktree(s), reclaiming {}", human_size(bytes))
    }
}

/// One fixed-width display line for a row, matching `ninox workspaces
/// list`'s columns (name, session, status, size, action), each truncated to
/// its own budget rather than left to the terminal to clip mid-field — so a
/// narrow panel always shows a legible prefix of every column instead of
/// silently losing everything past the name. Percentages apply to the
/// *content* budget (`width` minus the four fixed inter-column gaps), not
/// to `width` itself, and every column is floored at 1 char rather than a
/// larger minimum — so the columns' widths plus gaps can never add up to
/// more than `width`, and the rendered line's length always equals `width`
/// exactly (a larger per-column floor can't make that guarantee: on a
/// genuinely narrow panel, several such floors can individually be fine but
/// still sum past `width` once stacked). Widths: name 24%, session 14%,
/// status 16%, size 20% (right-aligned), action whatever remains. Size's
/// share is larger than its typical content needs (`human_size` never
/// produces more than 8 chars, e.g. "999.9 MB") specifically so it keeps
/// fitting without truncation at realistic panel widths (~50+ columns) —
/// name and action are free text and tolerate truncation far more
/// gracefully than a size reading being silently cut.
pub fn format_row_line(row: &WorkspaceRowView, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let basename = row.path.rsplit('/').next().unwrap_or(&row.path);

    let gap = 2usize;
    let content_budget = width.saturating_sub(gap * 4);
    let name_w = (content_budget * 24 / 100).max(1);
    let session_w = (content_budget * 14 / 100).max(1);
    let status_w = (content_budget * 16 / 100).max(1);
    let size_w = (content_budget * 20 / 100).max(1);
    let used = name_w + gap + session_w + gap + status_w + gap + size_w + gap;
    let action_w = width.saturating_sub(used);

    let name = truncate(basename, name_w);
    let session = truncate(&row.session_id, session_w);
    let status = truncate(&row.status, status_w);
    let size = truncate(&row.size, size_w);
    let action = truncate(&row.action, action_w);

    format!(
        "{name:<name_w$}  {session:<session_w$}  {status:<status_w$}  {size:>size_w$}  {action:<action_w$}",
    )
}

/// Truncates to at most `max` chars, replacing the last character with `…`
/// when it had to cut (so a truncated field never looks like a complete,
/// if short, value). `pub(crate)` so the footer hint line (`view.rs`) can
/// apply the same cutting rule to the preview summary when it doesn't fit.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max == 1 {
        "…".to_string()
    } else {
        let mut out: String = chars[..max - 1].iter().collect();
        out.push('…');
        out
    }
}

/// One row per `.claude/worktrees/*` entry across every repo Ninox has
/// spawned into. `build_workspace_rows` is `async` (it shares its
/// implementation with the orphan sweeper) but never actually suspends —
/// every git call it makes is a blocking subprocess — so this runs it to
/// completion inline via `block_in_place`, the same way `refresh()`'s other
/// store reads are synchronous calls from a non-async context.
pub fn collect(store: &Store, retention_ms: i64) -> Vec<WorkspaceRowView> {
    let roots = workspace::discover_repo_roots(store).unwrap_or_default();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let mut rows = Vec::new();
            for root in &roots {
                if let Ok(repo_rows) = workspace::build_workspace_rows(store, root, retention_ms).await {
                    rows.extend(repo_rows.iter().map(to_row_view));
                }
            }
            rows
        })
    })
}

/// Current retention window, same source `refresh()` already uses
/// elsewhere for the retention sweep.
pub fn retention_millis() -> i64 {
    AppConfig::load_or_default().session_retention.retention_millis()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkspacesPanelState {
    pub open: bool,
    pub selected: usize,
    /// Toggled by `p` — shows the "would prune N, reclaiming X" readout.
    /// Never triggers a real prune; that stays a CLI-only action.
    pub preview: bool,
}

impl WorkspacesPanelState {
    pub fn move_sel(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let max = len - 1;
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, max as i32) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::workspace::WorkspaceFacts;
    use std::path::PathBuf;

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

    #[test]
    fn to_row_view_matches_fixture_data() {
        let row = WorkspaceRow {
            path: PathBuf::from("/repo/.claude/worktrees/w1"),
            session_id: Some("w1".to_string()),
            status: OrphanStatus::Orphan(Ownership::Tracked),
            facts: facts(),
            size_bytes: 2048,
            action: WorkspaceAction::WouldClean { delete_branch: true, unmerged_with_content: false },
        };

        let view = to_row_view(&row);

        assert_eq!(view.path, "/repo/.claude/worktrees/w1");
        assert_eq!(view.session_id, "w1");
        assert_eq!(view.status, "orphan(tracked)");
        assert_eq!(view.size, "2.0KB");
        assert_eq!(view.action, "would clean (worktree + branch)");
        assert_eq!(view.size_bytes, 2048);
        assert!(view.prunable, "merged, no commits beyond base — the default prune bucket");
    }

    #[test]
    fn to_row_view_marks_needs_attention_rows_as_not_prunable() {
        let row = WorkspaceRow {
            path: PathBuf::from("/repo/.claude/worktrees/dirty"),
            session_id: None,
            status: OrphanStatus::Orphan(Ownership::Tracked),
            facts: WorkspaceFacts { dirty: true, ..facts() },
            size_bytes: 10,
            action: WorkspaceAction::NeedsAttention(SkipReason::Dirty),
        };
        assert!(!to_row_view(&row).prunable);
    }

    #[test]
    fn prune_preview_sums_only_prunable_rows() {
        let prunable = WorkspaceRowView {
            path: "/a".into(), session_id: "—".into(), status: "orphan(tracked)".into(),
            size: "1.0KB".into(), action: "would clean (worktree + branch)".into(),
            size_bytes: 1024, prunable: true,
        };
        let not_prunable = WorkspaceRowView {
            path: "/b".into(), session_id: "—".into(), status: "orphan(tracked)".into(),
            size: "2.0KB".into(), action: "needs attention: dirty".into(),
            size_bytes: 2048, prunable: false,
        };
        assert_eq!(prune_preview(&[prunable, not_prunable]), (1, 1024));
        assert_eq!(prune_preview(&[]), (0, 0));
    }

    #[test]
    fn preview_summary_line_reports_nothing_to_prune_when_empty() {
        assert_eq!(preview_summary_line(&[]), "nothing to prune");
    }

    #[test]
    fn preview_summary_line_reports_count_and_reclaimed_size() {
        let prunable = WorkspaceRowView {
            path: "/a".into(), session_id: "—".into(), status: "orphan(tracked)".into(),
            size: "1.0KB".into(), action: "would clean (worktree + branch)".into(),
            size_bytes: 1024, prunable: true,
        };
        assert_eq!(preview_summary_line(&[prunable]), "would prune 1 worktree(s), reclaiming 1.0KB");
    }

    fn row_view_with_long_fields() -> WorkspaceRowView {
        WorkspaceRowView {
            path: "/repo/.claude/worktrees/a-very-long-session-name-indeed".into(),
            session_id: "a-very-long-session-id".into(),
            status: "orphan(legacy)".into(),
            size: "123.4 MB".into(),
            action: "needs attention: unpushed commits".into(),
            size_bytes: 123_400_000,
            prunable: false,
        }
    }

    #[test]
    fn format_row_line_fits_exactly_within_the_requested_width() {
        for width in [20usize, 40, 60, 80, 120] {
            let line = format_row_line(&row_view_with_long_fields(), width);
            assert_eq!(line.chars().count(), width, "width={width}: {line:?}");
        }
    }

    #[test]
    fn format_row_line_is_empty_for_zero_width() {
        assert_eq!(format_row_line(&row_view_with_long_fields(), 0), "");
    }

    #[test]
    fn format_row_line_shows_a_legible_prefix_of_every_column_even_when_narrow() {
        let line = format_row_line(&row_view_with_long_fields(), 30);
        assert!(line.contains('…'), "narrow width must truncate, not panic: {line:?}");
        assert!(!line.trim().is_empty());
    }

    #[test]
    fn truncate_marks_a_cut_field_with_an_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("x", 0), "");
    }

    #[test]
    fn to_row_view_defaults_session_id_to_em_dash_when_absent() {
        let row = WorkspaceRow {
            path: PathBuf::from("/repo/.claude/worktrees/human-dir"),
            session_id: None,
            status: OrphanStatus::NotOurs,
            facts: facts(),
            size_bytes: 0,
            action: WorkspaceAction::None,
        };

        let view = to_row_view(&row);

        assert_eq!(view.session_id, "—");
        assert_eq!(view.action, "none");
    }

    #[test]
    fn move_sel_clamps_at_zero_and_at_the_max() {
        let mut v = WorkspacesPanelState::default();
        v.move_sel(-1, 3);
        assert_eq!(v.selected, 0);
        v.move_sel(2, 3);
        assert_eq!(v.selected, 2);
        v.move_sel(1, 3);
        assert_eq!(v.selected, 2, "must clamp at len - 1");
    }

    #[test]
    fn move_sel_with_zero_rows_stays_at_zero() {
        let mut v = WorkspacesPanelState::default();
        v.move_sel(5, 0);
        assert_eq!(v.selected, 0);
    }
}
