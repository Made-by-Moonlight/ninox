//! Visual-only terminal line layout.
//!
//! The terminal emulator remains authoritative in logical cell order. This
//! module applies Unicode Bidirectional Algorithm levels to display clusters
//! and keeps both directions of the cell mapping for rendering and input.

use unicode_bidi::{BidiInfo, Level};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalCell {
    pub column: usize,
    /// Base character followed by any zero-width characters stored in the cell.
    pub text:   String,
    /// Number of terminal columns occupied by this cluster.
    pub width:  usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphRun {
    pub visual_start: usize,
    pub visual_width: usize,
    /// Logical text; the shaping engine uses `rtl` to produce visual glyph order.
    pub text:         String,
    pub rtl:          bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualLine {
    /// `visual_to_logical[visual_column] = logical_column`.
    pub visual_to_logical: Vec<usize>,
    /// `logical_to_visual[logical_column] = visual_column`.
    pub logical_to_visual: Vec<usize>,
    /// Directional runs in left-to-right visual order.
    pub runs:             Vec<GlyphRun>,
}

impl VisualLine {
    pub fn identity(columns: usize) -> Self {
        let columns: Vec<_> = (0..columns).collect();
        Self {
            visual_to_logical: columns.clone(),
            logical_to_visual: columns,
            runs: Vec::new(),
        }
    }
}

/// Apply UAX #9 to one physical terminal row without mutating its logical cells.
///
/// Terminal rows are independently addressable, so each physical row is an
/// independent BiDi paragraph. Trailing blank padding is left in place instead
/// of becoming part of the paragraph.
pub fn layout_line(cells: &[LogicalCell], columns: usize) -> VisualLine {
    let Some(active_len) = cells
        .iter()
        .rposition(|cell| cell.text.chars().any(|character| character != ' ' && character != '\0'))
        .map(|index| index + 1)
    else {
        return VisualLine::identity(columns);
    };
    let active = &cells[..active_len];

    let mut text = String::new();
    let mut byte_starts = Vec::with_capacity(active.len());
    for cell in active {
        byte_starts.push(text.len());
        text.push_str(&cell.text);
    }

    let bidi = BidiInfo::new(&text, None);
    let paragraph = &bidi.paragraphs[0];
    let reordered = bidi.reordered_levels(paragraph, paragraph.range.clone());
    let levels: Vec<Level> = byte_starts.iter().map(|&start| reordered[start]).collect();
    let visual_order = BidiInfo::reorder_visual(&levels);

    let mut line = VisualLine::identity(columns);
    let mut cluster_visual_starts = vec![0; active.len()];
    let mut cluster_visual_widths = vec![0; active.len()];
    let mut visual_column = 0;
    for &cluster_index in &visual_order {
        let cell = &active[cluster_index];
        let width = cell.width.max(1).min(columns.saturating_sub(visual_column));
        cluster_visual_starts[cluster_index] = visual_column;
        cluster_visual_widths[cluster_index] = width;
        for offset in 0..width {
            let logical = cell.column + offset;
            let visual = visual_column + offset;
            if logical < columns && visual < columns {
                line.visual_to_logical[visual] = logical;
                line.logical_to_visual[logical] = visual;
            }
        }
        visual_column += width;
    }

    let mut run_start = 0;
    while run_start < visual_order.len() {
        let level = levels[visual_order[run_start]];
        let rtl = level.is_rtl();
        let mut run_end = run_start + 1;
        while run_end < visual_order.len() {
            let previous = visual_order[run_end - 1];
            let next = visual_order[run_end];
            let contiguous = if rtl { previous == next + 1 } else { next == previous + 1 };
            if levels[next] != level || !contiguous {
                break;
            }
            run_end += 1;
        }

        let run_clusters = &visual_order[run_start..run_end];
        let logical_start = *run_clusters.iter().min().unwrap();
        let logical_end = *run_clusters.iter().max().unwrap();
        let run_text = active[logical_start..=logical_end]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        let visual_width = run_clusters
            .iter()
            .map(|&index| cluster_visual_widths[index])
            .sum();
        let visual_start = run_clusters
            .iter()
            .map(|&index| cluster_visual_starts[index])
            .min()
            .unwrap();
        line.runs.push(GlyphRun {
            visual_start,
            visual_width,
            text: run_text,
            rtl,
        });
        run_start = run_end;
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(text: &str) -> Vec<LogicalCell> {
        text.chars()
            .enumerate()
            .map(|(column, character)| LogicalCell {
                column,
                text: character.to_string(),
                width: 1,
            })
            .collect()
    }

    fn visual_text(logical: &str, layout: &VisualLine) -> String {
        let logical: Vec<_> = logical.chars().collect();
        layout.visual_to_logical[..logical.len()]
            .iter()
            .map(|&column| logical[column])
            .collect()
    }

    #[test]
    fn arabic_and_hebrew_are_reversed_visually_but_remain_logical_in_runs() {
        for logical in ["مرحبا", "שלום"] {
            let layout = layout_line(&cells(logical), logical.chars().count());
            assert_eq!(visual_text(logical, &layout), logical.chars().rev().collect::<String>());
            assert_eq!(layout.runs.len(), 1);
            assert!(layout.runs[0].rtl);
            assert_eq!(layout.runs[0].text, logical);
        }
    }

    #[test]
    fn mixed_scripts_numbers_and_punctuation_follow_uax9() {
        let logical = "English مرحبا 123, שלום!";
        let layout = layout_line(&cells(logical), logical.chars().count());
        assert_eq!(visual_text(logical, &layout), "English םולש ,123 ابحرم!");
        assert_eq!(
            layout.visual_to_logical
                .iter()
                .enumerate()
                .map(|(visual, &logical)| layout.logical_to_visual[logical] == visual)
                .filter(|reversible| *reversible)
                .count(),
            logical.chars().count()
        );
    }

    #[test]
    fn combining_text_and_explicit_bidi_controls_are_not_discarded() {
        let logical = vec![
            LogicalCell { column: 0, text: "A\u{2067}".into(), width: 1 },
            LogicalCell { column: 1, text: "ש\u{05b8}".into(), width: 1 },
            LogicalCell { column: 2, text: "ל".into(), width: 1 },
            LogicalCell { column: 3, text: "ו".into(), width: 1 },
            LogicalCell { column: 4, text: "ם\u{2069}".into(), width: 1 },
        ];
        let layout = layout_line(&logical, 5);
        let shaped_text: String = layout.runs.iter().map(|run| run.text.as_str()).collect();
        assert!(shaped_text.contains('\u{2067}'));
        assert!(shaped_text.contains('\u{2069}'));
        assert!(shaped_text.contains('\u{05b8}'));
    }

    #[test]
    fn wide_clusters_keep_two_reversible_terminal_cells() {
        let logical = vec![
            LogicalCell { column: 0, text: "ש".into(), width: 1 },
            LogicalCell { column: 1, text: "界".into(), width: 2 },
            LogicalCell { column: 3, text: "ם".into(), width: 1 },
        ];
        let layout = layout_line(&logical, 4);
        for logical in 0..4 {
            let visual = layout.logical_to_visual[logical];
            assert_eq!(layout.visual_to_logical[visual], logical);
        }
        assert_eq!(layout.logical_to_visual[1].abs_diff(layout.logical_to_visual[2]), 1);
        assert!(
            layout
                .runs
                .iter()
                .all(|run| run.visual_start + run.visual_width <= 4)
        );
    }

    #[test]
    fn rtl_wide_cluster_moves_as_a_unit_without_flipping_its_cells() {
        let logical = vec![
            LogicalCell { column: 0, text: "ش".into(), width: 1 },
            LogicalCell { column: 1, text: "🙂".into(), width: 2 },
        ];
        let layout = layout_line(&logical, 3);
        assert_eq!(layout.logical_to_visual[2], layout.logical_to_visual[1] + 1);
    }

    #[test]
    fn truncated_wide_cluster_cannot_overstate_visual_run_width() {
        let logical = vec![LogicalCell { column: 0, text: "ش".into(), width: 2 }];
        let layout = layout_line(&logical, 1);
        assert!(
            layout
                .runs
                .iter()
                .all(|run| run.visual_start + run.visual_width <= 1)
        );
    }

    #[test]
    fn trailing_grid_padding_stays_at_its_logical_columns() {
        let mut logical = cells("שלום");
        logical.extend((4..10).map(|column| LogicalCell {
            column,
            text: " ".into(),
            width: 1,
        }));
        let layout = layout_line(&logical, 10);
        assert_eq!(&layout.visual_to_logical[4..], &[4, 5, 6, 7, 8, 9]);
    }
}
