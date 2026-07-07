# Clickable Terminal Links Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make URLs in the terminal view clickable — both OSC 8 hyperlinks emitted by programs, and bare `http(s)://` URLs in plain text.

**Architecture:** A new pure-logic module (`components/links.rs`) scans a row of cells for clickable spans (OSC 8 runs + bare-URL regex-free scan) and returns `LinkSpan { start_col, end_col, url }`. `StyledCell` (tmux scrollback cache) gains a `hyperlink` field so OSC 8 data survives `capture-pane` round-trips. `TerminalWidget` (the `iced::canvas::Program` that renders and hit-tests the terminal) calls into `links` per visible row to: underline link spans while drawing, switch to a pointer cursor on hover (`mouse_interaction`), and emit the existing `Message::OpenUrl` on click (checked before the existing session-ID click-to-navigate).

**Tech Stack:** Rust, `alacritty_terminal` 0.26 (already parses OSC 8 into `Cell::hyperlink()`), `iced` 0.13 canvas `Program` trait.

## Global Constraints

- No new crate dependencies — the bare-URL scan is hand-rolled (no `regex` crate in this workspace today).
- Follow existing code style: 4-space indent within aligned struct-literal blocks matches surrounding file conventions; doc comments explain *why*, not *what*.
- Every task must leave `cargo test -p ninox` (or the workspace equivalent) green before moving to the next task.

---

## File Map

- **Create:** `crates/ninox-app/src/components/links.rs` — pure link-span detection, no UI/alacritty dependency beyond a small `LinkCell` view struct.
- **Modify:** `crates/ninox-app/src/components/mod.rs` — register the new module.
- **Modify:** `crates/ninox-app/src/components/scrollback.rs` — add `hyperlink: Option<String>` to `StyledCell`, populate it in `parse_capture`.
- **Modify:** `crates/ninox-app/src/components/terminal.rs` — wire `links` into `TerminalWidget`'s `draw()` (underline), `update()` (hover + click), and add `mouse_interaction()`.

---

### Task 1: `links` module — pure link-span detection

**Files:**
- Create: `crates/ninox-app/src/components/links.rs`
- Modify: `crates/ninox-app/src/components/mod.rs:8` (insert `pub mod links;` after `pub mod inspector_panel;`)

**Interfaces:**
- Produces (consumed by Tasks 2 & 3):
  - `pub struct LinkSpan { pub start_col: usize, pub end_col: usize, pub url: String }` (`end_col` inclusive)
  - `pub struct LinkCell<'a> { pub c: char, pub hyperlink: Option<&'a str> }`
  - `pub fn find_links(row: &[LinkCell]) -> Vec<LinkSpan>`
  - `pub fn link_at(row: &[LinkCell], col: usize) -> Option<String>`

- [ ] **Step 1: Write the failing tests**

Create `crates/ninox-app/src/components/links.rs` with just the types and a `#[cfg(test)]` module (no implementation yet, so the tests fail to compile / fail on `todo!()`):

```rust
//! Clickable-link detection for terminal rows: OSC 8 hyperlinks emitted by
//! the running program, plus a fallback scan for bare `http(s)://` URLs in
//! plain text (most CLI tools never bother emitting OSC 8).

/// One clickable span within a single terminal row.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSpan {
    pub start_col: usize,
    pub end_col: usize, // inclusive
    pub url: String,
}

/// One cell's rendered character plus its OSC 8 hyperlink URI, if any. The
/// minimal view `find_links` needs — both the live alacritty grid and the
/// cached tmux scrollback can build a row of these without either depending
/// on the other's cell type.
#[derive(Clone, Copy)]
pub struct LinkCell<'a> {
    pub c: char,
    pub hyperlink: Option<&'a str>,
}

pub fn find_links(_row: &[LinkCell]) -> Vec<LinkSpan> {
    todo!()
}

pub fn link_at(row: &[LinkCell], col: usize) -> Option<String> {
    find_links(row).into_iter().find(|s| col >= s.start_col && col <= s.end_col).map(|s| s.url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_from(text: &str) -> Vec<LinkCell<'static>> {
        text.chars().map(|c| LinkCell { c, hyperlink: None }).collect()
    }

    #[test]
    fn finds_bare_url_in_plain_text() {
        let row = row_from("see http://example.com/path for docs");
        let spans = find_links(&row);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "http://example.com/path");
        assert_eq!(spans[0].start_col, 4);
        assert_eq!(spans[0].end_col, 4 + "http://example.com/path".len() - 1);
    }

    #[test]
    fn trims_trailing_sentence_punctuation() {
        let row = row_from("visit https://example.com/x.");
        let spans = find_links(&row);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/x");
    }

    #[test]
    fn no_url_in_plain_text_returns_no_spans() {
        let row = row_from("nothing clickable here");
        assert!(find_links(&row).is_empty());
    }

    #[test]
    fn finds_osc8_hyperlink_span() {
        let mut row = row_from("click me");
        for cell in &mut row {
            cell.hyperlink = Some("http://example.com");
        }
        let spans = find_links(&row);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "http://example.com");
        assert_eq!(spans[0].start_col, 0);
        assert_eq!(spans[0].end_col, 7);
    }

    #[test]
    fn osc8_span_does_not_duplicate_as_bare_url() {
        let mut row = row_from("http://example.com");
        for cell in &mut row {
            cell.hyperlink = Some("http://example.com");
        }
        assert_eq!(find_links(&row).len(), 1);
    }

    #[test]
    fn link_at_finds_url_under_column() {
        let row = row_from("see http://example.com here");
        assert_eq!(link_at(&row, 5), Some("http://example.com".to_string()));
        assert_eq!(link_at(&row, 0), None);
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/ninox-app/src/components/mod.rs`, insert alphabetically:

```rust
pub mod inspector_panel;
pub mod links;
pub mod notification_panel;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ninox links::`
Expected: FAIL (panics on `todo!()`).

- [ ] **Step 4: Implement `find_links`**

Replace the `todo!()` body in `links.rs` with:

```rust
/// Find every clickable span in one row: contiguous same-URI OSC 8 runs
/// first, then a fallback scan for bare `http(s)://` URLs over the
/// remaining text.
pub fn find_links(row: &[LinkCell]) -> Vec<LinkSpan> {
    let mut spans = Vec::new();
    let mut col = 0;
    while col < row.len() {
        if let Some(uri) = row[col].hyperlink {
            let start = col;
            while col < row.len() && row[col].hyperlink == Some(uri) {
                col += 1;
            }
            spans.push(LinkSpan { start_col: start, end_col: col - 1, url: uri.to_string() });
        } else {
            col += 1;
        }
    }

    let text: String = row.iter().map(|cell| if cell.c == '\0' { ' ' } else { cell.c }).collect();
    for (start_col, url) in find_bare_urls(&text) {
        let end_col = start_col + url.chars().count() - 1;
        let overlaps = spans.iter().any(|s| start_col <= s.end_col && end_col >= s.start_col);
        if !overlaps {
            spans.push(LinkSpan { start_col, end_col, url });
        }
    }

    spans
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Scan plain text for bare `http://`/`https://` URLs, trimming trailing
/// punctuation that's almost always sentence/bracket noise rather than part
/// of the link itself (e.g. a URL at the end of a sentence followed by '.').
fn find_bare_urls(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let scheme_len = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            0
        };
        if scheme_len == 0 {
            i += 1;
            continue;
        }
        let mut end = i + scheme_len;
        while end < chars.len() && is_url_char(chars[end]) {
            end += 1;
        }
        while end > i + scheme_len
            && matches!(
                chars[end - 1],
                '.' | ',' | ')' | ']' | '>' | '"' | '\'' | ';' | ':' | '!' | '?'
            )
        {
            end -= 1;
        }
        if end > i + scheme_len {
            out.push((i, chars[i..end].iter().collect()));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox links::`
Expected: PASS (6 tests).

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src/components/links.rs crates/ninox-app/src/components/mod.rs
git commit -m "feat(terminal): add link-span detection (OSC 8 + bare URL scan)"
```

---

### Task 2: Thread OSC 8 hyperlinks through `StyledCell` / scrollback

**Files:**
- Modify: `crates/ninox-app/src/components/scrollback.rs:11-17` (`StyledCell` struct), `:52-59` (`parse_capture`'s cell mapping), `:176-181` (`line_above_indexes_newest_first` test's `mk` closure)
- Modify: `crates/ninox-app/src/components/terminal.rs:764-771` (`extract_selection_reads_history_lines_when_scrolled_back` test's `mk_line` closure)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces (consumed by Task 3): `StyledCell.hyperlink: Option<String>` — `None` when the cell has no OSC 8 hyperlink, `Some(uri)` otherwise.

- [ ] **Step 1: Write the failing test**

In `crates/ninox-app/src/components/scrollback.rs`, add to the `tests` module:

```rust
#[test]
fn parse_capture_preserves_hyperlink() {
    let bytes = b"\x1b]8;;http://example.com\x1b\\click me\x1b]8;;\x1b\\\n";
    let lines = parse_capture(bytes, 40);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0][0].hyperlink.as_deref(), Some("http://example.com"));
    assert_eq!(lines[0][7].hyperlink.as_deref(), Some("http://example.com"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p ninox parse_capture_preserves_hyperlink`
Expected: FAIL with a compile error (`StyledCell` has no field `hyperlink`).

- [ ] **Step 3: Add the field and populate it**

In `crates/ninox-app/src/components/scrollback.rs`, change the struct:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct StyledCell {
    pub c:         char,
    pub fg:        Color,
    pub bg:        Color,
    pub flags:     Flags,
    pub hyperlink: Option<String>,
}
```

And update the cell mapping inside `parse_capture`:

```rust
let mut cells: StyledLine = (0..grid.columns())
    .map(|col| {
        let cell = &grid[line][Column(col)];
        StyledCell {
            c:         cell.c,
            fg:        cell.fg,
            bg:        cell.bg,
            flags:     cell.flags,
            hyperlink: cell.hyperlink().map(|h| h.uri().to_string()),
        }
    })
    .collect();
```

- [ ] **Step 4: Fix the other `StyledCell` construction sites**

In `crates/ninox-app/src/components/scrollback.rs`, the `mk` closure inside `line_above_indexes_newest_first`:

```rust
let mk = |ch: char| vec![StyledCell {
    c: ch,
    fg: alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
    bg: alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
    flags: alacritty_terminal::term::cell::Flags::empty(),
    hyperlink: None,
}];
```

In `crates/ninox-app/src/components/terminal.rs`, the `mk_line` closure inside `extract_selection_reads_history_lines_when_scrolled_back`:

```rust
let mk_line = |text: &str| -> Vec<StyledCell> {
    text.chars().map(|c| StyledCell {
        c,
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: Flags::empty(),
        hyperlink: None,
    }).collect()
};
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox -- scrollback:: extract_selection_reads_history_lines_when_scrolled_back`
Expected: PASS, including the new `parse_capture_preserves_hyperlink` test.

- [ ] **Step 6: Run the full test suite to check nothing else broke**

Run: `cargo test -p ninox`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-app/src/components/scrollback.rs crates/ninox-app/src/components/terminal.rs
git commit -m "feat(terminal): preserve OSC 8 hyperlinks through tmux scrollback capture"
```

---

### Task 3: Wire hover cursor, underline rendering, and click-to-open into `TerminalWidget`

**Files:**
- Modify: `crates/ninox-app/src/components/terminal.rs`:
  - `SelectionState` struct (~line 233-242): add `hovering_link` field
  - New inherent `impl<'a> TerminalWidget<'a>` block (insert after the `SelectionState` `impl` block, ~line 261, before the `Program` impl): add `row_link_spans` and `link_at`
  - `Program::update` (~line 284-381): hover detection + click-to-open
  - `Program::draw` (~line 383-473): pass `is_link` into `draw_cell`
  - `Program` impl: add `mouse_interaction`
  - `draw_cell` (~line 499-611): new `is_link` parameter, OR'd into the underline condition
  - `tests` module: new tests

**Interfaces:**
- Consumes: `crate::components::links::{find_links, link_at, LinkCell, LinkSpan}` (Task 1), `StyledCell.hyperlink` (Task 2).
- Produces: nothing further downstream — this is the leaf of the chain.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module at the bottom of `crates/ninox-app/src/components/terminal.rs`:

```rust
fn test_widget<'a>(state: &'a TerminalState) -> TerminalWidget<'a> {
    TerminalWidget {
        state,
        session_id:   String::new(),
        font_size:    FONT_SIZE,
        terminal_bg:  IcedColor::BLACK,
        terminal_fg:  IcedColor::WHITE,
        cursor_color: IcedColor::WHITE,
        ansi:         [IcedColor::BLACK; 16],
        session_ids:  vec![],
    }
}

#[test]
fn row_link_spans_detects_osc8_hyperlink_on_live_grid() {
    let mut s = TerminalState::new(80, 5, None);
    s.process(b"\x1b]8;;http://example.com\x1b\\click me\x1b]8;;\x1b\\");
    let widget = test_widget(&s);
    let spans = widget.row_link_spans(0);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].url, "http://example.com");
    assert_eq!(spans[0].start_col, 0);
    assert_eq!(spans[0].end_col, 7);
}

#[test]
fn row_link_spans_detects_bare_url_on_live_grid() {
    let mut s = TerminalState::new(80, 5, None);
    s.process(b"see http://example.com/path for docs");
    let widget = test_widget(&s);
    let spans = widget.row_link_spans(0);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].url, "http://example.com/path");
}

#[test]
fn link_at_returns_none_outside_any_span() {
    let mut s = TerminalState::new(80, 5, None);
    s.process(b"see http://example.com/path for docs");
    let widget = test_widget(&s);
    assert_eq!(widget.link_at(0, 0), None); // inside "see "
    assert_eq!(widget.link_at(5, 0), Some("http://example.com/path".to_string()));
}

#[test]
fn hovering_a_link_sets_pointer_cursor() {
    use iced::widget::canvas::Program;

    let mut s = TerminalState::new(80, 5, None);
    s.process(b"see http://example.com/path for docs");
    let widget = test_widget(&s);
    let (cell_w, cell_h) = cell_size(FONT_SIZE);
    let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(80.0 * cell_w, 5.0 * cell_h));
    let mut state = SelectionState::default();

    // Column 5 sits inside "http://example.com/path".
    let hover_pos = iced::Point::new(5.0 * cell_w + 1.0, 0.5 * cell_h);
    widget.update(
        &mut state,
        iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: hover_pos }),
        bounds,
        iced::mouse::Cursor::Available(hover_pos),
    );
    assert_eq!(
        widget.mouse_interaction(&state, bounds, iced::mouse::Cursor::Available(hover_pos)),
        iced::mouse::Interaction::Pointer
    );

    // Column 0 ("s" of "see") is not a link.
    let no_link_pos = iced::Point::new(0.5 * cell_w, 0.5 * cell_h);
    widget.update(
        &mut state,
        iced::widget::canvas::Event::Mouse(iced::mouse::Event::CursorMoved { position: no_link_pos }),
        bounds,
        iced::mouse::Cursor::Available(no_link_pos),
    );
    assert_eq!(
        widget.mouse_interaction(&state, bounds, iced::mouse::Cursor::Available(no_link_pos)),
        iced::mouse::Interaction::default()
    );
}

#[test]
fn clicking_a_link_emits_open_url() {
    use iced::mouse::Button;
    use iced::widget::canvas::Program;

    let mut s = TerminalState::new(80, 5, None);
    s.process(b"see http://example.com/path for docs");
    let widget = test_widget(&s);
    let (cell_w, cell_h) = cell_size(FONT_SIZE);
    let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(80.0 * cell_w, 5.0 * cell_h));
    let mut state = SelectionState::default();
    let pos = iced::Point::new(5.0 * cell_w + 1.0, 0.5 * cell_h);
    let cursor = iced::mouse::Cursor::Available(pos);

    widget.update(
        &mut state,
        iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonPressed(Button::Left)),
        bounds,
        cursor,
    );
    let (_, message) = widget.update(
        &mut state,
        iced::widget::canvas::Event::Mouse(iced::mouse::Event::ButtonReleased(Button::Left)),
        bounds,
        cursor,
    );
    match message {
        Some(Message::OpenUrl(url)) => assert_eq!(url, "http://example.com/path"),
        other => panic!("expected OpenUrl, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox terminal::`
Expected: FAIL with compile errors — `row_link_spans`/`link_at`/`mouse_interaction`/`hovering_link` don't exist yet.

- [ ] **Step 3: Add `hovering_link` to `SelectionState`**

```rust
#[derive(Default, Clone)]
pub struct SelectionState {
    /// Anchor cell (col, row) where the drag started.
    anchor: Option<(usize, usize)>,
    /// Current end cell while dragging.
    end: Option<(usize, usize)>,
    dragging: bool,
    /// Whether the cursor moved after the press (distinguishes click from drag).
    moved: bool,
    /// Whether the cell under the cursor is currently part of a clickable
    /// link — drives the pointer cursor via `mouse_interaction`.
    hovering_link: bool,
}
```

- [ ] **Step 4: Add `row_link_spans` and `link_at` to `TerminalWidget`**

Insert a new `impl` block right after the `SelectionState` `impl` block (before `// TerminalWidget — iced canvas Program`):

```rust
impl<'a> TerminalWidget<'a> {
    /// Every clickable link span in viewport row `row`, from OSC 8
    /// hyperlinks (live grid or cached history) or a bare-URL fallback scan.
    fn row_link_spans(&self, row: usize) -> Vec<crate::components::links::LinkSpan> {
        use crate::components::links::{find_links, LinkCell};

        let grid = self.state.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        if row >= rows {
            return Vec::new();
        }
        let offset = self.state.scrollback.offset as i32;
        let logical = row as i32 - offset;

        if logical < 0 {
            let Some(cells) = self.state.scrollback.line_above((-logical - 1) as usize) else {
                return Vec::new();
            };
            let link_cells: Vec<LinkCell> = cells
                .iter()
                .map(|c| LinkCell { c: c.c, hyperlink: c.hyperlink.as_deref() })
                .collect();
            find_links(&link_cells)
        } else {
            use alacritty_terminal::index::{Column, Line};
            let line = Line(logical);
            let hyperlinks: Vec<Option<alacritty_terminal::term::cell::Hyperlink>> =
                (0..cols).map(|c| grid[line][Column(c)].hyperlink()).collect();
            let link_cells: Vec<LinkCell> = (0..cols)
                .map(|c| LinkCell {
                    c: grid[line][Column(c)].c,
                    hyperlink: hyperlinks[c].as_ref().map(|h| h.uri()),
                })
                .collect();
            find_links(&link_cells)
        }
    }

    /// The URL under viewport cell (col, row), if any.
    fn link_at(&self, col: usize, row: usize) -> Option<String> {
        self.row_link_spans(row)
            .into_iter()
            .find(|s| col >= s.start_col && col <= s.end_col)
            .map(|s| s.url)
    }
}
```

- [ ] **Step 5: Hover detection + click-to-open in `update()`**

Replace the existing `CursorMoved` arm:

```rust
Event::Mouse(MouseEvent::CursorMoved { .. }) if state.dragging => {
    if let Some(pos) = cursor.position_in(bounds) {
        let cell = SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
        if state.anchor != Some(cell) {
            state.moved = true;
        }
        state.end = Some(cell);
        self.state.cache.clear();
    }
    return (iced::widget::canvas::event::Status::Captured, None);
}
```

with:

```rust
Event::Mouse(MouseEvent::CursorMoved { .. }) => {
    // Hover-link detection runs regardless of drag state, so the pointer
    // cursor still reflects the cell under the mouse mid-drag.
    let hovering = cursor
        .position_in(bounds)
        .map(|pos| SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows))
        .is_some_and(|(col, row)| self.link_at(col, row).is_some());
    state.hovering_link = hovering;

    if state.dragging {
        if let Some(pos) = cursor.position_in(bounds) {
            let cell = SelectionState::pixel_to_cell(pos.x, pos.y, cell_w, cell_h, cols, rows);
            if state.anchor != Some(cell) {
                state.moved = true;
            }
            state.end = Some(cell);
            self.state.cache.clear();
        }
        return (iced::widget::canvas::event::Status::Captured, None);
    }
    return (iced::widget::canvas::event::Status::Ignored, None);
}

Event::Mouse(MouseEvent::CursorLeft) => {
    state.hovering_link = false;
    return (iced::widget::canvas::event::Status::Ignored, None);
}
```

Then, in the `ButtonReleased(Button::Left) if state.dragging` arm, add the link check before the session-ID check:

```rust
Event::Mouse(MouseEvent::ButtonReleased(Button::Left)) if state.dragging => {
    state.dragging = false;
    if !state.moved {
        // Single click — a link takes priority over session-ID navigation.
        if let (Some((col, row)), Some(pos)) = (state.anchor, cursor.position_in(bounds)) {
            let _ = pos; // bounds-checked via anchor
            if let Some(url) = self.link_at(col, row) {
                state.anchor = None;
                state.end    = None;
                return (iced::widget::canvas::event::Status::Captured,
                        Some(Message::OpenUrl(url)));
            }
            let word = word_at(self.state.term.grid(), col, row);
            if self.session_ids.iter().any(|id| id == &word) {
                state.anchor = None;
                state.end    = None;
                return (iced::widget::canvas::event::Status::Captured,
                        Some(Message::NavigateSession(word)));
            }
        }
        state.anchor = None;
        state.end    = None;
        return (iced::widget::canvas::event::Status::Captured, None);
    }
    // (rest of the drag/copy branch is unchanged)
    ...
```

(Leave the trailing drag/copy logic in that arm exactly as it is today — only the block above changes.)

- [ ] **Step 6: Add `mouse_interaction`**

In the `impl<'a> iced::widget::canvas::Program<Message> for TerminalWidget<'a>` block, add after `draw()` (before the closing `}` of the impl):

```rust
fn mouse_interaction(
    &self,
    state: &Self::State,
    _bounds: Rectangle,
    _cursor: iced::mouse::Cursor,
) -> iced::mouse::Interaction {
    if state.hovering_link {
        iced::mouse::Interaction::Pointer
    } else {
        iced::mouse::Interaction::default()
    }
}
```

- [ ] **Step 7: Underline detected links in `draw()`**

In the history branch, compute spans once per row and pass `is_link` per cell:

```rust
if logical < 0 {
    let Some(cells) = self.state.scrollback.line_above((-logical - 1) as usize)
    else {
        continue;
    };
    let link_spans = self.row_link_spans(row);
    for (col, cell) in cells.iter().enumerate().take(cols) {
        let x = col as f32 * cell_w;
        let is_selected = cell_is_selected(sel, row, col);
        let is_link = link_spans.iter().any(|s| col >= s.start_col && col <= s.end_col);
        draw_cell(
            frame, x, y, cell_w, cell_h, self.font_size,
            cell.c, cell.fg, cell.bg, cell.flags,
            false, // cursor never draws in history
            cursor_shape, is_selected, is_link,
            colors, &self.ansi, term_bg, term_fg, cursor_color,
        );
    }
    continue;
}
```

And in the live-grid branch:

```rust
let line = Line(logical);
let link_spans = self.row_link_spans(row);
for col in 0..cols {
    let column = Column(col);
    let cell = &grid[line][column];
    let x = col as f32 * cell_w;

    let is_cursor =
        offset == 0 && cursor_point.line == line && cursor_point.column == column;
    let is_selected = cell_is_selected(sel, row, col);
    let is_link = link_spans.iter().any(|s| col >= s.start_col && col <= s.end_col);

    draw_cell(
        frame, x, y, cell_w, cell_h, self.font_size,
        cell.c, cell.fg, cell.bg, cell.flags,
        is_cursor, cursor_shape, is_selected, is_link,
        colors, &self.ansi, term_bg, term_fg, cursor_color,
    );
}
```

- [ ] **Step 8: Add the `is_link` parameter to `draw_cell`**

Change the signature (insert after `is_selected: bool,`):

```rust
#[allow(clippy::too_many_arguments)]
fn draw_cell(
    frame: &mut Frame,
    x: f32,
    y: f32,
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    c: char,
    fg_color: Color,
    bg_color: Color,
    flags: Flags,
    is_cursor: bool,
    cursor_shape: CursorShape,
    is_selected: bool,
    is_link: bool,
    colors: &alacritty_terminal::term::color::Colors,
    ansi: &[IcedColor; 16],
    term_bg: IcedColor,
    term_fg: IcedColor,
    cursor_color: IcedColor,
) {
```

And change the underline condition near the bottom of the function:

```rust
if flags.contains(Flags::UNDERLINE) || is_link {
    stroke_line(frame, x, baseline, x + cell_w, baseline, fg);
}
```

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p ninox terminal::`
Expected: PASS (all new tests + previously-passing tests still green).

- [ ] **Step 10: Run the full test suite**

Run: `cargo test -p ninox`
Expected: PASS.

- [ ] **Step 11: Commit**

```bash
git add crates/ninox-app/src/components/terminal.rs
git commit -m "feat(terminal): make OSC 8 and bare-URL links clickable"
```

---

## Manual Verification (after Task 3)

Automated tests cover span detection, hover-cursor state, and the click → `Message::OpenUrl` wiring, but not the actual GPU-rendered underline or that `open_url_program()` really launches a browser. After Task 3 is committed:

1. Use the `verify` skill (or `run` skill) to launch the app against a real session.
2. In that session's terminal, run something that prints a bare URL (e.g. `echo https://github.com`) and confirm it renders underlined and the cursor becomes a pointer on hover.
3. If a program on hand emits real OSC 8 (e.g. `printf '\e]8;;https://example.com\e\\click me\e]8;;\e\\\n'`), confirm the same for that link's text specifically (not the whole line).
4. Click the link and confirm the system browser opens the URL.
5. Scroll the link into tmux scrollback history and repeat steps 2-4 there, to confirm the scrollback path (`StyledCell.hyperlink`) works too.
