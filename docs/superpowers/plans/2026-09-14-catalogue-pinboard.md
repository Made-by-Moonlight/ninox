# Catalogue/Pinboard: shared sidebar, cross-highlight, force-directed pinboard — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify the catalogue and pinboard brain-view sidebars into one shared component, wire hover/selection cross-highlighting both directions between them, and replace the pinboard's static hash-scatter with a live, draggable force-directed layout.

**Architecture:** All changes live in `crates/ninox-app/src/components/brain_panel.rs` (sidebar unification + cross-highlight), a new pure `crates/ninox-app/src/components/force_layout.rs` (the physics, no `iced` dependency, fully unit-testable in isolation), and `crates/ninox-app/src/components/brain_pinboard.rs` (canvas reads/writes the layout, click-vs-drag input handling) plus `crates/ninox-app/src/app.rs` (new `Message` variants, `BrainViewState` fields, subscription).

**Tech Stack:** Rust, `iced` 0.13 (canvas, subscriptions), existing `ninox-core::BrainEntry`/`BrainIndex`. No new crates.

**Spec:** `docs/superpowers/specs/2026-09-14-catalogue-pinboard-design.md`

## Global Constraints

- Sidebar unifies on the drawer style: 272px wide, `heavy_frame` styled, one component (`drawers_rail`) used by both catalogue and pinboard modes — no separate flat rail.
- Layout positions are stored normalized to `[0,1]×[0,1]`, never in raw canvas pixels, so resizes stay safe exactly like the old hash-scatter.
- The force simulation runs live/continuously while pinboard mode is on screen — not a settle-once layout.
- Dragged positions are session-only: never written to disk/config; reseeded from the deterministic hash scatter on reindex/catalogue switch/restart.
- Click vs. drag is disambiguated by a small pixel-movement threshold on the same press, not a modifier key.
- The physics subscription only runs while `view == View::Brain && brain_view.mode == BrainMode::Pinboard`.
- Cross-highlighting reuses the existing `Message::BrainHoverEntry`/`BrainSelectEntry` messages and `BrainViewState.hovered`/`.selected` fields as the single source of truth — no parallel hover state.
- No new external dependencies — the force layout is hand-rolled, no graph/physics crate added.
- Never run `cargo fmt` wholesale (repo `CLAUDE.md`) — the tree isn't rustfmt-clean; format only the exact lines you touch if your editor insists.

---

### Task 1: Shared sidebar — pinboard adopts `drawers_rail`

**Files:**
- Modify: `crates/ninox-app/src/components/brain_panel.rs:447-521` (`pinboard_body`, `dentry_row`)

**Interfaces:**
- Consumes: existing `drawers_rail(app: &App) -> Element<'_, Message>` (already defined at `brain_panel.rs:525`), `category_color(s: &ColorScheme, ty: &str) -> Color` (`brain_panel.rs:25`).
- Produces: no new signatures — `dentry_row` and `pinboard_body`'s signatures are unchanged, only their bodies change.

- [ ] **Step 1: Replace `pinboard_body`'s inline flat rail with `drawers_rail`**

Replace the whole function (currently `brain_panel.rs:447-510`, including its stale "Task 13" doc comment) with:

```rust
/// Pinboard mode: the same drawers rail catalogue mode uses, beside the
/// specimen-board canvas.
fn pinboard_body(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let board_frame = container(super::brain_pinboard::pinboard_canvas(app))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| crate::style::heavy_frame(s));

    // Hovered id may no longer exist (a reindex/catalogue switch can drop or
    // rename entries out from under a stale hover) — resolve defensively and
    // simply skip the slip rather than panicking or showing stale content.
    let hovered_entry = app
        .brain_view
        .hovered
        .as_deref()
        .and_then(|id| app.brain_view.entries.iter().find(|e| e.id == id));
    let board: Element<Message> = match hovered_entry {
        Some(e) => iced::widget::stack![board_frame, hover_preview_slip(s, e)].into(),
        None => board_frame.into(),
    };

    row![drawers_rail(app), board]
        .spacing(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 22.0, left: 28.0 })
        .into()
}
```

This drops the now-unused `categories(&app.brain_view.entries)` loop and the
`shadow_alpha`-derived `card_a` local — both were only used by the deleted
flat rail. `categories` and `shadow_alpha` stay imported/used elsewhere in
the file (`drawers_rail`, `hover_preview_slip`), so no import changes.

- [ ] **Step 2: Add a category-color dot to each `dentry_row`**

Replace `dentry_row` (currently `brain_panel.rs:625-668`) with:

```rust
fn dentry_row<'a>(app: &'a App, entry: &BrainEntry) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_selected = app.brain_view.selected.as_deref() == Some(entry.id.as_str());
    let id = entry.id.clone();
    let name = entry.name.clone();
    let dot_color = category_color(s, &entry.entry_type);
    let updated: String = entry.updated.as_deref().unwrap_or("").chars().take(10).collect();
    let bar_color = if is_selected { s.accent } else { Color::TRANSPARENT };

    button(
        row![
            vline(bar_color, 3.0),
            container(
                row![
                    text("●").size(8).color(dot_color),
                    Space::new(8, 0),
                    text(name).size(10.5).font(if is_selected { MONO_MEDIUM } else { MONO }),
                    Space::new(Length::Fill, 0),
                    text(updated).size(8.5).font(MONO).color(s.faint),
                ]
                .align_y(Alignment::Center),
            )
            .padding(iced::Padding { top: 4.0, right: 16.0, bottom: 4.0, left: 36.0 })
            .width(Length::Fill),
        ]
        .height(Length::Fixed(22.0)),
    )
    .on_press(Message::BrainSelectEntry(id))
    .width(Length::Fill)
    .padding(0)
    .style(move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if is_selected {
                s.card
            } else if hovered {
                s.paper_2
            } else {
                Color::TRANSPARENT
            })),
            text_color: if is_selected || hovered { s.ink } else { s.ink_2 },
            border: Border::default(),
            ..Default::default()
        }
    })
    .into()
}
```

(Left padding drops from 44 to 36 to make room for the new dot+spacer; this
is a visual tuning value, not load-bearing — adjust in Step 4 if it looks
cramped or misaligned against the drawer header's own dot.)

- [ ] **Step 3: Build**

Run: `cargo build -p ninox-app`
Expected: builds cleanly, no unused-variable/import warnings.

- [ ] **Step 4: Manual visual check**

Run the app (`cargo run -p ninox-app`, or use the `run` skill), open the
Brain view, and check: (a) Pinboard mode now shows the same collapsible
drawers as Catalogue mode instead of the old flat category-count list, (b)
each entry row in an open drawer shows a small category-color dot before
its name, matching that category's pinboard node color. Adjust the left
padding from Step 2 if the dot/name alignment looks off.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/brain_panel.rs
git commit -m "feat(brain): pinboard adopts the shared drawers-rail sidebar"
```

---

### Task 2: Cross-highlighting between sidebar and pinboard

**Files:**
- Modify: `crates/ninox-app/src/components/brain_panel.rs:1-12` (imports), `dentry_row` (from Task 1)
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs:157-226` (`Pinboard::draw`)

**Interfaces:**
- Consumes: `Message::BrainHoverEntry(Option<String>)` (already exists, `app.rs:363`), `BrainViewState.hovered`/`.selected` (already exist, `app.rs:111-117`).
- Produces: no new signatures.

- [ ] **Step 1: Wrap `dentry_row` in a `mouse_area` and add cross-hover styling**

First, add `mouse_area` to the widget import list at `brain_panel.rs:2`:

```rust
    widget::{button, column, container, mouse_area, pick_list, row, scrollable, text, text_input, Space},
```

Then update `dentry_row` (from Task 1) to:

```rust
fn dentry_row<'a>(app: &'a App, entry: &BrainEntry) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_selected = app.brain_view.selected.as_deref() == Some(entry.id.as_str());
    // Hovered via the *pinboard* canvas, not this row's own mouse-over —
    // rendered as a lighter tint so it reads differently from both the
    // selected accent bar and the local button-hover background below.
    let is_cross_hovered =
        !is_selected && app.brain_view.hovered.as_deref() == Some(entry.id.as_str());
    let id = entry.id.clone();
    let hover_id = entry.id.clone();
    let name = entry.name.clone();
    let dot_color = category_color(s, &entry.entry_type);
    let updated: String = entry.updated.as_deref().unwrap_or("").chars().take(10).collect();
    let bar_color = if is_selected { s.accent } else { Color::TRANSPARENT };

    let row_button = button(
        row![
            vline(bar_color, 3.0),
            container(
                row![
                    text("●").size(8).color(dot_color),
                    Space::new(8, 0),
                    text(name).size(10.5).font(if is_selected { MONO_MEDIUM } else { MONO }),
                    Space::new(Length::Fill, 0),
                    text(updated).size(8.5).font(MONO).color(s.faint),
                ]
                .align_y(Alignment::Center),
            )
            .padding(iced::Padding { top: 4.0, right: 16.0, bottom: 4.0, left: 36.0 })
            .width(Length::Fill),
        ]
        .height(Length::Fixed(22.0)),
    )
    .on_press(Message::BrainSelectEntry(id))
    .width(Length::Fill)
    .padding(0)
    .style(move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(if is_selected {
                s.card
            } else if hovered || is_cross_hovered {
                s.paper_2
            } else {
                Color::TRANSPARENT
            })),
            text_color: if is_selected || hovered { s.ink } else { s.ink_2 },
            border: Border::default(),
            ..Default::default()
        }
    });

    mouse_area(row_button)
        .on_enter(Message::BrainHoverEntry(Some(hover_id.clone())))
        .on_exit(Message::BrainHoverEntry(None))
        .into()
}
```

- [ ] **Step 2: Switch the pinboard canvas's ring rendering to the shared hover/selected state**

In `brain_pinboard.rs`, the `draw` method currently takes `state: &PinboardState` and only ever reads `state.hovered` (for the hover ring — there is no selection ring at all). The body no longer reads it after this change, but the trait signature requires the parameter, so rename it to `_state`:

```rust
    fn draw(
        &self,
        _state: &PinboardState,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
```

Then replace the per-node ring block (currently `brain_pinboard.rs:201-223`) with:

```rust
        for n in &nodes {
            let dot = Path::circle(Point::new(n.x, n.y), n.r);
            frame.fill(&dot, n.color);
            frame.stroke(
                &dot,
                Stroke::default().with_color(Color { a: 0.75, ..s.ink }).with_width(1.2),
            );
            if n.hit {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 4.0),
                    Stroke::default().with_color(s.accent).with_width(1.2),
                );
            }
            // Selection ring: thicker, always-on accent ring, matching the
            // sidebar's selected-row accent bar (`dentry_row`) — shared
            // source of truth (`app.brain_view.selected`), not local mouse
            // state.
            if self.app.brain_view.selected.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 5.0),
                    Stroke::default().with_color(s.accent).with_width(2.0),
                );
            }
            // Hover ring: sourced from `app.brain_view.hovered`, shared with
            // the sidebar's cross-highlight (`dentry_row`'s `is_cross_hovered`)
            // — a sidebar-row hover rings the matching node here too, not
            // just this canvas's own mouse movement.
            if self.app.brain_view.hovered.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 3.0),
                    Stroke::default().with_color(Color { a: 1.0, ..s.ink }).with_width(1.4),
                );
            }
        }
```

`PinboardState.hovered` keeps being written in `update()` (unchanged in this
task) — it's still needed there as the dedupe guard against re-emitting
`BrainHoverEntry` on every sub-pixel `CursorMoved`, and by
`mouse_interaction()` for the pointer-cursor icon (that one **stays** tied
to the canvas's own local mouse state deliberately — the cursor icon should
reflect where the physical mouse is, not a sidebar hover elsewhere). Only
`draw()`'s rendering source changes.

- [ ] **Step 3: Build and run existing tests**

Run: `cargo build -p ninox-app && cargo test -p ninox-app --lib brain_pinboard`
Expected: builds cleanly; existing `brain_pinboard` unit tests
(`hash01_*`, `node_radius_*`, `hit_test_*`, `edge_key_*`, `resolve_edges_*`)
still pass unchanged.

- [ ] **Step 4: Manual visual check**

Run the app, open Pinboard mode, hover a sidebar drawer entry: the matching
pinboard node should ring. Hover a pinboard node: the matching sidebar row
(if its drawer is open) should tint. Click a sidebar entry: it should show
the persistent accent ring on its pinboard node too.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/brain_panel.rs crates/ninox-app/src/components/brain_pinboard.rs
git commit -m "feat(brain): cross-highlight hover/selection between sidebar and pinboard"
```

---

### Task 3: `ForceLayout` — entry seeding/pruning (no physics yet)

**Files:**
- Create: `crates/ninox-app/src/components/force_layout.rs`
- Modify: `crates/ninox-app/src/components/mod.rs` (register the module)
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs:31` (`hash01` visibility)

**Interfaces:**
- Consumes: `ninox_core::BrainEntry` (existing), `brain_pinboard::hash01(s: &str, salt: u64) -> f32` (existing, gains `pub(crate)`).
- Produces: `pub struct ForceLayout` with `sync_entries(&mut self, entries: &[BrainEntry])`, `position(&self, id: &str) -> Option<(f32, f32)>`, `set_position(&mut self, id: &str, pos: (f32, f32))`. Derives `Debug, Clone, Default`. Task 5 puts one on `BrainViewState`; Task 4 adds `step()` to this same struct.

- [ ] **Step 1: Make `hash01` visible to the new module**

In `brain_pinboard.rs:31`, change:
```rust
fn hash01(s: &str, salt: u64) -> f32 {
```
to:
```rust
pub(crate) fn hash01(s: &str, salt: u64) -> f32 {
```

- [ ] **Step 2: Register the new module**

In `crates/ninox-app/src/components/mod.rs`, insert alphabetically (after `folio`, before `info_panel`):
```rust
pub mod force_layout;
```

- [ ] **Step 3: Write the failing tests for seeding/pruning/positioning**

Create `crates/ninox-app/src/components/force_layout.rs` with:

```rust
//! Pure force-directed layout simulation for the brain pinboard — no
//! `iced` dependency, so the physics is testable in isolation from
//! rendering. Positions live in normalized `[0,1] x [0,1]` space, not
//! canvas pixels, so the layout survives window resizes the same way the
//! old per-frame hash-scatter did — multiply by canvas bounds only at
//! draw time (see `brain_pinboard::Pinboard::nodes`).

use std::collections::{HashMap, HashSet};

use ninox_core::BrainEntry;

use super::brain_pinboard::hash01;

#[derive(Debug, Clone, Default)]
pub struct ForceLayout {
    pos: HashMap<String, (f32, f32)>,
    vel: HashMap<String, (f32, f32)>,
}

impl ForceLayout {
    /// Seed any entry id missing from the layout via the same deterministic
    /// hash scatter the pinboard always used, and drop any id no longer
    /// present in `entries` — called every physics tick, so a
    /// reindex/catalogue switch can't leak stale positions or leave a
    /// removed entry's node stuck on screen forever.
    pub fn sync_entries(&mut self, entries: &[BrainEntry]) {
        let ids: HashSet<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        self.pos.retain(|id, _| ids.contains(id.as_str()));
        self.vel.retain(|id, _| ids.contains(id.as_str()));
        for e in entries {
            self.pos
                .entry(e.id.clone())
                .or_insert_with(|| (0.05 + 0.90 * hash01(&e.id, 7), 0.06 + 0.88 * hash01(&e.id, 13)));
            self.vel.entry(e.id.clone()).or_insert((0.0, 0.0));
        }
    }

    pub fn position(&self, id: &str) -> Option<(f32, f32)> {
        self.pos.get(id).copied()
    }

    /// No-op if `id` hasn't been seeded yet (via `sync_entries`) — a drag
    /// message racing ahead of the next physics tick simply has nothing to
    /// move.
    pub fn set_position(&mut self, id: &str, new_pos: (f32, f32)) {
        if let Some(p) = self.pos.get_mut(id) {
            *p = (new_pos.0.clamp(0.0, 1.0), new_pos.1.clamp(0.0, 1.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> BrainEntry {
        BrainEntry {
            id: id.to_string(),
            entry_type: "concepts".to_string(),
            name: id.to_string(),
            tags: vec![],
            repos: vec![],
            updated: None,
            body: String::new(),
        }
    }

    #[test]
    fn sync_entries_seeds_new_ids_deterministically() {
        let mut a = ForceLayout::default();
        let mut b = ForceLayout::default();
        let entries = vec![entry("concepts/x.md")];
        a.sync_entries(&entries);
        b.sync_entries(&entries);
        assert_eq!(a.position("concepts/x.md"), b.position("concepts/x.md"));
        assert!(a.position("concepts/x.md").is_some());
    }

    #[test]
    fn sync_entries_prunes_removed_ids() {
        let mut layout = ForceLayout::default();
        layout.sync_entries(&[entry("a.md"), entry("b.md")]);
        assert!(layout.position("b.md").is_some());
        layout.sync_entries(&[entry("a.md")]);
        assert!(layout.position("b.md").is_none());
    }

    #[test]
    fn set_position_clamps_into_unit_square() {
        let mut layout = ForceLayout::default();
        layout.sync_entries(&[entry("a.md")]);
        layout.set_position("a.md", (1.5, -0.5));
        assert_eq!(layout.position("a.md"), Some((1.0, 0.0)));
    }

    #[test]
    fn set_position_is_a_noop_for_an_unseeded_id() {
        let mut layout = ForceLayout::default();
        layout.set_position("ghost.md", (0.5, 0.5));
        assert_eq!(layout.position("ghost.md"), None);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p ninox-app --lib force_layout`
Expected: all 4 tests pass (this step combines "write" and "verify" since
the implementation above is already the minimal correct version — there's
no meaningful red/green split for a pure data-structure module like this).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/force_layout.rs crates/ninox-app/src/components/mod.rs crates/ninox-app/src/components/brain_pinboard.rs
git commit -m "feat(brain): ForceLayout — seeded, prunable node positions in normalized space"
```

---

### Task 4: `ForceLayout::step()` — the physics

**Files:**
- Modify: `crates/ninox-app/src/components/force_layout.rs`

**Interfaces:**
- Consumes: `Self::pos`/`Self::vel` (from Task 3).
- Produces: `pub fn step(&mut self, entries: &[BrainEntry], edges: &[(usize, usize)], pinned: Option<&str>, dt: f32)` — Task 5's `BrainPhysicsTick` handler calls this every tick.

- [ ] **Step 1: Write the failing convergence/repulsion/bounds/pinning tests**

Add to `force_layout.rs`'s constants (top of file, after the doc comment) and test module:

```rust
/// Spring rest length, as a fraction of the unit square's diagonal.
const REST_LENGTH: f32 = 0.22;
/// Hooke's-law spring constant along edges.
const SPRING_K: f32 = 3.0;
/// Coulomb-like repulsion constant between all node pairs.
const REPULSION_K: f32 = 0.0026;
/// Minimum pairwise distance used in repulsion/spring math, avoiding a
/// division singularity when two nodes land exactly on top of each other.
const MIN_DIST: f32 = 0.02;
/// Pull-to-center strength — keeps disconnected/isolated nodes on-canvas.
const CENTER_K: f32 = 0.03;
/// Velocity damping applied every step (1.0 = no damping, drifts forever).
const DAMPING: f32 = 0.82;
```

```rust
    #[test]
    fn connected_nodes_converge_toward_rest_length() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.1, 0.5));
        layout.set_position("b.md", (0.9, 0.5));
        let edges = vec![(0, 1)];
        for _ in 0..500 {
            layout.step(&entries, &edges, None, 0.05);
        }
        let (ax, _) = layout.position("a.md").unwrap();
        let (bx, _) = layout.position("b.md").unwrap();
        let dist = (bx - ax).abs();
        assert!((dist - REST_LENGTH).abs() < 0.05, "expected ~{REST_LENGTH}, got {dist}");
    }

    #[test]
    fn unconnected_nodes_are_pushed_apart_by_repulsion() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.5, 0.5));
        layout.set_position("b.md", (0.5001, 0.5));
        for _ in 0..50 {
            layout.step(&entries, &[], None, 0.05);
        }
        let (ax, ay) = layout.position("a.md").unwrap();
        let (bx, by) = layout.position("b.md").unwrap();
        assert!((ax - bx).hypot(ay - by) > 0.01);
    }

    #[test]
    fn isolated_node_stays_within_the_unit_square() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.99, 0.01));
        for _ in 0..200 {
            layout.step(&entries, &[], None, 0.05);
        }
        let (x, y) = layout.position("a.md").unwrap();
        assert!((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y));
    }

    #[test]
    fn pinned_node_is_excluded_from_integration() {
        let mut layout = ForceLayout::default();
        let entries = vec![entry("a.md"), entry("b.md")];
        layout.sync_entries(&entries);
        layout.set_position("a.md", (0.5, 0.5));
        layout.set_position("b.md", (0.5001, 0.5));
        layout.step(&entries, &[], Some("a.md"), 0.05);
        assert_eq!(layout.position("a.md"), Some((0.5, 0.5)));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ninox-app --lib force_layout`
Expected: FAIL with "no method named `step` found" (doesn't exist yet).

- [ ] **Step 3: Implement `step`**

Add to `impl ForceLayout` in `force_layout.rs`:

```rust
    /// Advance the simulation by one step of `dt` seconds: inverse-square
    /// repulsion between every pair, Hooke's-law springs along `edges`
    /// pulling toward `REST_LENGTH`, a mild centering pull, velocity
    /// damping, then integration. `pinned`, if set, is excluded from force
    /// *integration* (a drag is driving its position directly via
    /// `set_position`) but still exerts repulsion/spring forces on every
    /// other node, exactly like any other node.
    pub fn step(&mut self, entries: &[BrainEntry], edges: &[(usize, usize)], pinned: Option<&str>, dt: f32) {
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        let mut force: HashMap<&str, (f32, f32)> = ids.iter().map(|&id| (id, (0.0, 0.0))).collect();

        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (Some(&pi), Some(&pj)) = (self.pos.get(ids[i]), self.pos.get(ids[j])) else { continue };
                let (dx, dy) = (pi.0 - pj.0, pi.1 - pj.1);
                let dist = dx.hypot(dy).max(MIN_DIST);
                let f = REPULSION_K / (dist * dist);
                let (fx, fy) = (dx / dist * f, dy / dist * f);
                force.get_mut(ids[i]).unwrap().0 += fx;
                force.get_mut(ids[i]).unwrap().1 += fy;
                force.get_mut(ids[j]).unwrap().0 -= fx;
                force.get_mut(ids[j]).unwrap().1 -= fy;
            }
        }

        for &(a, b) in edges {
            let (Some(&ia), Some(&ib)) = (ids.get(a), ids.get(b)) else { continue };
            let (Some(&pa), Some(&pb)) = (self.pos.get(ia), self.pos.get(ib)) else { continue };
            let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
            let dist = dx.hypot(dy).max(MIN_DIST);
            let f = SPRING_K * (dist - REST_LENGTH);
            let (fx, fy) = (dx / dist * f, dy / dist * f);
            force.get_mut(ia).unwrap().0 += fx;
            force.get_mut(ia).unwrap().1 += fy;
            force.get_mut(ib).unwrap().0 -= fx;
            force.get_mut(ib).unwrap().1 -= fy;
        }

        for &id in &ids {
            if let Some(&p) = self.pos.get(id) {
                let f = force.get_mut(id).unwrap();
                f.0 += (0.5 - p.0) * CENTER_K;
                f.1 += (0.5 - p.1) * CENTER_K;
            }
        }

        for &id in &ids {
            if pinned == Some(id) {
                continue;
            }
            let (fx, fy) = force[id];
            let v = self.vel.entry(id.to_string()).or_insert((0.0, 0.0));
            v.0 = (v.0 + fx * dt) * DAMPING;
            v.1 = (v.1 + fy * dt) * DAMPING;
            if let Some(p) = self.pos.get_mut(id) {
                p.0 = (p.0 + v.0 * dt).clamp(0.0, 1.0);
                p.1 = (p.1 + v.1 * dt).clamp(0.0, 1.0);
            }
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ninox-app --lib force_layout`
Expected: all 8 tests (4 from Task 3, 4 new) pass. If
`connected_nodes_converge_toward_rest_length` or
`unconnected_nodes_are_pushed_apart_by_repulsion` is flaky/fails, adjust
`SPRING_K`/`REPULSION_K`/`DAMPING` (these are the "tuned empirically"
constants the design doc calls out) rather than the test tolerances.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/force_layout.rs
git commit -m "feat(brain): force-directed physics step — springs, repulsion, centering, damping"
```

---

### Task 5: Wire `ForceLayout` into `App` — messages, state, subscription

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (`BrainViewState`, `Message`, `App::apply`, `App::subscription`)

**Interfaces:**
- Consumes: `force_layout::ForceLayout` (Tasks 3-4): `sync_entries`, `step`, `set_position`.
- Produces: `Message::BrainPhysicsTick`, `Message::BrainDragStart(String)`, `Message::BrainDragMove(String, f32, f32)`, `Message::BrainDragEnd`; `BrainViewState.layout: force_layout::ForceLayout`, `BrainViewState.dragging: Option<String>`; `App::wants_physics_tick(view: &View, mode: BrainMode) -> bool`. Task 6's canvas emits the three drag messages and reads `brain_view.layout`/`.dragging` indirectly through them.

- [ ] **Step 1: Add the new `BrainViewState` fields**

In `app.rs`, extend the `BrainViewState` struct (currently ending at
`app.rs:138` with the `edges` field) by adding, right after `edges`:

```rust
    /// Live force-directed layout for the pinboard canvas — positions in
    /// normalized `[0,1]^2` space, stepped by `Message::BrainPhysicsTick`.
    /// Session-only: reseeded from scratch (via `sync_entries`'s hash-based
    /// fallback) on every reindex/catalogue switch and on app restart,
    /// never persisted to disk.
    pub layout: crate::components::force_layout::ForceLayout,
    /// Id of the pinboard node currently being dragged, if any — excluded
    /// from `layout`'s force integration while set (see `BrainDragMove`).
    pub dragging: Option<String>,
```

- [ ] **Step 2: Add the new `Message` variants**

In the `Message` enum, right after `BrainHoverEntry(Option<String>)`
(`app.rs:363`), insert:

```rust
    /// One physics step of the pinboard's live force-directed layout —
    /// ticked by `App::subscription`'s physics subscription, only while
    /// the Brain view is open in Pinboard mode.
    BrainPhysicsTick,
    /// A pinboard node's press crossed the drag threshold — pins it so the
    /// physics tick stops integrating forces onto it directly (it still
    /// exerts forces on its neighbors).
    BrainDragStart(String),
    /// The dragged node's cursor-driven position, in normalized `[0,1]^2`
    /// pinboard-space.
    BrainDragMove(String, f32, f32),
    /// The drag ended — release the node back to the simulation.
    BrainDragEnd,
```

- [ ] **Step 3: Write the failing `App::apply` tests**

Add to `app.rs`'s `mod tests` (near the other `brain_*` tests, e.g. after
`switching_mode_clears_hovered`):

```rust
    #[test]
    fn physics_tick_seeds_and_positions_new_entries() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("concepts")).unwrap();
        std::fs::write(brain_dir.join("concepts").join("note.md"), "---\nname: Note\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m, _) = m.update(Message::NavigateBrain);
        assert!(m.brain_view.layout.position("concepts/note.md").is_none());

        let (m, _) = m.update(Message::BrainPhysicsTick);
        assert!(m.brain_view.layout.position("concepts/note.md").is_some());
    }

    #[test]
    fn drag_start_move_end_pins_then_releases_a_node() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("concepts")).unwrap();
        std::fs::write(brain_dir.join("concepts").join("note.md"), "---\nname: Note\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m, _) = m.update(Message::NavigateBrain);
        let (m, _) = m.update(Message::BrainPhysicsTick); // seeds the layout

        let (m, _) = m.update(Message::BrainDragStart("concepts/note.md".into()));
        assert_eq!(m.brain_view.dragging.as_deref(), Some("concepts/note.md"));

        let (m, _) = m.update(Message::BrainDragMove("concepts/note.md".into(), 0.9, 0.1));
        assert_eq!(m.brain_view.layout.position("concepts/note.md"), Some((0.9, 0.1)));

        let (m, _) = m.update(Message::BrainDragEnd);
        assert_eq!(m.brain_view.dragging, None);
        assert_eq!(m.brain_view.layout.position("concepts/note.md"), Some((0.9, 0.1)));
    }

    #[test]
    fn drag_move_for_a_different_id_than_dragging_is_ignored() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("concepts")).unwrap();
        std::fs::write(brain_dir.join("concepts").join("a.md"), "---\nname: A\n---\nbody").unwrap();
        std::fs::write(brain_dir.join("concepts").join("b.md"), "---\nname: B\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let m = base_with_brain(e, brain);
        let (m, _) = m.update(Message::NavigateBrain);
        let (m, _) = m.update(Message::BrainPhysicsTick);
        let before = m.brain_view.layout.position("concepts/b.md").unwrap();

        let (m, _) = m.update(Message::BrainDragStart("concepts/a.md".into()));
        let (m, _) = m.update(Message::BrainDragMove("concepts/b.md".into(), 0.5, 0.5));
        assert_eq!(m.brain_view.layout.position("concepts/b.md"), Some(before));
    }

    #[test]
    fn wants_physics_tick_only_for_brain_pinboard() {
        assert!(App::wants_physics_tick(&View::Brain, BrainMode::Pinboard));
        assert!(!App::wants_physics_tick(&View::Brain, BrainMode::Catalogue));
        assert!(!App::wants_physics_tick(&View::FleetBoard { scope: None }, BrainMode::Pinboard));
    }
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p ninox-app --lib brain_ -- --test-threads=1`
Expected: FAIL to compile — `Message::BrainPhysicsTick` etc. and
`App::wants_physics_tick` don't exist yet.

- [ ] **Step 5: Implement the `App::apply` handlers**

In `App::apply`'s match (`app.rs:1240`), right after the
`Message::BrainHoverEntry(id) => { ... }` arm (`app.rs:2575-2578`), insert:

```rust
            Message::BrainPhysicsTick => {
                state.brain_view.layout.sync_entries(&state.brain_view.entries);
                let pinned = state.brain_view.dragging.clone();
                state.brain_view.layout.step(
                    &state.brain_view.entries,
                    &state.brain_view.edges,
                    pinned.as_deref(),
                    1.0 / 60.0,
                );
                Task::none()
            }

            Message::BrainDragStart(id) => {
                state.brain_view.dragging = Some(id);
                Task::none()
            }

            Message::BrainDragMove(id, x, y) => {
                if state.brain_view.dragging.as_deref() == Some(id.as_str()) {
                    state.brain_view.layout.set_position(&id, (x, y));
                }
                Task::none()
            }

            Message::BrainDragEnd => {
                state.brain_view.dragging = None;
                Task::none()
            }
```

- [ ] **Step 6: Implement `wants_physics_tick` and wire the subscription**

Add this as an associated function on `App` (e.g. right before
`pub fn subscription`, `app.rs:3268`):

```rust
    /// Whether the pinboard's physics subscription should be running —
    /// pulled out of `subscription()` so the (View, BrainMode) predicate is
    /// directly unit-testable without going through `Subscription` itself.
    fn wants_physics_tick(view: &View, mode: BrainMode) -> bool {
        matches!(view, View::Brain) && mode == BrainMode::Pinboard
    }
```

Then in `subscription()` (`app.rs:3268-3296`), add a fourth branch before
the final `Subscription::batch`:

```rust
        let physics_sub = if Self::wants_physics_tick(&state.view, state.brain_view.mode) {
            iced::time::every(std::time::Duration::from_millis(16)).map(|_| Message::BrainPhysicsTick)
        } else {
            Subscription::none()
        };

        Subscription::batch([engine_sub, keyboard_sub, poll_sub, physics_sub])
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p ninox-app --lib brain_ -- --test-threads=1`
Expected: all pass, including the 4 new tests from Step 3.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-app/src/app.rs
git commit -m "feat(brain): wire ForceLayout into App state, messages, and subscription"
```

---

### Task 6: Canvas reads live positions; click-vs-drag input handling

**Files:**
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs`

**Interfaces:**
- Consumes: `app.brain_view.layout.position(&str) -> Option<(f32, f32)>` (Task 3), `Message::BrainDragStart/Move/End` (Task 5).
- Produces: `handle_mouse_event` (private, but directly unit-tested in this file) — no signature other tasks depend on.

- [ ] **Step 1: Make `Pinboard::nodes()` read positions from the layout**

Replace the `.map(...)` closure inside `nodes()` (`brain_pinboard.rs:71-80`)
with:

```rust
            .map(|(i, e)| {
                let (nx, ny) = self
                    .app
                    .brain_view
                    .layout
                    .position(&e.id)
                    .unwrap_or((0.05 + 0.90 * hash01(&e.id, 7), 0.06 + 0.88 * hash01(&e.id, 13)));
                Node {
                    x: bounds.width * nx,
                    y: bounds.height * ny,
                    r: node_radius(degree.get(i).copied().unwrap_or(0) as f32),
                    color: category_color(s, &e.entry_type),
                    hit: !q.is_empty()
                        && (e.name.to_lowercase().contains(&q) || e.id.to_lowercase().contains(&q)),
                    id: e.id.clone(),
                }
            })
```

The `unwrap_or` fallback (same hash scatter as before) only matters for the
one frame between an entry appearing and the next `BrainPhysicsTick`
seeding it — practically unreachable since the tick runs continuously
whenever this canvas is visible, but keeps the function total.

- [ ] **Step 2: Write the failing click-vs-drag state-machine tests**

Add `press: Option<(String, Point)>` and `dragging: Option<String>` fields
to `PinboardState` (`brain_pinboard.rs:90-93`):

```rust
#[derive(Default)]
pub struct PinboardState {
    hovered: Option<String>,
    /// Candidate node + press-origin (local canvas coords), recorded on
    /// `ButtonPressed`, before it's known whether this press resolves to a
    /// click (select) or a drag.
    press: Option<(String, Point)>,
    /// Set once a press has moved past `DRAG_THRESHOLD` — the id currently
    /// being dragged. Mirrored into `App.brain_view.dragging` via
    /// `BrainDragStart`/`BrainDragEnd` so the physics tick knows to pin it.
    dragging: Option<String>,
}

/// Minimum cursor movement (local canvas pixels) from the press origin
/// before a press upgrades from "candidate click" to "drag".
const DRAG_THRESHOLD: f32 = 4.0;
```

Add this test module (append to the existing `#[cfg(test)] mod tests`
block, alongside the `entry`/`Node` test helpers already there):

```rust
    fn bounds_100() -> Rectangle {
        Rectangle { x: 0.0, y: 0.0, width: 100.0, height: 100.0 }
    }

    #[test]
    fn click_without_movement_selects() {
        let mut state = PinboardState::default();
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::ButtonPressed(mouse::Button::Left),
            Some(Point::new(10.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(msg.is_none());
        assert!(matches!(&state.press, Some((id, _)) if id == "a.md"));

        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::ButtonReleased(mouse::Button::Left),
            Some(Point::new(11.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainSelectEntry(id)) if id == "a.md"));
        assert!(state.press.is_none());
        assert!(state.dragging.is_none());
    }

    #[test]
    fn press_move_past_threshold_starts_a_drag_not_a_select() {
        let mut state = PinboardState::default();
        let bounds = bounds_100();
        handle_mouse_event(
            &mut state,
            mouse::Event::ButtonPressed(mouse::Button::Left),
            Some(Point::new(10.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );

        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(30.0, 10.0) },
            Some(Point::new(30.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainDragStart(id)) if id == "a.md"));
        assert_eq!(state.dragging.as_deref(), Some("a.md"));
        assert!(state.press.is_none());

        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::ButtonReleased(mouse::Button::Left),
            Some(Point::new(30.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainDragEnd)));
        assert!(state.dragging.is_none());
    }

    #[test]
    fn dragging_emits_normalized_position() {
        let mut state = PinboardState { dragging: Some("a.md".to_string()), ..Default::default() };
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(25.0, 75.0) },
            Some(Point::new(25.0, 75.0)),
            None,
            bounds,
        );
        assert!(matches!(
            msg,
            Some(Message::BrainDragMove(id, x, y))
                if id == "a.md" && (x - 0.25).abs() < 1e-6 && (y - 0.75).abs() < 1e-6
        ));
    }

    #[test]
    fn hover_still_fires_when_not_pressing() {
        let mut state = PinboardState::default();
        let bounds = bounds_100();
        let (_, msg) = handle_mouse_event(
            &mut state,
            mouse::Event::CursorMoved { position: Point::new(10.0, 10.0) },
            Some(Point::new(10.0, 10.0)),
            Some("a.md".to_string()),
            bounds,
        );
        assert!(matches!(msg, Some(Message::BrainHoverEntry(Some(id))) if id == "a.md"));
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ninox-app --lib brain_pinboard`
Expected: FAIL to compile — `handle_mouse_event` doesn't exist yet.

- [ ] **Step 4: Implement `handle_mouse_event` and wire it into `Program::update`**

Add this function above `impl<'a> canvas::Program<Message> for Pinboard<'a>`:

```rust
/// The press/drag/hover state machine, factored out of `Program::update` so
/// it's testable without a real `App`. `hit` is whichever node (if any) is
/// under the cursor for this event, already resolved via `hit_test`; `pos`
/// is the cursor's local-canvas position (`None` once it's left this
/// canvas's own bounds, matching `cursor.position_in`); `bounds` is used
/// only to normalize a drag's emitted position into `[0,1]^2`.
fn handle_mouse_event(
    state: &mut PinboardState,
    event: mouse::Event,
    pos: Option<Point>,
    hit: Option<String>,
    bounds: Rectangle,
) -> (canvas::event::Status, Option<Message>) {
    match event {
        mouse::Event::ButtonPressed(mouse::Button::Left) => {
            if let (Some(pos), Some(id)) = (pos, hit) {
                state.press = Some((id, pos));
            }
            (canvas::event::Status::Ignored, None)
        }
        mouse::Event::ButtonReleased(mouse::Button::Left) => {
            if state.dragging.take().is_some() {
                return (canvas::event::Status::Captured, Some(Message::BrainDragEnd));
            }
            if let Some((id, _)) = state.press.take() {
                return (canvas::event::Status::Captured, Some(Message::BrainSelectEntry(id)));
            }
            (canvas::event::Status::Ignored, None)
        }
        mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft => {
            let hover_changed = hit != state.hovered;
            if hover_changed {
                state.hovered = hit.clone();
            }

            if let Some(pos) = pos {
                if let Some(dragging) = state.dragging.clone() {
                    let nx = (pos.x / bounds.width).clamp(0.0, 1.0);
                    let ny = (pos.y / bounds.height).clamp(0.0, 1.0);
                    return (canvas::event::Status::Captured, Some(Message::BrainDragMove(dragging, nx, ny)));
                }
                if let Some((id, origin)) = state.press.clone() {
                    if (pos.x - origin.x).hypot(pos.y - origin.y) > DRAG_THRESHOLD {
                        state.press = None;
                        state.dragging = Some(id.clone());
                        return (canvas::event::Status::Captured, Some(Message::BrainDragStart(id)));
                    }
                }
            }

            if hover_changed {
                return (canvas::event::Status::Ignored, Some(Message::BrainHoverEntry(hit)));
            }
            (canvas::event::Status::Ignored, None)
        }
        _ => (canvas::event::Status::Ignored, None),
    }
}
```

Then replace the whole `fn update` body inside
`impl<'a> canvas::Program<Message> for Pinboard<'a>` (currently
`brain_pinboard.rs:228-262`) with:

```rust
    fn update(
        &self,
        state: &mut PinboardState,
        event: canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        let canvas::Event::Mouse(mouse_event) = event else {
            return (canvas::event::Status::Ignored, None);
        };
        let local_bounds = Rectangle { x: 0.0, y: 0.0, ..bounds };
        let nodes = self.nodes(local_bounds);
        let pos = cursor.position_in(bounds);
        let hit = pos.and_then(|p| hit_test(&nodes, p));
        handle_mouse_event(state, mouse_event, pos, hit, bounds)
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ninox-app --lib brain_pinboard`
Expected: all pass, including the 4 new tests and the pre-existing
`hash01_*`/`node_radius_*`/`hit_test_*`/`edge_key_*`/`resolve_edges_*` ones.

- [ ] **Step 6: Full workspace check**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: clean. Do **not** run `cargo fmt` on the whole tree (repo
`CLAUDE.md` rule) — if your editor auto-formatted anything beyond the lines
you touched, revert those extra hunks.

- [ ] **Step 7: Manual visual check**

Run the app, open Pinboard mode: nodes should drift into a real spread-out
layout instead of a static scatter (springs pull linked notes together,
repulsion spreads everything else out). Click-and-hold a node and move the
mouse a few pixels — it should follow the cursor and its neighbors should
visibly react; release — it should rejoin the simulation instead of
snapping back. A plain click (no movement) should still open that entry in
Catalogue mode, same as before.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-app/src/components/brain_pinboard.rs
git commit -m "feat(brain): pinboard nodes are draggable and layout live via ForceLayout"
```
