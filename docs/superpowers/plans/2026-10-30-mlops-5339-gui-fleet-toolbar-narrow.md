# MLOPS-5339: Fleet toolbar at narrow widths

At about 745px the Fleet toolbar was one non-wrapping row with a `Fill` spacer, so iced squeezed the
segmented controls until "Compact" rendered one letter per line.

## Tasks

1. **Headless layout test** (`fleet_islands` tests, using the `iced_tiny_skia` renderer pattern from
   `fleet_board`): lay out the toolbar at 420, 700, 1099, 1100 and 1600px. No label may be taller
   than one line, and the toolbar must fit the width. The test fails on the old layout, where a
   label was laid out 150px tall.
2. **Layout**: `toolbar_row(width, ..)`. At `ONE_LINE_TOOLBAR_MIN_WIDTH` (1100px) and above it's the
   designed single row, with controls right-aligned. Below that the controls go on their own
   wrapping line (`Row::wrap`) under the title. The width is the Fleet canvas width,
   `fleet_focus::canvas_width`, factored out of `cards_per_row` so both use the same number.
   `iced::widget::responsive` isn't used because in iced 0.13 it always fills the available height.
3. **Review fixes**: `seg_control`'s buttons wrap inside their bordered group, so a Machine control
   with many remotes doesn't squeeze on a narrow canvas. A Cmd+/- zoom rescales
   `window_width`/`window_height` by old÷new zoom and reflows terminals. iced reports sizes in
   zoomed logical px but sends no resize event on a zoom change, so the canvas width (and
   `cards_per_row`, and terminal sizing) used to go stale.
4. One isolated window capture each at 745px and 1280px.
