//! The Board tab: sessions grouped into six status columns (Working / PR
//! Open / CI Failed / Review / Mergeable / Done), mirroring `ninox-gui`'s
//! `fleet_board.rs` column/fold logic in ratatui terms.

use ninox_core::types::SessionStatus;

use super::state::Row;

pub struct Column {
    pub label: &'static str,
    pub status: SessionStatus,
}

pub const COLUMNS: [Column; 6] = [
    Column { label: "Working", status: SessionStatus::Working },
    Column { label: "PR Open", status: SessionStatus::PrOpen },
    Column { label: "CI Failed", status: SessionStatus::CiFailed },
    Column { label: "Review", status: SessionStatus::ReviewPending },
    Column { label: "Mergeable", status: SessionStatus::Mergeable },
    Column { label: "Done", status: SessionStatus::Done },
];

/// The cards in `column` under the Board's current scope and filter — the
/// GUI board's rules (`ninox_overview::board`).
pub fn column_rows<'r>(rows: &'r [Row], column: SessionStatus, view: &KanbanView) -> Vec<&'r Row> {
    rows.iter()
        .filter(|r| ninox_overview::board::column_absorbs(&column, &r.session.status))
        .filter(|r| ninox_overview::board::on_board(&r.session, r.is_orchestrator, view.scope.as_deref(), &view.query))
        .collect()
}

/// Every card on the Board under its scope and filter, any column.
pub fn shown(rows: &[Row], view: &KanbanView) -> usize {
    COLUMNS.iter().map(|c| column_rows(rows, c.status.clone(), view).len()).sum()
}

/// The orchestrators the scope chips cycle through, in fleet order.
pub fn scopes(rows: &[Row]) -> Vec<(String, String)> {
    rows.iter().filter(|r| r.is_orchestrator).map(|r| (r.id().to_string(), r.session.name.clone())).collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KanbanView {
    pub col: usize,
    pub row: usize,
    /// `/`'s name/repo filter.
    pub query: String,
    pub editing_query: bool,
    /// The orchestrator whose workers are shown (`o` cycles); `None` is all.
    pub scope: Option<String>,
}

impl KanbanView {
    /// `o`: all → each orchestrator in turn → all. A scope whose
    /// orchestrator is gone falls back to all.
    pub fn cycle_scope(&mut self, scopes: &[(String, String)]) {
        let at = self.scope.as_ref().and_then(|s| scopes.iter().position(|(id, _)| id == s));
        self.scope = match at {
            None => scopes.first().map(|(id, _)| id.clone()),
            Some(i) => scopes.get(i + 1).map(|(id, _)| id.clone()),
        };
        self.row = 0;
    }

    pub fn move_row(&mut self, delta: i32, rows_in_col: usize) {
        self.row = clamp_move(self.row, delta, rows_in_col);
    }

    /// Only lands on a column where `populated[i]` is true; stays put if
    /// none exists in that direction (`board_key`'s h/l — every column
    /// move needs to skip empty ones, so there is no plain `move_col`).
    pub fn move_col_among(&mut self, delta: i32, populated: &[bool; 6]) {
        let step = delta.signum();
        if step == 0 {
            return;
        }
        let mut next = self.col as i32;
        loop {
            next += step;
            if next < 0 || next as usize >= populated.len() {
                return;
            }
            if populated[next as usize] {
                self.col = next as usize;
                self.row = 0;
                return;
            }
        }
    }
}

fn clamp_move(current: usize, delta: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    let next = current as i32 + delta;
    next.clamp(0, max as i32) as usize
}

#[cfg(test)]
mod tests {
    use ninox_core::types::SessionStatus;

    use super::super::state::Row;

    fn row_with_status(id: &str, status: SessionStatus) -> Row {
        Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, None, status),
            is_orchestrator: false,
            group: None,
            machine_health: None, queue_position: None,
        }
    }

    #[test]
    fn column_rows_puts_a_ci_failed_row_only_in_the_ci_failed_column() {
        let rows = vec![row_with_status("s1", SessionStatus::CiFailed)];
        assert_eq!(super::column_rows(&rows, SessionStatus::CiFailed, &super::KanbanView::default()).len(), 1);
        assert_eq!(super::column_rows(&rows, SessionStatus::Working, &super::KanbanView::default()).len(), 0);
        assert_eq!(super::column_rows(&rows, SessionStatus::Done, &super::KanbanView::default()).len(), 0);
    }

    #[test]
    fn column_rows_folds_terminated_into_done() {
        let rows = vec![row_with_status("s1", SessionStatus::Terminated)];
        assert_eq!(super::column_rows(&rows, SessionStatus::Done, &super::KanbanView::default()).len(), 1);
        assert_eq!(super::column_rows(&rows, SessionStatus::Working, &super::KanbanView::default()).len(), 0);
    }

    #[test]
    fn column_rows_folds_interrupted_into_working() {
        let rows = vec![row_with_status("s1", SessionStatus::Interrupted)];
        assert_eq!(super::column_rows(&rows, SessionStatus::Working, &super::KanbanView::default()).len(), 1);
        assert_eq!(super::column_rows(&rows, SessionStatus::Done, &super::KanbanView::default()).len(), 0);
    }

    fn row_in(id: &str, orch: Option<&str>, is_orchestrator: bool) -> Row {
        Row {
            session: ninox_ui_shared::cli::test_fixtures::session(id, orch, SessionStatus::Working),
            is_orchestrator,
            group: orch.map(str::to_string),
            machine_health: None, queue_position: None,
        }
    }

    #[test]
    fn scope_and_query_narrow_the_cards_and_orchestrators_are_never_cards() {
        let rows = vec![row_in("o1", Some("o1"), true), row_in("w-a", Some("o1"), false), row_in("w-b", Some("o2"), false)];
        let mut v = super::KanbanView::default();
        let ids = |v: &super::KanbanView| super::column_rows(&rows, SessionStatus::Working, v).iter().map(|r| r.id().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(&v), vec!["w-a", "w-b"], "the orchestrator's own session isn't a card");
        v.scope = Some("o1".into());
        assert_eq!(ids(&v), vec!["w-a"]);
        v.scope = None;
        v.query = "W-B".into();
        assert_eq!(ids(&v), vec!["w-b"]);
        assert_eq!(super::shown(&rows, &v), 1);
    }

    #[test]
    fn o_cycles_all_then_each_orchestrator_then_all() {
        let scopes = vec![("o1".to_string(), "one".to_string()), ("o2".to_string(), "two".to_string())];
        let mut v = super::KanbanView { row: 3, ..Default::default() };
        v.cycle_scope(&scopes);
        assert_eq!((v.scope.as_deref(), v.row), (Some("o1"), 0));
        v.cycle_scope(&scopes);
        assert_eq!(v.scope.as_deref(), Some("o2"));
        v.cycle_scope(&scopes);
        assert_eq!(v.scope, None);
        v.scope = Some("gone".into());
        v.cycle_scope(&scopes);
        assert_eq!(v.scope.as_deref(), Some("o1"), "a vanished scope restarts the cycle");
    }

    #[test]
    fn move_row_clamps_at_zero_and_at_the_max() {
        let mut v = super::KanbanView::default();
        v.move_row(-1, 3);
        assert_eq!(v.row, 0);
        v.move_row(2, 3);
        assert_eq!(v.row, 2);
        v.move_row(1, 3);
        assert_eq!(v.row, 2, "must clamp at rows_in_col - 1");
    }

    #[test]
    fn move_col_among_skips_empty_columns() {
        let mut v = super::KanbanView::default();
        // Working(0) populated, PR Open(1) empty, CI Failed(2) populated.
        let populated = [true, false, true, false, false, false];
        v.move_col_among(1, &populated);
        assert_eq!(v.col, 2, "column 1 is empty, so h/l lands on the next populated one");
        v.move_col_among(-1, &populated);
        assert_eq!(v.col, 0);
        v.move_col_among(-1, &populated);
        assert_eq!(v.col, 0, "no populated column to the left — stays put");
    }
}
