# Brain Pinboard Decluttering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the three brain-pinboard problems from the spec — no clustering, a sidebar that doesn't match the catalogue view, and categories washing out to grey — by adding a cached force-directed layout, reusing the catalogue's drawer rail in pinboard mode, and normalizing category type aliases with a procedural color fallback.

**Architecture:** Category normalization happens once at file-parse time in `ninox-core` (shared by CLI/server/app). A Fruchterman-Reingold force simulation runs once per data change (reindex/catalogue-switch/initial load) in `ninox-app` and caches normalized positions on `BrainViewState`; the pinboard canvas draws from that cache every frame, unchanged from today's "recompute on data change, not per draw" pattern. The pinboard's sidebar becomes a straight reuse of the catalogue's `drawers_rail` component, with entry selection no longer force-switching the view out of pinboard mode.

**Tech Stack:** Rust, iced 0.13 (canvas/widget), rusqlite (via `ninox-core`'s `BrainIndex`). No new dependencies.

## Global Constraints

- No new crate dependencies — the force simulation and procedural color are plain `f32`/`iced::Color` arithmetic.
- Category alias normalization happens at read time in `ninox-core::brain::process_file`, not as a one-time rewrite of existing `.md` frontmatter files.
- Alias normalization is a fixed table (`repo→repos`, `symbol→symbols`, `concept→concepts`, `pattern→patterns`, `decision→decisions`, `relationship→relationships`, `error→errors`), not fuzzy matching — an unrecognized type is left alone.
- The force layout runs a fixed 400 iterations per computation (no wall-clock or convergence-threshold termination) so it stays a pure, deterministic function of `entries`/`edges`.
- The force layout is cached on data-change triggers only (`NavigateBrain`, `BrainReindex`, `BrainSwitchCatalogue`) — never recomputed per canvas draw.
- No pan/zoom camera system and no manual node dragging — out of scope per the spec's non-goals.

---

### Task 1: Category alias normalization (`ninox-core`)

**Files:**
- Modify: `crates/ninox-core/src/brain.rs:591-607` (entry_type derivation inside `process_file`)
- Test: `crates/ninox-core/src/brain.rs` (`mod tests` at the bottom of the same file)

**Interfaces:**
- Produces: `fn normalize_entry_type(raw: &str) -> String` (private to `ninox-core::brain`), applied inside `process_file` before `entry_type` is stored on `FileRecord`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/ninox-core/src/brain.rs` (after the existing `query_filters_by_type` test, `brain.rs:1096`):

```rust
#[test]
fn normalize_entry_type_maps_known_singular_aliases_to_canonical_plural() {
    for (raw, expected) in [
        ("repo", "repos"),
        ("symbol", "symbols"),
        ("concept", "concepts"),
        ("pattern", "patterns"),
        ("decision", "decisions"),
        ("relationship", "relationships"),
        ("error", "errors"),
    ] {
        assert_eq!(normalize_entry_type(raw), expected);
    }
}

#[test]
fn normalize_entry_type_is_case_insensitive() {
    assert_eq!(normalize_entry_type("Error"), "errors");
    assert_eq!(normalize_entry_type("DECISION"), "decisions");
}

#[test]
fn normalize_entry_type_passes_through_unrecognized_types() {
    assert_eq!(normalize_entry_type("people"), "people");
    assert_eq!(normalize_entry_type("Architecture"), "architecture");
}

#[test]
fn rebuild_normalizes_singular_frontmatter_type_to_canonical_plural() {
    let (brain, dir) = make_brain();
    let errors_dir = dir.path().join("errors");
    fs::create_dir_all(&errors_dir).unwrap();
    fs::write(
        errors_dir.join("timeout.md"),
        "---\nname: Timeout\ntype: error\n---\nConnection timed out.",
    )
    .unwrap();

    brain.rebuild(None).unwrap();

    let plural = brain
        .query("", None, QueryFilters { entry_type: Some("errors".into()), tag: None })
        .unwrap();
    assert_eq!(
        plural.len(),
        1,
        "singular frontmatter type must normalize to the plural canonical category"
    );
    assert_eq!(plural[0].name, "Timeout");

    let singular = brain
        .query("", None, QueryFilters { entry_type: Some("error".into()), tag: None })
        .unwrap();
    assert!(
        singular.is_empty(),
        "the un-normalized singular type must no longer exist as a separate category"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-core normalize_entry_type -- --test-threads=1`

Expected: FAIL to compile with `cannot find function 'normalize_entry_type' in this scope` (it doesn't exist yet).

- [ ] **Step 3: Implement `normalize_entry_type` and wire it into `process_file`**

Add this function directly above `fn process_file` in `crates/ninox-core/src/brain.rs` (immediately before line 581):

```rust
/// Collapse known singular/plural category aliases (e.g. a file whose
/// frontmatter declares `type: error` under the `errors/` directory) to
/// this app's canonical plural taxonomy names, so the same underlying
/// category never fragments into two differently-named entries. This is a
/// fixed alias table, not a fuzzy matcher — an unrecognized type passes
/// through (lowercased) untouched rather than being silently reassigned.
fn normalize_entry_type(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    match lower.as_str() {
        "repo" => "repos".to_string(),
        "symbol" => "symbols".to_string(),
        "concept" => "concepts".to_string(),
        "pattern" => "patterns".to_string(),
        "decision" => "decisions".to_string(),
        "relationship" => "relationships".to_string(),
        "error" => "errors".to_string(),
        _ => lower,
    }
}
```

Then change the `entry_type` derivation inside `process_file` (`crates/ninox-core/src/brain.rs:601-607`) from:

```rust
    let entry_type = parsed
        .frontmatter
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or(parent_type)
        .unwrap_or_else(|| "note".to_string());
```

to:

```rust
    let entry_type = normalize_entry_type(
        &parsed
            .frontmatter
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(parent_type)
            .unwrap_or_else(|| "note".to_string()),
    );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-core normalize_entry_type -- --test-threads=1`

Expected: PASS (4 tests: `normalize_entry_type_maps_known_singular_aliases_to_canonical_plural`, `normalize_entry_type_is_case_insensitive`, `normalize_entry_type_passes_through_unrecognized_types`, `rebuild_normalizes_singular_frontmatter_type_to_canonical_plural`).

- [ ] **Step 5: Run the full `ninox-core` suite to check for regressions**

Run: `cargo test -p ninox-core`

Expected: PASS, no regressions in existing `query_filters_by_type` or `rebuild_indexes_files` tests.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/brain.rs
git commit -m "fix(brain): normalize singular/plural category type aliases at read time"
```

---

### Task 2: Procedural fallback color for unrecognized categories (`ninox-app`)

**Files:**
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs:31` (`hash01` visibility)
- Modify: `crates/ninox-app/src/components/brain_panel.rs:25-37` (`category_color`)
- Test: `crates/ninox-app/src/components/brain_panel.rs` (`mod tests` at the bottom of the same file)

**Interfaces:**
- Consumes: `pub(crate) fn hash01(s: &str, salt: u64) -> f32` from `brain_pinboard.rs` (currently private, becomes `pub(crate)` in this task).
- Produces: `fn procedural_category_color(s: &ColorScheme, ty: &str) -> Color` and `fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color`, both private to `brain_panel.rs`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/ninox-app/src/components/brain_panel.rs` (after the existing `categories_are_counted_and_ordered_by_taxonomy` test, `brain_panel.rs:933`):

```rust
#[test]
fn category_color_known_taxonomy_types_are_unaffected_by_the_fallback() {
    let s = crate::theme::dark();
    assert_eq!(category_color(&s, "errors"), s.cat_error);
    assert_eq!(category_color(&s, "repos"), s.status_pr_open);
}

#[test]
fn category_color_falls_back_to_a_procedural_color_for_unrecognized_types() {
    let s = crate::theme::dark();
    let color = category_color(&s, "totally-new-category");
    assert_ne!(color, s.faint, "must not wash out to flat grey");
}

#[test]
fn category_color_procedural_fallback_is_deterministic() {
    let s = crate::theme::dark();
    assert_eq!(category_color(&s, "widgets"), category_color(&s, "widgets"));
}

#[test]
fn category_color_procedural_fallback_differs_across_distinct_unknown_categories() {
    let s = crate::theme::dark();
    assert_ne!(category_color(&s, "widgets"), category_color(&s, "gadgets"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox category_color -- --test-threads=1`

Expected: 2 FAIL, 2 PASS trivially (both sides of `assert_eq!`/`assert_ne!` currently resolve to `s.faint` since the fallback isn't implemented yet):
- FAIL `category_color_falls_back_to_a_procedural_color_for_unrecognized_types` (currently returns `s.faint`, equal to `s.faint`)
- FAIL `category_color_procedural_fallback_differs_across_distinct_unknown_categories` (both currently return `s.faint`, so `assert_ne!` fails)
- PASS `category_color_known_taxonomy_types_are_unaffected_by_the_fallback` (unaffected by this change)
- PASS `category_color_procedural_fallback_is_deterministic` (trivially true pre-implementation too — kept as a regression guard)

- [ ] **Step 3: Make `hash01` visible to `brain_panel.rs`**

In `crates/ninox-app/src/components/brain_pinboard.rs:31`, change:

```rust
fn hash01(s: &str, salt: u64) -> f32 {
```

to:

```rust
pub(crate) fn hash01(s: &str, salt: u64) -> f32 {
```

- [ ] **Step 4: Implement `procedural_category_color` and `hsl_to_rgb`, wire into `category_color`**

In `crates/ninox-app/src/components/brain_panel.rs`, change `category_color` (`brain_panel.rs:25-37`) from:

```rust
pub fn category_color(s: &ColorScheme, ty: &str) -> Color {
    match ty {
        "repos"         => s.status_pr_open,
        "symbols"       => s.status_working,
        "concepts"      => s.status_review,
        "architecture"  => s.status_mergeable,
        "patterns"      => s.cat_pattern,
        "decisions"     => s.cat_decision,
        "relationships" => s.cat_relationship,
        "errors"        => s.cat_error,
        _               => s.faint,
    }
}
```

to:

```rust
pub fn category_color(s: &ColorScheme, ty: &str) -> Color {
    match ty {
        "repos"         => s.status_pr_open,
        "symbols"       => s.status_working,
        "concepts"      => s.status_review,
        "architecture"  => s.status_mergeable,
        "patterns"      => s.cat_pattern,
        "decisions"     => s.cat_decision,
        "relationships" => s.cat_relationship,
        "errors"        => s.cat_error,
        _               => procedural_category_color(s, ty),
    }
}

/// Deterministic HSL-hue fallback for any category outside the 8
/// hand-picked taxonomy colors above — used instead of a flat grey so an
/// unrecognized or novel category type still reads as its own distinct
/// color. The same `ty` string always yields the same hue (via `hash01`,
/// reused from the pinboard's deterministic-layout seed), and saturation/
/// lightness are fixed per theme to stay legible against `paper`/`ink`.
fn procedural_category_color(s: &ColorScheme, ty: &str) -> Color {
    let hue = crate::components::brain_pinboard::hash01(ty, 29) * 360.0;
    let (sat, light) = if s.dark { (0.45, 0.62) } else { (0.45, 0.40) };
    hsl_to_rgb(hue, sat, light)
}

/// Standard HSL→RGB conversion (`h` in degrees `[0, 360)`, `s`/`l` in
/// `[0, 1]`), returning an opaque `iced::Color`.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Color::from_rgb(r1 + m, g1 + m, b1 + m)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p ninox category_color -- --test-threads=1`

Expected: PASS (all 4 tests).

- [ ] **Step 6: Run the full `ninox-app` suite to check for regressions**

Run: `cargo test -p ninox`

Expected: PASS, no regressions.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-app/src/components/brain_pinboard.rs crates/ninox-app/src/components/brain_panel.rs
git commit -m "fix(brain): procedural fallback color for unrecognized categories"
```

---

### Task 3: Force-directed layout pure function (`ninox-app`)

**Files:**
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs` (add `force_layout` near `resolve_edges`)
- Test: same file's `mod tests` block

**Interfaces:**
- Consumes: `fn hash01(s: &str, salt: u64) -> f32` (already in this module), `ninox_core::BrainEntry`.
- Produces: `pub(crate) fn force_layout(entries: &[BrainEntry], edges: &[(usize, usize)]) -> HashMap<String, (f32, f32)>` — each entry id's normalized `(x, y)` in `[0.05, 0.95]`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/ninox-app/src/components/brain_pinboard.rs` (after the existing `resolve_edges_drops_self_links` test, `brain_pinboard.rs:389`), reusing the existing `entry()` test helper already defined in that module:

```rust
#[test]
fn force_layout_is_deterministic() {
    let entries = vec![entry("a.md"), entry("b.md"), entry("c.md")];
    let edges = vec![(0, 1)];
    let first = force_layout(&entries, &edges);
    let second = force_layout(&entries, &edges);
    assert_eq!(first, second);
}

#[test]
fn force_layout_keeps_positions_within_the_canvas_margin() {
    let entries: Vec<BrainEntry> = (0..12).map(|i| entry(&format!("n{i}.md"))).collect();
    let edges: Vec<(usize, usize)> = (0..11).map(|i| (i, i + 1)).collect();
    let positions = force_layout(&entries, &edges);
    for (x, y) in positions.values() {
        assert!((0.05..=0.95).contains(x), "x {x} out of bounds");
        assert!((0.05..=0.95).contains(y), "y {y} out of bounds");
    }
}

#[test]
fn force_layout_pulls_linked_nodes_closer_than_an_unlinked_outlier() {
    // A tightly-linked triangle (a-b, b-c, a-c) plus one entirely unlinked
    // outlier — the triangle's own pairwise distances should end up
    // smaller than any distance from the outlier to the triangle.
    let entries = vec![entry("a.md"), entry("b.md"), entry("c.md"), entry("outlier.md")];
    let edges = vec![(0, 1), (1, 2), (0, 2)];
    let positions = force_layout(&entries, &edges);

    let dist = |a: &str, b: &str| {
        let (ax, ay) = positions[a];
        let (bx, by) = positions[b];
        ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
    };

    let max_triangle_dist =
        dist("a.md", "b.md").max(dist("b.md", "c.md")).max(dist("a.md", "c.md"));
    let min_outlier_dist = dist("a.md", "outlier.md")
        .min(dist("b.md", "outlier.md"))
        .min(dist("c.md", "outlier.md"));

    assert!(
        max_triangle_dist < min_outlier_dist,
        "triangle pairwise distances ({max_triangle_dist}) should be smaller than any \
         distance to the unlinked outlier ({min_outlier_dist})"
    );
}

#[test]
fn force_layout_handles_empty_and_singleton_input_without_panicking() {
    assert!(force_layout(&[], &[]).is_empty());

    let one = vec![entry("solo.md")];
    let positions = force_layout(&one, &[]);
    assert_eq!(positions.len(), 1);
    let (x, y) = positions["solo.md"];
    assert!((0.05..=0.95).contains(&x));
    assert!((0.05..=0.95).contains(&y));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox force_layout -- --test-threads=1`

Expected: FAIL to compile with `cannot find function 'force_layout' in this scope` (it doesn't exist yet).

- [ ] **Step 3: Implement `force_layout`**

Add this function to `crates/ninox-app/src/components/brain_pinboard.rs`, directly above `fn node_radius` (currently at `brain_pinboard.rs:153`):

```rust
/// Relaxation steps `force_layout` runs — fixed rather than convergence-
/// threshold-based, so the result is a pure, deterministic function of
/// `entries`/`edges` alone (no wall-clock or RNG dependency anywhere in
/// this function beyond the deterministic `hash01`-seeded start
/// positions).
const FORCE_LAYOUT_ITERATIONS: u32 = 400;

/// Fruchterman-Reingold force-directed layout: nodes repel each other,
/// linked nodes attract along `edges`, and per-iteration displacement is
/// capped by a linearly-cooling "temperature" so the system settles
/// instead of oscillating. Run once per data change (see
/// `App::refresh_brain_graph`) and cached — NOT recomputed per canvas
/// draw, unlike the rest of this module's per-frame re-derivation.
///
/// Initial positions come from the existing `hash01` (same salts the old
/// pure-scatter placement used), so re-running on unchanged
/// `entries`/`edges` reproduces bit-identical output. Returns each entry's
/// normalized `(x, y)` in `[0.05, 0.95]` on both axes (the same margin the
/// old hash-based placement used), keyed by entry id.
pub(crate) fn force_layout(
    entries: &[BrainEntry],
    edges: &[(usize, usize)],
) -> HashMap<String, (f32, f32)> {
    let n = entries.len();
    if n == 0 {
        return HashMap::new();
    }

    let mut x: Vec<f32> = entries.iter().map(|e| 0.05 + 0.90 * hash01(&e.id, 7)).collect();
    let mut y: Vec<f32> = entries.iter().map(|e| 0.05 + 0.90 * hash01(&e.id, 13)).collect();

    // Ideal spacing scales down as node count grows, so density stays
    // roughly constant regardless of how many specimens are in the graph.
    let k = 0.9 / (n as f32).sqrt();
    let mut temperature = 0.1f32;
    let cooling = temperature / FORCE_LAYOUT_ITERATIONS as f32;

    for _ in 0..FORCE_LAYOUT_ITERATIONS {
        let mut dx = vec![0.0f32; n];
        let mut dy = vec![0.0f32; n];

        for i in 0..n {
            for j in (i + 1)..n {
                let ddx = x[i] - x[j];
                let ddy = y[i] - y[j];
                let d = (ddx * ddx + ddy * ddy).sqrt().max(1e-3);
                let f = (k * k) / d;
                let (ux, uy) = (ddx / d, ddy / d);
                dx[i] += ux * f;
                dy[i] += uy * f;
                dx[j] -= ux * f;
                dy[j] -= uy * f;
            }
        }

        for &(a, b) in edges {
            let ddx = x[b] - x[a];
            let ddy = y[b] - y[a];
            let d = (ddx * ddx + ddy * ddy).sqrt().max(1e-3);
            let f = (d * d) / k;
            let (ux, uy) = (ddx / d, ddy / d);
            dx[a] += ux * f;
            dy[a] += uy * f;
            dx[b] -= ux * f;
            dy[b] -= uy * f;
        }

        for i in 0..n {
            let mag = (dx[i] * dx[i] + dy[i] * dy[i]).sqrt().max(1e-6);
            let capped = mag.min(temperature);
            x[i] = (x[i] + (dx[i] / mag) * capped).clamp(0.05, 0.95);
            y[i] = (y[i] + (dy[i] / mag) * capped).clamp(0.05, 0.95);
        }

        temperature = (temperature - cooling).max(0.0);
    }

    entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.clone(), (x[i], y[i])))
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox force_layout -- --test-threads=1`

Expected: PASS (4 tests). If `force_layout_pulls_linked_nodes_closer_than_an_unlinked_outlier` fails, the fix is tuning `k`'s scale factor (`0.9`) or `FORCE_LAYOUT_ITERATIONS` — not the overall algorithm shape, which is the standard Fruchterman-Reingold formulation.

- [ ] **Step 5: Run the full `ninox-app` suite to check for regressions**

Run: `cargo test -p ninox`

Expected: PASS, no regressions (this task adds a new unused-until-Task-4 function — expect a `dead_code` warning, not a test failure, from `cargo test`; `cargo build` will show the same warning).

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src/components/brain_pinboard.rs
git commit -m "feat(brain): add force-directed layout pure function for the pinboard"
```

---

### Task 4: Wire the force layout into app state and the pinboard canvas

**Files:**
- Modify: `crates/ninox-app/src/app.rs:78-108` (`BrainViewState`)
- Modify: `crates/ninox-app/src/app.rs:757-775` (rename `refresh_brain_edges` → `refresh_brain_graph`, add layout computation)
- Modify: `crates/ninox-app/src/app.rs:1860,1979,2064` (call-site renames)
- Modify: `crates/ninox-app/src/app.rs` (`BrainSwitchCatalogue`'s clear block, `app.rs:2049-2055`)
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs:53-82` (`Pinboard::nodes()`)
- Test: `crates/ninox-app/src/app.rs` (`mod tests` at the bottom)

**Interfaces:**
- Consumes: `pub(crate) fn force_layout(entries: &[BrainEntry], edges: &[(usize, usize)]) -> HashMap<String, (f32, f32)>` (Task 3).
- Produces: `BrainViewState.layout: HashMap<String, (f32, f32)>`, `fn refresh_brain_graph(state: &mut Self)` (renamed from `refresh_brain_edges`).

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `crates/ninox-app/src/app.rs` (after the existing `navigate_brain_populates_pinboard_edges_from_the_index` test, `app.rs:3813`):

```rust
#[test]
fn navigate_brain_populates_pinboard_layout_from_the_index() {
    let brain_dir = tempdir().unwrap().keep();
    std::fs::create_dir_all(brain_dir.join("people")).unwrap();
    std::fs::write(
        brain_dir.join("people").join("alice.md"),
        "---\nname: Alice\n---\nManages [[bob]].",
    )
    .unwrap();
    std::fs::write(
        brain_dir.join("people").join("bob.md"),
        "---\nname: Bob\n---\nReports to [[alice]].",
    )
    .unwrap();
    let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
    brain.rebuild(None).unwrap();

    let e = test_engine();
    let m = base_with_brain(e, brain);
    assert!(m.brain_view.layout.is_empty());

    let (m2, _) = m.update(Message::NavigateBrain);
    assert_eq!(m2.brain_view.layout.len(), 2);
    for entry in &m2.brain_view.entries {
        let (x, y) = m2.brain_view.layout[&entry.id];
        assert!((0.05..=0.95).contains(&x));
        assert!((0.05..=0.95).contains(&y));
    }
}

#[test]
fn switching_catalogue_clears_and_repopulates_pinboard_layout() {
    let dir_a = tempdir().unwrap().keep();
    std::fs::create_dir_all(dir_a.join("people")).unwrap();
    std::fs::write(dir_a.join("people").join("alice.md"), "Sees [[bob]].").unwrap();
    std::fs::write(dir_a.join("people").join("bob.md"), "Sees [[alice]].").unwrap();

    let dir_b = tempdir().unwrap().keep();
    std::fs::create_dir_all(dir_b.join("people")).unwrap();
    std::fs::write(dir_b.join("people").join("carol.md"), "No links here.").unwrap();

    let brain_a = Arc::new(BrainIndex::open(&dir_a).unwrap());
    brain_a.rebuild(None).unwrap();
    BrainIndex::open(&dir_b).unwrap().rebuild(None).unwrap();

    let e = test_engine();
    let mut app = base_with_brain(e, brain_a);
    app.catalogues = vec![
        ninox_core::config::CatalogueRef { name: "default".into(), path: dir_a.clone() },
        ninox_core::config::CatalogueRef { name: "second".into(), path: dir_b.clone() },
    ];
    let (app, _) = app.update(Message::NavigateBrain);
    assert_eq!(app.brain_view.layout.len(), 2);

    let (app, _) = app.update(Message::BrainSwitchCatalogue(1));
    assert_eq!(
        app.brain_view.layout.len(),
        1,
        "catalogue B has one entry -- layout must be repopulated for it, not left over from A"
    );
    assert!(app.brain_view.layout.contains_key("people/carol.md"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox navigate_brain_populates_pinboard_layout -- --test-threads=1`

Expected: FAIL to compile with `no field 'layout' on type 'BrainViewState'` (it doesn't exist yet).

- [ ] **Step 3: Add the `layout` field to `BrainViewState`**

In `crates/ninox-app/src/app.rs`, change the end of the `BrainViewState` struct (`app.rs:102-108`) from:

```rust
    /// Pinboard edges as undirected, deduplicated node-index pairs into
    /// `entries`, resolved from `BrainIndex::links_all()` once per data
    /// change (`NavigateBrain` / `BrainReindex` / `BrainSwitchCatalogue`) —
    /// see `App::refresh_brain_edges` and `brain_pinboard::resolve_edges`.
    /// Never re-derived per canvas draw.
    pub edges: Vec<(usize, usize)>,
}
```

to:

```rust
    /// Pinboard edges as undirected, deduplicated node-index pairs into
    /// `entries`, resolved from `BrainIndex::links_all()` once per data
    /// change (`NavigateBrain` / `BrainReindex` / `BrainSwitchCatalogue`) —
    /// see `App::refresh_brain_graph` and `brain_pinboard::resolve_edges`.
    /// Never re-derived per canvas draw.
    pub edges: Vec<(usize, usize)>,
    /// Force-directed pinboard layout: each entry id's normalized `(x, y)`
    /// position in `[0.05, 0.95]`, computed once per data change by
    /// `brain_pinboard::force_layout` alongside `edges` (same triggers,
    /// same `App::refresh_brain_graph`) — never recomputed per canvas
    /// draw. `Pinboard::nodes()` falls back to the old hash-based position
    /// for any id missing here (e.g. a reindex race).
    pub layout: HashMap<String, (f32, f32)>,
}
```

- [ ] **Step 4: Rename `refresh_brain_edges` to `refresh_brain_graph` and compute the layout**

In `crates/ninox-app/src/app.rs`, change (`app.rs:757-775`):

```rust
    /// Re-derive pinboard edges (node-index pairs into `brain_view.entries`)
    /// from the index's resolved link graph. Called once per data change —
    /// `NavigateBrain`'s initial load, `BrainReindex`, and
    /// `BrainSwitchCatalogue` — never per canvas draw. A DB error is
    /// tolerated: warn and leave the pinboard edge-less rather than panic.
    fn refresh_brain_edges(state: &mut Self) {
        match state.brain.links_all() {
            Ok(links) => {
                state.brain_view.edges = crate::components::brain_pinboard::resolve_edges(
                    &state.brain_view.entries,
                    &links,
                );
            }
            Err(e) => {
                tracing::warn!("brain links_all: {e}");
                state.brain_view.edges.clear();
            }
        }
    }
```

to:

```rust
    /// Re-derive pinboard edges (node-index pairs into `brain_view.entries`)
    /// and the force-directed layout built from them. Called once per data
    /// change — `NavigateBrain`'s initial load, `BrainReindex`, and
    /// `BrainSwitchCatalogue` — never per canvas draw. A DB error is
    /// tolerated: warn and leave the pinboard edge-less (and layout
    /// unchanged) rather than panic.
    fn refresh_brain_graph(state: &mut Self) {
        match state.brain.links_all() {
            Ok(links) => {
                state.brain_view.edges = crate::components::brain_pinboard::resolve_edges(
                    &state.brain_view.entries,
                    &links,
                );
                state.brain_view.layout = crate::components::brain_pinboard::force_layout(
                    &state.brain_view.entries,
                    &state.brain_view.edges,
                );
            }
            Err(e) => {
                tracing::warn!("brain links_all: {e}");
                state.brain_view.edges.clear();
            }
        }
    }
```

- [ ] **Step 5: Update the three call sites**

In `crates/ninox-app/src/app.rs`, change each of these three lines from `Self::refresh_brain_edges(state);` to `Self::refresh_brain_graph(state);`:
- `app.rs:1860` (inside `Message::NavigateBrain`)
- `app.rs:1979` (inside `Message::BrainReindex`)
- `app.rs:2064` (inside `Message::BrainSwitchCatalogue`)

- [ ] **Step 6: Clear `layout` alongside `edges` in `BrainSwitchCatalogue`**

In `crates/ninox-app/src/app.rs`, inside `Message::BrainSwitchCatalogue`'s clear block, change:

```rust
                            state.brain_view.open_drawers.clear();
                            state.brain_view.backlinks.clear();
                            state.brain_view.related.clear();
                            state.brain_view.edges.clear();
                            state.brain_view.loaded = false;
```

to:

```rust
                            state.brain_view.open_drawers.clear();
                            state.brain_view.backlinks.clear();
                            state.brain_view.related.clear();
                            state.brain_view.edges.clear();
                            state.brain_view.layout.clear();
                            state.brain_view.loaded = false;
```

- [ ] **Step 7: Read from the cached layout in `Pinboard::nodes()`**

In `crates/ninox-app/src/components/brain_pinboard.rs`, change `nodes()` (`brain_pinboard.rs:53-82`) from:

```rust
        self.app
            .brain_view
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| Node {
                x: bounds.width * (0.05 + 0.90 * hash01(&e.id, 7)),
                y: bounds.height * (0.06 + 0.88 * hash01(&e.id, 13)),
                r: node_radius(degree.get(i).copied().unwrap_or(0) as f32),
                color: category_color(s, &e.entry_type),
                hit: !q.is_empty()
                    && (e.name.to_lowercase().contains(&q) || e.id.to_lowercase().contains(&q)),
                id: e.id.clone(),
            })
            .collect()
    }
```

to:

```rust
        self.app
            .brain_view
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                // `layout` is the cached force-directed position, recomputed
                // once per data change by `App::refresh_brain_graph` — a
                // cache miss (e.g. a reindex race) falls back to the old
                // hash-based scatter position rather than hiding the node.
                let (nx, ny) = self
                    .app
                    .brain_view
                    .layout
                    .get(&e.id)
                    .copied()
                    .unwrap_or_else(|| {
                        (0.05 + 0.90 * hash01(&e.id, 7), 0.06 + 0.88 * hash01(&e.id, 13))
                    });
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
            .collect()
    }
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p ninox navigate_brain_populates_pinboard_layout switching_catalogue_clears_and_repopulates_pinboard_layout -- --test-threads=1`

Expected: PASS (both tests).

- [ ] **Step 9: Run the full `ninox-app` suite to check for regressions**

Run: `cargo test -p ninox`

Expected: PASS, including the existing `navigate_brain_populates_pinboard_edges_from_the_index` and `switching_catalogue_clears_and_repopulates_pinboard_edges` tests (unaffected — `edges` behavior is unchanged, only augmented).

- [ ] **Step 10: Commit**

```bash
git add crates/ninox-app/src/app.rs crates/ninox-app/src/components/brain_pinboard.rs
git commit -m "feat(brain): cache the force-directed layout on data change, draw from it"
```

---

### Task 5: Selection stays in pinboard mode, with an in-place highlight ring

**Files:**
- Modify: `crates/ninox-app/src/app.rs:1943-1954` (`Message::BrainSelectEntry`)
- Modify: `crates/ninox-app/src/app.rs:3704-3720` (rename/update `selecting_entry_opens_catalogue_and_drawer`)
- Modify: `crates/ninox-app/src/components/brain_pinboard.rs:199-223` (`Pinboard::draw`, persistent selection ring)

**Interfaces:**
- Consumes: `app.brain_view.selected: Option<String>` (already exists).
- Produces: no new public interface — behavior change only.

- [ ] **Step 1: Update the existing test to the new expected behavior**

In `crates/ninox-app/src/app.rs`, replace the test at `app.rs:3704-3720`:

```rust
    #[test]
    fn selecting_entry_opens_catalogue_and_drawer() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("symbols")).unwrap();
        std::fs::write(brain_dir.join("symbols").join("x.md"), "---\nname: X\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let app = base_with_brain(e, brain);
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSetMode(BrainMode::Pinboard));
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(app.brain_view.mode, BrainMode::Catalogue);
        assert!(app.brain_view.open_drawers.contains("symbols"));
        assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/x.md"));
    }
```

with:

```rust
    #[test]
    fn selecting_entry_opens_its_drawer_without_changing_mode() {
        let brain_dir = tempdir().unwrap().keep();
        std::fs::create_dir_all(brain_dir.join("symbols")).unwrap();
        std::fs::write(brain_dir.join("symbols").join("x.md"), "---\nname: X\n---\nbody").unwrap();
        let brain = Arc::new(BrainIndex::open(&brain_dir).unwrap());
        brain.rebuild(None).unwrap();

        let e = test_engine();
        let app = base_with_brain(e, brain);
        let (app, _) = app.update(Message::NavigateBrain);
        let (app, _) = app.update(Message::BrainSetMode(BrainMode::Pinboard));
        let (app, _) = app.update(Message::BrainSelectEntry("symbols/x.md".into()));
        assert_eq!(
            app.brain_view.mode,
            BrainMode::Pinboard,
            "selecting a specimen must not bounce the user out of pinboard mode"
        );
        assert!(app.brain_view.open_drawers.contains("symbols"));
        assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/x.md"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ninox selecting_entry_opens_its_drawer_without_changing_mode -- --test-threads=1`

Expected: FAIL — `assertion 'left == right' failed`, left: `Catalogue`, right: `Pinboard` (today's code still force-switches mode).

- [ ] **Step 3: Remove the forced mode switch**

In `crates/ninox-app/src/app.rs`, change `Message::BrainSelectEntry` (`app.rs:1943-1954`) from:

```rust
            Message::BrainSelectEntry(id) => {
                if let Some(e) = state.brain_view.entries.iter().find(|e| e.id == id) {
                    state.brain_view.markdown = iced::widget::markdown::parse(
                        &crate::components::brain_panel::preprocess_wikilinks(&e.body),
                    ).collect();
                    state.brain_view.open_drawers.insert(e.entry_type.clone());
                    state.brain_view.mode = BrainMode::Catalogue;
                }
                state.brain_view.selected = Some(id);
                Self::refresh_selection_graph(state);
                Task::none()
            }
```

to:

```rust
            Message::BrainSelectEntry(id) => {
                if let Some(e) = state.brain_view.entries.iter().find(|e| e.id == id) {
                    state.brain_view.markdown = iced::widget::markdown::parse(
                        &crate::components::brain_panel::preprocess_wikilinks(&e.body),
                    ).collect();
                    state.brain_view.open_drawers.insert(e.entry_type.clone());
                }
                state.brain_view.selected = Some(id);
                Self::refresh_selection_graph(state);
                Task::none()
            }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ninox selecting_entry_opens_its_drawer_without_changing_mode -- --test-threads=1`

Expected: PASS.

- [ ] **Step 5: Add the persistent selection ring to the canvas**

In `crates/ninox-app/src/components/brain_pinboard.rs`, inside `Pinboard::draw`'s per-node loop (`brain_pinboard.rs:201-223`), change:

```rust
            // Hover ring: full-alpha ink, +3px — deliberately narrower and a
            // different color from the +4px vermilion search-hit ring above
            // so the two states never read as the same thing.
            if state.hovered.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 3.0),
                    Stroke::default().with_color(Color { a: 1.0, ..s.ink }).with_width(1.4),
                );
            }
        }
```

to:

```rust
            // Hover ring: full-alpha ink, +3px — deliberately narrower and a
            // different color from the +4px vermilion search-hit ring above
            // so the two states never read as the same thing.
            if state.hovered.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 3.0),
                    Stroke::default().with_color(Color { a: 1.0, ..s.ink }).with_width(1.4),
                );
            }
            // Persistent selection ring: dashed +6px accent — distinct from
            // both the solid +4px search-hit ring and the solid +3px hover
            // ring above, so all three states stay visually distinguishable
            // even when they overlap (e.g. hovering the selected node).
            // Unlike hover (transient, cursor-driven `state.hovered`),
            // selection comes from `self.app.brain_view.selected`, set by a
            // canvas click OR a sidebar drawer click, and persists until a
            // different entry is selected.
            if self.app.brain_view.selected.as_deref() == Some(n.id.as_str()) {
                frame.stroke(
                    &Path::circle(Point::new(n.x, n.y), n.r + 6.0),
                    Stroke {
                        style: canvas::Style::Solid(s.accent),
                        width: 1.4,
                        line_dash: canvas::LineDash { segments: &[3.0, 3.0], offset: 0 },
                        ..Stroke::default()
                    },
                );
            }
        }
```

- [ ] **Step 6: Run the full `ninox-app` suite to check for regressions**

Run: `cargo test -p ninox`

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-app/src/app.rs crates/ninox-app/src/components/brain_pinboard.rs
git commit -m "fix(brain): selecting a specimen stays in pinboard mode with an in-place ring"
```

---

### Task 6: Reuse the catalogue's drawer rail in pinboard mode

**Files:**
- Modify: `crates/ninox-app/src/components/brain_panel.rs:447-510` (`pinboard_body`)

**Interfaces:**
- Consumes: `fn drawers_rail(app: &App) -> Element<'_, Message>` (already defined at `brain_panel.rs:525`, used today only by `catalogue_body`).

- [ ] **Step 1: Replace the flat category-count rail with `drawers_rail`**

In `crates/ninox-app/src/components/brain_panel.rs`, change `pinboard_body` (`brain_panel.rs:447-510`) from:

```rust
fn pinboard_body(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let (card_a, _, _) = shadow_alpha(s);

    let cat_rows: Vec<Element<Message>> = categories(&app.brain_view.entries)
        .into_iter()
        .map(|(ty, n)| {
            let color = category_color(s, &ty);
            container(
                row![
                    text("●").size(9).color(color),
                    Space::new(10, 0),
                    text(ty).size(12).color(s.ink_2),
                    Space::new(Length::Fill, 0),
                    text(n.to_string()).size(9.5).font(MONO).color(s.faint),
                ]
                .align_y(Alignment::Center),
            )
            .padding([3, 16])
            .width(Length::Fill)
            .into()
        })
        .collect();

    let rail = container(column![
        volume_plate(app),
        container(text("brain/ — taxonomy").size(14).font(SERIF_ITALIC).color(s.faint))
            .padding([10, 16]),
        scrollable(column(cat_rows)).height(Length::Fill),
    ])
    .width(Length::Fixed(215.0))
    .height(Length::Fill)
    .style(move |_theme| container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
        shadow: hard_shadow(s, 2.0, 3.0, card_a),
        ..Default::default()
    });

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

    row![rail, board]
        .spacing(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding { top: 16.0, right: 28.0, bottom: 22.0, left: 28.0 })
        .into()
}
```

to:

```rust
fn pinboard_body(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    let board_frame = container(super::brain_pinboard::pinboard_canvas(app))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_theme| crate::style::heavy_frame(s));

    // Preview slip shows whichever is active: a live hover takes precedence
    // over a persisted selection (e.g. from a drawer click), so moving the
    // mouse over a different node always reflects what's under the cursor;
    // otherwise the selected entry's slip stays up as the "highlight in
    // place" feedback for a drawer-driven selection. Either id may no
    // longer exist (a reindex/catalogue switch can drop or rename entries
    // out from under a stale hover/selection) — resolve defensively and
    // simply skip the slip rather than panicking or showing stale content.
    let display_id = app.brain_view.hovered.clone().or_else(|| app.brain_view.selected.clone());
    let display_entry =
        display_id.as_deref().and_then(|id| app.brain_view.entries.iter().find(|e| e.id == id));
    let board: Element<Message> = match display_entry {
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

- [ ] **Step 2: Build and run the full `ninox-app` suite**

Run: `cargo build -p ninox && cargo test -p ninox`

Expected: builds cleanly (no unused-import warnings — `shadow_alpha`, `hard_shadow`, `Background`, `Border`, `column`, `scrollable` all stay in use elsewhere in this file, e.g. `hover_preview_slip` and `drawers_rail` itself); all tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ninox-app/src/components/brain_panel.rs
git commit -m "fix(brain): pinboard mode reuses the catalogue's drawer rail"
```

---

### Task 7: Manual end-to-end verification

**Files:** none (verification only)

- [ ] **Step 1: Launch the app**

Run: `cargo run -p ninox` from the workspace root, then navigate to the Brain view (key `3` or the sidebar nav).

- [ ] **Step 2: Verify the pinboard**

- Confirm the pinboard's left rail now shows expandable category drawers (matching catalogue mode's rail), not a flat count list.
- Confirm specimens visually cluster instead of scattering uniformly — categories should read as loose neighborhoods, not a uniform mesh.
- Confirm no specimen renders as flat grey (categories that used to show as duplicate singular/plural rows, e.g. `errors`/`error`, should now be a single merged drawer).

- [ ] **Step 3: Verify selection behavior**

- With the pinboard showing, click an entry inside an expanded drawer. Confirm: the view stays on the pinboard (does not jump to catalogue), the corresponding node gets a dashed ring, and the preview slip shows that entry's content.
- Click a different node directly on the canvas. Confirm: the ring moves to the new node, the view still stays on the pinboard.
- Switch to Catalogue mode (`☰`) and click a drawer entry there. Confirm: this still opens the reading pane as before (unaffected).

- [ ] **Step 4: Report results in the PR description or follow-up commit message if any of the above fail**

No code changes in this step unless a manual check surfaces a regression — if so, return to the relevant task above and fix before proceeding.

---

## Self-Review Notes

- **Spec coverage:** Category normalization + procedural fallback (spec §1) → Tasks 1–2. Force-directed layout, cached per data change (spec §2) → Tasks 3–4. Unified sidebar + in-place selection highlight (spec §3) → Tasks 5–6. Manual verification closes the loop since several of these are rendering-level changes with no automated visual test, consistent with this codebase's existing test coverage (pure logic is unit tested; `iced` rendering is not).
- **Placeholder scan:** no TBD/TODO; the one place numeric tuning could need adjustment (Task 3, Step 4) is called out explicitly with what to change, not left vague.
- **Type consistency:** `force_layout`'s signature (`&[BrainEntry], &[(usize, usize)]) -> HashMap<String, (f32, f32)>`) is identical everywhere it's referenced (Task 3's Interfaces, Task 4's Interfaces and Step 7 call site). `refresh_brain_graph` is renamed consistently across its definition (Task 4 Step 4) and all three call sites (Task 4 Step 5) with no lingering `refresh_brain_edges` references (including the doc comment on `BrainViewState::edges`, updated in Task 4 Step 3).
