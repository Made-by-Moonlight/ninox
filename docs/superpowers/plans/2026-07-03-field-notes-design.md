# Field Notes Design Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reskin the entire ninox Iced app to the "Field Notes" design (warm paper/ink field-journal aesthetic) and add the two features the design introduces: an enriched Spawn modal and a brand-new Brain view (pinboard graph + catalogue reading pane).

**Architecture:** All colors move into a rewritten `ColorScheme` token struct (`theme.rs`) using the Field Notes vocabulary (paper/ink/rule/accent + status + brain-category + terminal tokens). A new `style.rs` module holds the three bundled font families and shared style helpers (hard offset shadows, cards, stamps, micro-labels) so components stop re-declaring styles inline. Each view is then restyled component-by-component. The Brain view builds on the basic brain browser that already exists on main (`components/brain_panel.rs`, `View::Brain`, `BrainViewState`, `App.brain: Arc<BrainIndex>` — added by PR #5): Tasks 11–13 extend it with Pinboard/Catalogue modes, wikilinks, markdown rendering, and the canvas graph.

**Base branch note:** This branches from `origin/main` (829f16e). Open PRs #6/#7 rewrite terminal internals — keep Tasks 7–8's `terminal.rs` edits strictly color/chrome-localized so eventual merges stay tractable. Line numbers cited from the exploration may drift a little on main; locate code by symbol name, not line.

**Tech Stack:** Rust, iced 0.13 (features: tokio, canvas, advanced, wgpu — plus new `markdown`), alacritty_terminal 0.26, rusqlite-backed BrainIndex, chrono (new dep), bundled Google Fonts (Newsreader, Archivo, Spline Sans Mono — all OFL).

**Spec:** `docs/design-concepts/field-notes-design.md` (implementation handoff) and `docs/design-concepts/03-field-notes.html` (pixel reference, exact CSS values). Read both before starting any task.

## Global Constraints

- Design tokens are EXACT hex values from spec §1 — copy them verbatim, never approximate.
- Corner radius 2–3px everywhere; tag chips only may be 14px. Nothing else pill-shaped.
- Shadows are hard offsets, `blur_radius: 0.0` ALWAYS. Light theme shadow color `rgba(33,29,22,α)`, dark `rgba(0,0,0,α)`.
- Emoji-free UI. The only permitted glyphs: `⬡` (logo), `⚑` (attention), `⌕` (search), `✦`/`☰` (brain modes), plus `←`/`×`/`▸`/`▾`/`✓`/`✗`/`◌`/`·`/`№` as text.
- Typography: Newsreader (serif) = display/titles/tabs/card names; Archivo (sans) = UI labels/buttons/body; Spline Sans Mono = data (repos, costs, timestamps, IDs, terminal, frontmatter). iced has no letter-spacing — approximate micro-labels with `.to_uppercase()` + bold + small size only.
- iced limitations accepted by spec §8: no stamp rotation (keep border/typography), no paper-grain texture (skip, it's polish), hover "lift" approximated by growing the shadow on `button::Status::Hovered`.
- Do NOT touch the terminal data pipeline (`TerminalState::process`, PTY streaming, scrollback logic) — only its colors/chrome.
- Keep all existing tests green. `cargo test -p ninox` after every task.
- Conventional commits, no co-authors.
- Status semantic mapping is preserved: green=working, blue=PR open, red=CI failed, yellow=review, purple=mergeable, grey=done/terminated.
- The `ThemeVariant::Ninox` third theme is NOT designed yet (spec §7) — map it to `dark()` for now.

## Execution Setup (before Task 1)

Work in an isolated worktree per `superpowers:using-git-worktrees` (ALREADY CREATED at `/Users/ethan.brodie/slievr/ninox/.claude/worktrees/field-notes-design`, branch `feat/field-notes-design` off `origin/main`). All commands in this plan run from that worktree root.

The design docs are currently UNTRACKED in the main checkout. First commit in the worktree:

```bash
cp -R /Users/ethan.brodie/slievr/ninox/docs/design-brief.md docs/
cp -R /Users/ethan.brodie/slievr/ninox/docs/design-concepts docs/
cp /Users/ethan.brodie/slievr/ninox/docs/superpowers/plans/2026-07-03-field-notes-design.md docs/superpowers/plans/
git add docs/ && git commit -m "docs(design): field notes design spec, concepts, and implementation plan"
```

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `crates/ninox-app/assets/fonts/*.ttf` | Create | Newsreader (roman+italic var), Archivo (var), Spline Sans Mono (var) + OFL licenses |
| `crates/ninox-app/src/theme.rs` | Rewrite | `ColorScheme` token struct, `light()`/`dark()` constructors — colors ONLY |
| `crates/ninox-app/src/style.rs` | Create | Font constants, shadow/card/frame/stamp/micro-label/hline helpers, `stamp_word()` |
| `crates/ninox-app/src/main.rs` | Modify | Font bundling + default font; pass `BrainIndex` to App |
| `crates/ninox-app/src/app.rs` | Modify | token renames, `last_session`, keyboard shortcuts, `View::Brain`, brain+spawn messages |
| `crates/ninox-app/src/components/sidebar.rs` | Rewrite | Masthead, TOC nav, action row, session tree, theme-dots footer |
| `crates/ninox-app/src/components/filter_bar.rs` | Rewrite | Underlined `⌕` filter field for the folio row |
| `crates/ninox-app/src/components/fleet_board.rs` | Rewrite | Folio header, attention banner, ledger columns, stamped cards |
| `crates/ninox-app/src/components/session_detail.rs` | Modify | Header, italic-serif tabs, terminal chrome, layout (keep terminal wiring) |
| `crates/ninox-app/src/components/terminal.rs` | Modify | Warm ANSI palette, theme-driven selection color (colors only) |
| `crates/ninox-app/src/components/info_panel.rs` | Modify | "Marginalia" panel styling |
| `crates/ninox-app/src/components/inspector_panel.rs` | Modify | Uppercase micro-label + mono value kv sheet |
| `crates/ninox-app/src/components/pr_list.rs` | Rewrite | Heavy-framed ledger table |
| `crates/ninox-app/src/components/notification_panel.rs` | Modify | Journal-margin styling (spec §7 language) |
| `crates/ninox-app/src/components/spawn_modal.rs` | Rewrite | Enriched journal-entry modal (5 fields) |
| `crates/ninox-app/src/components/brain_panel.rs` | Rewrite | Catalogue drawers + reading pane, wikilink/backlink helpers (extends existing brain browser) |
| `crates/ninox-app/src/components/brain_pinboard.rs` | Create | Canvas specimen-board graph |
| `crates/ninox-app/Cargo.toml` | Modify | `+chrono`, iced `+markdown` feature |

**Scope note:** Tasks 1–10 (restyle + modal) and Tasks 11–13 (Brain view) are separable subsystems. Tasks 1–10 produce a complete working restyle on their own; stop there if scope must shrink.

---

### Task 1: Bundle the three font families

**Files:**
- Create: `crates/ninox-app/assets/fonts/Newsreader[opsz,wght].ttf`, `Newsreader-Italic[opsz,wght].ttf`, `Archivo[wdth,wght].ttf`, `SplineSansMono[wght].ttf`, `OFL-Newsreader.txt`, `OFL-Archivo.txt`, `OFL-SplineSansMono.txt`
- Modify: `crates/ninox-app/src/main.rs:316-324`

**Interfaces:**
- Produces: fonts registered under family names `"Newsreader"`, `"Archivo"`, `"Spline Sans Mono"`; Archivo becomes the app default font. Later tasks reference these via `crate::style::{SERIF, SERIF_ITALIC, SANS, SANS_BOLD, MONO}` (defined in Task 3).

- [ ] **Step 1: Download the variable TTFs + licenses from the google/fonts repo**

```bash
cd crates/ninox-app/assets/fonts
BASE=https://raw.githubusercontent.com/google/fonts/main/ofl
curl -fsSL -o 'Newsreader[opsz,wght].ttf'        "$BASE/newsreader/Newsreader%5Bopsz%2Cwght%5D.ttf"
curl -fsSL -o 'Newsreader-Italic[opsz,wght].ttf' "$BASE/newsreader/Newsreader-Italic%5Bopsz%2Cwght%5D.ttf"
curl -fsSL -o 'Archivo[wdth,wght].ttf'           "$BASE/archivo/Archivo%5Bwdth%2Cwght%5D.ttf"
curl -fsSL -o 'SplineSansMono[wght].ttf'         "$BASE/splinesansmono/SplineSansMono%5Bwght%5D.ttf"
curl -fsSL -o OFL-Newsreader.txt      "$BASE/newsreader/OFL.txt"
curl -fsSL -o OFL-Archivo.txt         "$BASE/archivo/OFL.txt"
curl -fsSL -o OFL-SplineSansMono.txt  "$BASE/splinesansmono/OFL.txt"
ls -la   # every .ttf must be > 100 KB; a tiny file means a 404 HTML page — re-check the URL
```

- [ ] **Step 2: Register the fonts in `main.rs`**

Find the existing block (main.rs:316-324):

```rust
const SYMBOLS_NERD_FONT_MONO: &[u8] =
    include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");
```

Extend to:

```rust
const SYMBOLS_NERD_FONT_MONO: &[u8] =
    include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular.ttf");
const FONT_NEWSREADER: &[u8] =
    include_bytes!("../assets/fonts/Newsreader[opsz,wght].ttf");
const FONT_NEWSREADER_ITALIC: &[u8] =
    include_bytes!("../assets/fonts/Newsreader-Italic[opsz,wght].ttf");
const FONT_ARCHIVO: &[u8] =
    include_bytes!("../assets/fonts/Archivo[wdth,wght].ttf");
const FONT_SPLINE_SANS_MONO: &[u8] =
    include_bytes!("../assets/fonts/SplineSansMono[wght].ttf");
```

and chain onto the application builder next to the existing `.font(SYMBOLS_NERD_FONT_MONO)` call:

```rust
.font(SYMBOLS_NERD_FONT_MONO)
.font(FONT_NEWSREADER)
.font(FONT_NEWSREADER_ITALIC)
.font(FONT_ARCHIVO)
.font(FONT_SPLINE_SANS_MONO)
.default_font(iced::Font::with_name("Archivo"))
```

- [ ] **Step 3: Build and launch to verify fonts render**

Run: `cargo build -p ninox` → expect success. Then `cargo run -p ninox` briefly — all UI text should render in Archivo (visibly different from the previous default).

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-app/assets/fonts crates/ninox-app/src/main.rs
git commit -m "feat(native-app): bundle Newsreader, Archivo, Spline Sans Mono fonts"
```

---

### Task 2: Field Notes color tokens

**Files:**
- Rewrite: `crates/ninox-app/src/theme.rs`
- Modify: every file referencing old token names (`app.rs`, all `components/*.rs`)

**Interfaces:**
- Produces: `ColorScheme` with fields `paper, paper_2, card, ink, ink_2, faint, rule, rule_dark, accent, shadow, status_working, status_pr_open, status_ci_failed, status_review, status_mergeable, status_done, cat_pattern, cat_decision, cat_relationship, cat_error, term_bg, term_bar, term_bar_border, term_fg, term_ok, term_err, term_agent, term_dim` (all `iced::Color`); `status_color(&SessionStatus) -> Color`; `iced_theme() -> Theme`; free fns `from_variant`, `light`, `dark`.
- All later tasks consume these exact field names.

- [ ] **Step 1: Rewrite `theme.rs` in full**

```rust
use ninox_core::{types::SessionStatus, ThemeVariant};
use iced::{color, Color, Theme};

/// Field Notes design tokens — spec: docs/design-concepts/field-notes-design.md §1.
/// The dark theme is the same journal read by lamplight, not a separate design.
#[derive(Debug, Clone, Copy)]
pub struct ColorScheme {
    // surfaces & ink
    pub paper:     Color, // app background
    pub paper_2:   Color, // sidebar, modal header, table header
    pub card:      Color, // cards, panels, modals, reading pane
    pub ink:       Color, // primary text, heavy borders
    pub ink_2:     Color, // secondary text
    pub faint:     Color, // tertiary/metadata text
    pub rule:      Color, // light rules/separators
    pub rule_dark: Color, // stronger rules, input underlines, card borders
    pub accent:    Color, // vermilion
    pub shadow:    Color, // hard-offset shadow base (alpha applied per-use)
    // status
    pub status_working:   Color,
    pub status_pr_open:   Color,
    pub status_ci_failed: Color,
    pub status_review:    Color,
    pub status_mergeable: Color,
    pub status_done:      Color,
    // brain categories beyond the status palette
    pub cat_pattern:      Color,
    pub cat_decision:     Color,
    pub cat_relationship: Color,
    pub cat_error:        Color,
    // terminal — "the dark object" on the page
    pub term_bg:         Color,
    pub term_bar:        Color,
    pub term_bar_border: Color,
    pub term_fg:         Color,
    pub term_ok:         Color,
    pub term_err:        Color,
    pub term_agent:      Color,
    pub term_dim:        Color,
}

impl ColorScheme {
    pub fn status_color(&self, status: &SessionStatus) -> Color {
        use SessionStatus::*;
        match status {
            Spawning | Working => self.status_working,
            PrOpen             => self.status_pr_open,
            CiFailed           => self.status_ci_failed,
            ReviewPending      => self.status_review,
            Mergeable          => self.status_mergeable,
            Done | Terminated  => self.status_done,
        }
    }

    pub fn iced_theme(&self) -> Theme {
        Theme::custom(
            "Ninox".into(),
            iced::theme::Palette {
                background: self.paper,
                text:       self.ink,
                primary:    self.accent,
                success:    self.status_working,
                danger:     self.status_ci_failed,
            },
        )
    }
}

pub fn from_variant(v: ThemeVariant) -> ColorScheme {
    match v {
        ThemeVariant::Light => light(),
        ThemeVariant::Dark  => dark(),
        // "ninox" third theme is not yet designed (spec §7) — lamplight for now.
        ThemeVariant::Ninox => dark(),
    }
}

pub fn light() -> ColorScheme {
    ColorScheme {
        paper:     color!(0xf5f0e4),
        paper_2:   color!(0xefe8d8),
        card:      color!(0xfbf7ee),
        ink:       color!(0x211d16),
        ink_2:     color!(0x5b5344),
        faint:     color!(0x968a72),
        rule:      color!(0xd9cfba),
        rule_dark: color!(0xb7ab90),
        accent:    color!(0xc8451f),
        shadow:    color!(0x211d16),
        status_working:   color!(0x3e7d34),
        status_pr_open:   color!(0x20629e),
        status_ci_failed: color!(0xc8451f),
        status_review:    color!(0xa97913),
        status_mergeable: color!(0x6d4fa3),
        status_done:      color!(0x8b8272),
        cat_pattern:      color!(0xa23f8c),
        cat_decision:     color!(0xc86a1f),
        cat_relationship: color!(0x2a8a80),
        cat_error:        color!(0xb3261e),
        term_bg:         color!(0x23201a),
        term_bar:        color!(0x2c2822),
        term_bar_border: color!(0x3a352c),
        term_fg:         color!(0xece4d0),
        term_ok:         color!(0x8fd37f),
        term_err:        color!(0xf08a72),
        term_agent:      color!(0xf0c069),
        term_dim:        color!(0x7a7260),
    }
}

pub fn dark() -> ColorScheme {
    ColorScheme {
        paper:     color!(0x171410),
        paper_2:   color!(0x1f1b15),
        card:      color!(0x262119),
        ink:       color!(0xece3cd),
        ink_2:     color!(0xb5a98d),
        faint:     color!(0x83775c),
        rule:      color!(0x393227),
        rule_dark: color!(0x4e4534),
        accent:    color!(0xe06038),
        shadow:    color!(0x000000),
        status_working:   color!(0x7cc46a),
        status_pr_open:   color!(0x5ca8e8),
        status_ci_failed: color!(0xe86a4c),
        status_review:    color!(0xd8a83c),
        status_mergeable: color!(0xa184d6),
        status_done:      color!(0x7d7461),
        cat_pattern:      color!(0xc876b4),
        cat_decision:     color!(0xe08a4a),
        cat_relationship: color!(0x4ab0a4),
        cat_error:        color!(0xe0604a),
        term_bg:         color!(0x100d09),
        term_bar:        color!(0x191510),
        term_bar_border: color!(0x2c261d),
        term_fg:         color!(0xece4d0),
        term_ok:         color!(0x8fd37f),
        term_err:        color!(0xf08a72),
        term_agent:      color!(0xf0c069),
        term_dim:        color!(0x7a7260),
    }
}
```

Note: `warm_dark()` is deleted. `sidebar.rs:317-319` references `crate::theme::warm_dark()` — that block is rewritten in Task 4, but for THIS task to compile, apply the mechanical rename below which includes pointing that reference at `dark()`.

- [ ] **Step 2: Mechanically rename old token fields across the crate**

Mapping (old → new):

| Old | New |
|---|---|
| `bg_base` | `paper` |
| `bg_sidebar` | `paper_2` |
| `bg_surface` | `card` |
| `bg_elevated` | `card` |
| `border` | `rule_dark` |
| `text_primary` | `ink` |
| `text_secondary` | `ink_2` |
| `text_muted` | `faint` |
| `terminal_bg` | `term_bg` |
| `terminal_fg` | `term_fg` |
| `status_green` | `status_working` |
| `status_blue` | `status_pr_open` |
| `status_red` | `status_ci_failed` |
| `status_yellow` | `status_review` |
| `status_purple` | `status_mergeable` |
| `status_grey` | `status_done` |
| `warm_dark()` | `dark()` |

```bash
cd crates/ninox-app/src
for f in app.rs components/*.rs; do
  sed -i '' \
    -e 's/\.bg_base/.paper/g' -e 's/\.bg_sidebar/.paper_2/g' \
    -e 's/\.bg_surface/.card/g' -e 's/\.bg_elevated/.card/g' \
    -e 's/\.border\b/.rule_dark/g' \
    -e 's/\.text_primary/.ink/g' -e 's/\.text_secondary/.ink_2/g' -e 's/\.text_muted/.faint/g' \
    -e 's/\.terminal_bg/.term_bg/g' -e 's/\.terminal_fg/.term_fg/g' \
    -e 's/status_green/status_working/g' -e 's/status_blue/status_pr_open/g' \
    -e 's/status_red/status_ci_failed/g' -e 's/status_yellow/status_review/g' \
    -e 's/status_purple/status_mergeable/g' -e 's/status_grey/status_done/g' \
    -e 's/warm_dark()/dark()/g' \
    "$f"
done
```

CAUTION: `s/\.border\b/.rule_dark/g` also matches iced's `Border` struct field accesses like `style.border` if any exist — after the sed, `cargo build` and fix any collateral by hand (`border:` struct-literal fields are untouched because of the leading dot in the pattern; the risky ones are reads like `s.border` which are exactly what we want, but check e.g. `Border::default().border` doesn't exist — it doesn't).

- [ ] **Step 3: Build + run tests**

Run: `cargo build -p ninox && cargo test -p ninox`
Expected: builds clean; all existing tests PASS (they assert state, not colors). Launch `cargo run -p ninox` — the app should now be visibly paper/ink colored (layout still old — that's expected).

- [ ] **Step 4: Commit**

```bash
git add -A crates/ninox-app/src
git commit -m "feat(native-app): field notes color tokens replace old color scheme"
```

---

### Task 3: `style.rs` — fonts + shared style helpers

**Files:**
- Create: `crates/ninox-app/src/style.rs`
- Modify: `crates/ninox-app/src/main.rs` (add `mod style;` next to `mod theme;` — check whether modules are declared in `main.rs` or `lib.rs` and add alongside)

**Interfaces:**
- Produces (consumed by every later task):
  - `pub const SERIF/SERIF_ITALIC/SERIF_MEDIUM/SERIF_MEDIUM_ITALIC/SANS/SANS_BOLD/MONO/MONO_MEDIUM: Font`
  - `pub fn hard_shadow(s: &ColorScheme, dx: f32, dy: f32, alpha: f32) -> iced::Shadow`
  - `pub fn card_style(s: &ColorScheme) -> container::Style` — card bg, 1px rule_dark, radius 2, shadow (2,3)
  - `pub fn heavy_frame(s: &ColorScheme) -> container::Style` — card bg, 2px ink, radius 3, shadow (4,5)
  - `pub fn stamp<'a, M: 'a>(word: &str, color: Color) -> Element<'a, M>`
  - `pub fn stamp_word(status: &SessionStatus) -> &'static str`
  - `pub fn micro_label<'a>(t: &str, color: Color) -> iced::widget::Text<'a>`
  - `pub fn hline<'a, M: 'a>(color: Color, height: f32) -> Element<'a, M>`
  - `pub fn dotted_rule<'a, M: 'a>(color: Color) -> Element<'a, M>`
  - `pub fn shadow_alpha(s: &ColorScheme) -> (f32, f32, f32)` — (card, hero, modal) alphas: light (0.12, 0.18, 0.30), dark (0.50, 0.55, 0.65)

- [ ] **Step 1: Write the failing test** (inside `style.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::SessionStatus;

    #[test]
    fn stamps_say_a_word_not_the_enum_name() {
        assert_eq!(stamp_word(&SessionStatus::Working),       "Working");
        assert_eq!(stamp_word(&SessionStatus::Spawning),      "Working");
        assert_eq!(stamp_word(&SessionStatus::PrOpen),        "PR Open");
        assert_eq!(stamp_word(&SessionStatus::CiFailed),      "Failed");
        assert_eq!(stamp_word(&SessionStatus::ReviewPending), "Awaiting");
        assert_eq!(stamp_word(&SessionStatus::Mergeable),     "Ready");
        assert_eq!(stamp_word(&SessionStatus::Done),          "Filed");
        assert_eq!(stamp_word(&SessionStatus::Terminated),    "Closed");
    }

    #[test]
    fn hard_shadows_never_blur() {
        let s = crate::theme::light();
        assert_eq!(hard_shadow(&s, 2.0, 3.0, 0.12).blur_radius, 0.0);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ninox style::` — Expected: FAIL (module/functions don't exist).

- [ ] **Step 3: Implement `style.rs`**

```rust
//! Field Notes shared styling: fonts, hard offset shadows, cards, stamps.
//! Spec: docs/design-concepts/field-notes-design.md §2–3.

use iced::font::{Family, Stretch, Style as FontStyle, Weight};
use iced::widget::{container, text, Space};
use iced::{Background, Border, Color, Element, Font, Length, Shadow, Vector};
use ninox_core::types::SessionStatus;

use crate::theme::ColorScheme;

// ── Typography: three families, three jobs ─────────────────────────────────
pub const SERIF: Font = Font {
    family: Family::Name("Newsreader"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const SERIF_MEDIUM: Font = Font { weight: Weight::Medium, ..SERIF };
pub const SERIF_ITALIC: Font = Font { style: FontStyle::Italic, ..SERIF };
pub const SERIF_MEDIUM_ITALIC: Font =
    Font { weight: Weight::Medium, style: FontStyle::Italic, ..SERIF };
pub const SANS: Font = Font {
    family: Family::Name("Archivo"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const SANS_BOLD: Font = Font { weight: Weight::Bold, ..SANS };
pub const MONO: Font = Font {
    family: Family::Name("Spline Sans Mono"),
    weight: Weight::Normal, stretch: Stretch::Normal, style: FontStyle::Normal,
};
pub const MONO_MEDIUM: Font = Font { weight: Weight::Medium, ..MONO };

// ── Hard offset shadows: no blur, ever ─────────────────────────────────────
/// (card, hero, modal) shadow alphas for the active theme.
pub fn shadow_alpha(s: &ColorScheme) -> (f32, f32, f32) {
    // dark() uses pure-black shadows; light() uses ink-tinted ones.
    if s.shadow == Color::BLACK { (0.50, 0.55, 0.65) } else { (0.12, 0.18, 0.30) }
}

pub fn hard_shadow(s: &ColorScheme, dx: f32, dy: f32, alpha: f32) -> Shadow {
    Shadow {
        color: Color { a: alpha, ..s.shadow },
        offset: Vector::new(dx, dy),
        blur_radius: 0.0,
    }
}

// ── Object styles ───────────────────────────────────────────────────────────
/// Card: 1px rule-dark border, radius 2, 2×3 offset shadow.
pub fn card_style(s: &ColorScheme) -> container::Style {
    let (card_a, _, _) = shadow_alpha(s);
    container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
        shadow: hard_shadow(s, 2.0, 3.0, card_a),
        ..Default::default()
    }
}

/// Hero object: 2px ink border, radius 3, 4×5 offset shadow.
pub fn heavy_frame(s: &ColorScheme) -> container::Style {
    let (_, hero_a, _) = shadow_alpha(s);
    container::Style {
        background: Some(Background::Color(s.card)),
        border: Border { color: s.ink, width: 2.0, radius: 3.0.into() },
        shadow: hard_shadow(s, 4.0, 5.0, hero_a),
        ..Default::default()
    }
}

// ── Rubber stamps ───────────────────────────────────────────────────────────
/// Stamps say a *word*, not the enum name (spec §3).
pub fn stamp_word(status: &SessionStatus) -> &'static str {
    use SessionStatus::*;
    match status {
        Spawning | Working => "Working",
        PrOpen             => "PR Open",
        CiFailed           => "Failed",
        ReviewPending      => "Awaiting",
        Mergeable          => "Ready",
        Done               => "Filed",
        Terminated         => "Closed",
    }
}

/// Uppercase, 8.5px, bold, 1.5px border in the status color.
/// (iced can't rotate widgets — spec §8 accepts an unrotated stamp.)
pub fn stamp<'a, M: 'a>(word: &str, color: Color) -> Element<'a, M> {
    container(text(word.to_uppercase()).size(8.5).font(SANS_BOLD).color(color))
        .padding([2, 6])
        .style(move |_| container::Style {
            border: Border { color, width: 1.5, radius: 2.0.into() },
            ..Default::default()
        })
        .into()
}

// ── Micro-labels & rules ────────────────────────────────────────────────────
/// 9–10px, 700, uppercase (letter-spacing unsupported in iced).
pub fn micro_label<'a>(t: &str, color: Color) -> iced::widget::Text<'a> {
    text(t.to_uppercase()).size(9.5).font(SANS_BOLD).color(color)
}

/// Solid horizontal rule of the given color/thickness.
pub fn hline<'a, M: 'a>(color: Color, height: f32) -> Element<'a, M> {
    container(Space::new(Length::Fill, 0))
        .width(Length::Fill)
        .height(Length::Fixed(height))
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            ..Default::default()
        })
        .into()
}

/// Dotted rule for soft separations (card footers, comment threads).
pub fn dotted_rule<'a, M: 'a>(color: Color) -> Element<'a, M> {
    container(
        text("· ".repeat(160))
            .size(9)
            .color(color)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(Length::Fill)
    .height(Length::Fixed(8.0))
    .clip(true)
    .into()
}
```

- [ ] **Step 4: Declare the module and run tests**

Add `mod style;` where `mod theme;` is declared (main.rs). Run: `cargo test -p ninox style::` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/style.rs crates/ninox-app/src/main.rs
git commit -m "feat(native-app): field notes style helpers (fonts, stamps, hard shadows)"
```

---

### Task 4: Sidebar — masthead, TOC nav, tree, theme dots

**Files:**
- Rewrite: `crates/ninox-app/src/components/sidebar.rs`
- Modify: `crates/ninox-app/src/app.rs` (add `last_session` field + `NavigateLastSession` message)
- Test: in `app.rs` tests module

**Interfaces:**
- Consumes: `crate::style::*` (Task 3 names), tokens (Task 2).
- Produces: `Message::NavigateLastSession`; `App.last_session: Option<SessionId>` maintained by the `NavigateSession` handler. Task 5 (keyboard `2`) and the TOC "II. Session" row both use `NavigateLastSession`. Sidebar keeps public fn `sidebar(app: &App) -> Element<'_, Message>`.

- [ ] **Step 1: Write the failing test** (append to `app.rs` `#[cfg(test)] mod tests`)

```rust
#[test]
fn navigate_session_records_last_session() {
    let mut app = base(engine());
    seed_session(&mut app, "sess-a", SessionStatus::Working); // use the existing session-seeding helper in this test module; if named differently, adapt
    app.update(Message::NavigateSession("sess-a".into()));
    assert_eq!(app.last_session.as_deref(), Some("sess-a"));
    app.update(Message::NavigateFleet { scope: None });
    app.update(Message::NavigateLastSession);
    assert!(matches!(&app.view, View::SessionDetail { session_id, .. } if session_id == "sess-a"));
}

#[test]
fn navigate_last_session_without_history_is_noop() {
    let mut app = base(engine());
    app.update(Message::NavigateLastSession);
    assert!(matches!(app.view, View::FleetBoard { .. }));
}
```

(Adapt fixture helper names to what the existing tests in that module actually use — read them first.)

- [ ] **Step 2: Run to verify failure** — `cargo test -p ninox navigate_last` → FAIL (no field/variant).

- [ ] **Step 3: Add state + message + handler in `app.rs`**

- `App` struct: add `pub last_session: Option<SessionId>,` after `pub view: View,`; initialize `last_session: None` in the constructor.
- `Message` enum: add `NavigateLastSession,` after `NavigateSession(SessionId),`.
- In `apply`, inside the existing `Message::NavigateSession(id)` arm, add `state.last_session = Some(id.clone());` as the first line (before the view switch). In the `SpawnFormConfirm` arm, after `state.view = View::SessionDetail { ... }` add `state.last_session = Some(orch.id.clone());`.
- New arm:

```rust
Message::NavigateLastSession => {
    if let Some(id) = state.last_session.clone() {
        if state.sessions.contains_key(&id) {
            return App::apply(state, Message::NavigateSession(id));
        }
    }
    Task::none()
}
```

- [ ] **Step 4: Run tests** — `cargo test -p ninox navigate_last` → PASS.

- [ ] **Step 5: Rewrite `sidebar.rs`**

Keep: `repo_short`, notification panel hookup, macOS traffic-light padding, `RemoveOrchestrator`/`RemoveSession`/`SelectOrchestrator` behavior, `app.sidebar_width`. Replace all rendering. Complete new file body (imports at top: add `crate::style::{self, micro_label, hline, SERIF, SERIF_ITALIC, SERIF_MEDIUM, MONO, SANS_BOLD}`):

```rust
use ninox_core::config::ThemeVariant;
use iced::{
    widget::{button, column, container, row, scrollable, text, Space},
    Alignment, Background, Border, Color, Element, Length, Padding,
};

use crate::{
    app::{App, Message, View},
    components::notification_panel::notification_panel,
    style::{hline, micro_label, MONO, SANS_BOLD, SERIF, SERIF_ITALIC, SERIF_MEDIUM},
};

fn repo_short(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

/// Status dot: filled circle, 1.5px border in the status color.
/// Done/terminated renders hollow (transparent fill).
fn status_dot(color: Color, hollow: bool) -> Element<'static, Message> {
    container(Space::new(0, 0))
        .width(Length::Fixed(8.0))
        .height(Length::Fixed(8.0))
        .style(move |_| container::Style {
            background: (!hollow).then_some(Background::Color(color)),
            border: Border { color, width: 1.5, radius: 4.0.into() },
            ..Default::default()
        })
        .into()
}

/// One table-of-contents row: roman numeral, serif label, dotted leader, key hint.
fn toc_item<'a>(
    app: &'a App,
    numeral: &'a str,
    label: &'a str,
    key: &'a str,
    msg: Message,
    active: bool,
) -> Element<'a, Message> {
    let s = &app.scheme;
    let bar_color = if active { s.accent } else { Color::TRANSPARENT };
    let rn_color = if active { s.accent } else { s.faint };
    let lbl_color = if active { s.ink } else { s.ink_2 };
    let lbl_font = if active { SERIF_MEDIUM } else { SERIF };

    button(
        row![
            container(Space::new(0, 0)).width(3).height(Length::Fixed(18.0)).style(
                move |_| container::Style {
                    background: Some(Background::Color(bar_color)),
                    ..Default::default()
                }
            ),
            Space::new(15, 0),
            text(numeral).size(12).font(SERIF_ITALIC).color(rn_color).width(Length::Fixed(22.0)),
            text(label).size(15).font(lbl_font).color(lbl_color),
            container(
                text("· ".repeat(40)).size(9).color(s.rule_dark)
                    .wrapping(iced::widget::text::Wrapping::None)
            ).width(Length::Fill).height(Length::Fixed(10.0)).clip(true).padding(Padding { top: 6.0, right: 4.0, bottom: 0.0, left: 6.0 }),
            text(key).size(9).font(MONO).color(s.faint),
        ]
        .align_y(Alignment::Center),
    )
    .on_press(msg)
    .padding(Padding { top: 4.0, right: 18.0, bottom: 4.0, left: 0.0 })
    .width(Length::Fill)
    .style(move |_t, status| button::Style {
        background: None,
        text_color: if matches!(status, button::Status::Hovered) { s.ink } else { lbl_color },
        border: Border::default(),
        ..Default::default()
    })
    .into()
}

pub fn sidebar(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;

    #[cfg(target_os = "macos")]
    let masthead_padding = Padding { top: 40.0, right: 18.0, bottom: 14.0, left: 18.0 };
    #[cfg(not(target_os = "macos"))]
    let masthead_padding = Padding { top: 20.0, right: 18.0, bottom: 14.0, left: 18.0 };

    // ── 1. Masthead ──────────────────────────────────────────────────────────
    let masthead = container(
        column![
            row![
                text("Nin").size(27).font(SERIF_MEDIUM).color(s.ink),
                text("ox").size(27).font(SERIF_ITALIC).color(s.ink),
                text(" ⬡").size(20).color(s.ink),
            ]
            .align_y(Alignment::End),
            Space::new(0, 6),
            micro_label("Fleet Field Journal", s.ink_2).size(9.0),
        ],
    )
    .padding(masthead_padding)
    .width(Length::Fill);

    // ── 2. Table-of-contents nav ─────────────────────────────────────────────
    let on_fleet = matches!(app.view, View::FleetBoard { .. });
    let on_session = matches!(app.view, View::SessionDetail { .. });
    let on_prs = matches!(app.view, View::PrList);
    let on_brain = matches!(app.view, View::Brain);
    let toc = column![
        toc_item(app, "I.", "Fleet board", "1", Message::NavigateFleet { scope: None }, on_fleet),
        toc_item(app, "II.", "Session", "2", Message::NavigateLastSession, on_session),
        toc_item(app, "III.", "Pull requests", "3", Message::NavigatePrList, on_prs),
        toc_item(app, "IV.", "Brain", "4", Message::NavigateBrain, on_brain),
    ]
    .padding(Padding { top: 10.0, right: 0.0, bottom: 10.0, left: 0.0 });

    // ── 3. Action row: Alerts (badge) · + Spawn ─────────────────────────────
    let unread = app.notifications.len();
    let alerts_label: Element<Message> = if unread > 0 {
        row![
            micro_label("Alerts", s.ink_2).size(10.0),
            Space::new(6, 0),
            container(text(unread.min(99).to_string()).size(8).font(SANS_BOLD).color(s.card))
                .padding([1, 4])
                .style(move |_| container::Style {
                    background: Some(Background::Color(s.accent)),
                    border: Border { radius: 7.0.into(), ..Default::default() },
                    ..Default::default()
                }),
        ]
        .align_y(Alignment::Center)
        .into()
    } else {
        micro_label("Alerts", s.ink_2).size(10.0).into()
    };

    let action_btn_style = move |_t: &iced::Theme, status: button::Status| button::Style {
        background: matches!(status, button::Status::Hovered)
            .then_some(Background::Color(s.card)),
        text_color: s.ink_2,
        border: Border::default(),
        ..Default::default()
    };
    let actions = row![
        button(container(alerts_label).center_x(Length::Fill))
            .on_press(Message::ToggleNotifications)
            .style(action_btn_style)
            .padding([9, 4])
            .width(Length::Fill),
        container(Space::new(0, 0)).width(1).height(Length::Fixed(30.0)).style(
            move |_| container::Style {
                background: Some(Background::Color(s.rule_dark)),
                ..Default::default()
            }
        ),
        button(container(micro_label("+ Spawn", s.accent).size(10.0)).center_x(Length::Fill))
            .on_press(Message::SpawnSession)
            .style(action_btn_style)
            .padding([9, 4])
            .width(Length::Fill),
    ]
    .align_y(Alignment::Center);

    // ── 4. Session tree ──────────────────────────────────────────────────────
    let mut items: Vec<Element<Message>> = Vec::new();
    if !app.orchestrators.is_empty() {
        items.push(
            container(text("Orchestrators").size(13).font(SERIF_ITALIC).color(s.faint))
                .padding(Padding { top: 12.0, right: 18.0, bottom: 4.0, left: 18.0 })
                .into(),
        );
    }
    for orch in &app.orchestrators {
        let is_expanded = app.sidebar.selected_orchestrator.as_deref() == Some(orch.id.as_str());
        let worker_count = app
            .sessions
            .values()
            .filter(|w| w.orchestrator_id.as_deref() == Some(orch.id.as_str()))
            .count();
        items.push(tree_row(
            app,
            &orch.id,
            &orch.name,
            &format!("{worker_count} workers"),
            app.sessions.get(&orch.id).map(|se| &se.status),
            true,  // bold
            false, // not indented
            Some(if is_expanded { None } else { Some(orch.id.clone()) }), // chevron toggle target
            Some(Message::RemoveOrchestrator(orch.id.clone())),
        ));
        if is_expanded {
            let mut workers: Vec<_> = app
                .sessions
                .values()
                .filter(|w| w.orchestrator_id.as_deref() == Some(orch.id.as_str()))
                .collect();
            workers.sort_by(|a, b| a.name.cmp(&b.name));
            for w in workers {
                items.push(tree_row(
                    app, &w.id, &w.name, repo_short(&w.repo),
                    Some(&w.status), false, true, None,
                    Some(Message::RemoveSession(w.id.clone())),
                ));
            }
        }
    }
    let mut standalone: Vec<_> = app
        .sessions
        .values()
        .filter(|w| {
            w.orchestrator_id.is_none() && !app.orchestrators.iter().any(|o| o.id == w.id)
        })
        .collect();
    standalone.sort_by(|a, b| a.name.cmp(&b.name));
    if !standalone.is_empty() {
        items.push(
            container(text("Standalone").size(13).font(SERIF_ITALIC).color(s.faint))
                .padding(Padding { top: 12.0, right: 18.0, bottom: 4.0, left: 18.0 })
                .into(),
        );
    }
    for w in standalone {
        items.push(tree_row(
            app, &w.id, &w.name, repo_short(&w.repo),
            Some(&w.status), false, false, None,
            Some(Message::RemoveSession(w.id.clone())),
        ));
    }
    let list = scrollable(column(items).width(Length::Fill)).height(Length::Fill);

    // ── 5. Footer: theme dots ────────────────────────────────────────────────
    let footer = theme_dots_footer(app);

    let mut col_items: Vec<Element<Message>> = vec![
        masthead.into(),
        hline(s.rule_dark, 1.0),
        toc.into(),
        hline(s.rule_dark, 1.0),
        actions.into(),
        hline(s.rule_dark, 1.0),
    ];
    if app.sidebar.show_notifications {
        col_items.push(notification_panel(app));
    }
    col_items.push(list.into());
    col_items.push(hline(s.rule_dark, 1.0));
    col_items.push(footer);

    // Sidebar edge is a structural 2px ink border (right side only — iced borders
    // are uniform, so draw the edge as a separate vertical line).
    row![
        container(column(col_items))
            .width(Length::Fixed(app.sidebar_width - 2.0))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(s.paper_2)),
                ..Default::default()
            }),
        container(Space::new(0, 0)).width(2).height(Length::Fill).style(move |_| {
            container::Style { background: Some(Background::Color(s.ink)), ..Default::default() }
        }),
    ]
    .into()
}

/// One session-tree row: status dot + name + mono repo slug; active = card bg
/// + vermilion left bar; × remove button.
#[allow(clippy::too_many_arguments)]
fn tree_row<'a>(
    app: &'a App,
    id: &str,
    name: &'a str,
    right: &str,
    status: Option<&ninox_core::types::SessionStatus>,
    bold: bool,
    indented: bool,
    chevron_toggle: Option<Option<ninox_core::types::OrchestratorId>>,
    remove: Option<Message>,
) -> Element<'a, Message> {
    let s = &app.scheme;
    let is_active = matches!(&app.view, View::SessionDetail { session_id, .. } if session_id == id);
    let dot: Element<Message> = match status {
        Some(st) => status_dot(
            s.status_color(st),
            matches!(st, ninox_core::types::SessionStatus::Done
                        | ninox_core::types::SessionStatus::Terminated),
        ),
        None => Space::new(8, 0).into(),
    };
    let name_font = if bold { SANS_BOLD } else { crate::style::SANS };
    let left_pad = if indented { 38.0 } else { 18.0 };

    let mut content = row![
        container(Space::new(0, 0)).width(3).height(Length::Fixed(20.0)).style(move |_| {
            container::Style {
                background: Some(Background::Color(if is_active { s.accent } else { Color::TRANSPARENT })),
                ..Default::default()
            }
        }),
        Space::new(left_pad - 3.0, 0),
        dot,
        Space::new(9, 0),
        text(name.to_owned()).size(12.5).font(name_font).color(if is_active || bold { s.ink } else { s.ink_2 }),
        Space::new(Length::Fill, 0),
        text(right.to_owned()).size(10).font(MONO).color(s.faint),
    ]
    .align_y(Alignment::Center);

    if let Some(toggle_target) = chevron_toggle {
        content = content.push(Space::new(4, 0));
        content = content.push(
            button(text(if toggle_target.is_none() { "▾" } else { "▸" }).size(9).color(s.faint))
                .on_press(Message::SelectOrchestrator(toggle_target))
                .style(|_t, _st| button::Style { background: None, border: Border::default(), ..Default::default() })
                .padding([2, 4]),
        );
    }
    if let Some(remove_msg) = remove {
        content = content.push(
            button(text("×").size(12).color(s.faint))
                .on_press(remove_msg)
                .style(|_t, _st| button::Style { background: None, border: Border::default(), ..Default::default() })
                .padding([2, 6]),
        );
    }

    button(content)
        .on_press(Message::NavigateSession(id.to_owned()))
        .style(move |_t, status| button::Style {
            background: (is_active || matches!(status, button::Status::Hovered))
                .then_some(Background::Color(s.card)),
            text_color: s.ink_2,
            border: Border::default(),
            ..Default::default()
        })
        .padding(Padding { top: 3.0, right: 10.0, bottom: 3.0, left: 0.0 })
        .width(Length::Fill)
        .into()
}

/// Footer: "THEME" microlabel + one dot per variant; selected dot ringed in accent.
fn theme_dots_footer(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let mut dots = row![].spacing(6).align_y(Alignment::Center);
    for variant in [ThemeVariant::Light, ThemeVariant::Dark, ThemeVariant::Ninox] {
        let selected = app.active_variant == variant;
        let fill = match variant {
            ThemeVariant::Light => crate::theme::light().paper,
            ThemeVariant::Dark | ThemeVariant::Ninox => crate::theme::dark().paper,
        };
        dots = dots.push(
            button(
                container(Space::new(0, 0)).width(14).height(Length::Fixed(14.0)).style(
                    move |_| container::Style {
                        background: Some(Background::Color(fill)),
                        border: Border {
                            color: if selected { s.accent } else { s.ink },
                            width: if selected { 2.0 } else { 1.5 },
                            radius: 7.0.into(),
                        },
                        ..Default::default()
                    },
                ),
            )
            .on_press(Message::SwitchTheme(variant))
            .style(|_t, _st| button::Style { background: None, border: Border::default(), ..Default::default() })
            .padding(0),
        );
    }
    container(
        row![
            micro_label("Theme", s.ink_2).size(10.0),
            Space::new(Length::Fill, 0),
            dots,
        ]
        .align_y(Alignment::Center),
    )
    .padding([12, 18])
    .width(Length::Fill)
    .into()
}
```

Notes for the implementer:
- `theme_swatch`, `worker_row`, `standalone_row`, `theme_footer` from the old file are deleted (replaced by `tree_row` / `theme_dots_footer`). `SidebarState.show_theme_popout` and `Message::ToggleThemePopout` become unused by the sidebar — leave the message/state in place (Task 14 removes dead code) so this task stays minimal.
- `Message::NavigateBrain` and `View::Brain` ALREADY EXIST on main (brain browser PR #5) — the TOC row IV wires straight to them. Main's current sidebar also has a Brain nav affordance; it's replaced by the TOC row. Preserve any other behavior main's sidebar gained in PR #5 (read the file on main first — it differs from the version shown above, which came from an older branch).

- [ ] **Step 6: Build, run tests, look at it**

Run: `cargo build -p ninox && cargo test -p ninox` → PASS. `cargo run -p ninox` — sidebar should read as a journal TOC: masthead "Nin*ox* ⬡ / FLEET FIELD JOURNAL", roman-numeral nav, Alerts/+Spawn split row, tree with hollow done-dots, theme dots footer.

- [ ] **Step 7: Commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): field notes sidebar (masthead, TOC nav, tree, theme dots)"
```

---

### Task 5: Keyboard shortcuts — `1–4` views, `t` theme, `Esc` modal

**Files:**
- Modify: `crates/ninox-app/src/app.rs` (`Message::RawKey` arm of `apply`)
- Test: `app.rs` tests module

**Interfaces:**
- Consumes: `Message::NavigateLastSession` (Task 4).
- Produces: shortcut behavior; `4` initially falls through (Brain arrives in Task 11 — leave a marked match arm).

Key facts (verified): `global_event_handler` (app.rs:135) already emits `RawKey` ONLY for keyboard events with `Status::Ignored` — i.e. never while a `text_input` is focused. The existing `RawKey` arm (app.rs:673) forwards bytes to the terminal only when the view is `SessionDetail` with `Terminal`/`Split` panel. Shortcuts must not fire in that terminal-capturing state (typing `1` into a shell must stay a `1`).

- [ ] **Step 1: Write the failing tests**

```rust
fn press(app: &mut App, ch: &str) {
    app.update(Message::RawKey {
        key: iced::keyboard::Key::Character(ch.into()),
        modifiers: iced::keyboard::Modifiers::default(),
        text: Some(ch.to_string()),
    });
}

#[test]
fn number_keys_switch_views() {
    let mut app = base(engine());
    press(&mut app, "3");
    assert!(matches!(app.view, View::PrList));
    press(&mut app, "1");
    assert!(matches!(app.view, View::FleetBoard { .. }));
}

#[test]
fn t_toggles_light_dark() {
    let mut app = base(engine());
    let before = app.active_variant;
    press(&mut app, "t");
    assert_ne!(app.active_variant, before);
    press(&mut app, "t");
    assert_eq!(app.active_variant, before);
}

#[test]
fn esc_closes_spawn_modal() {
    let mut app = base(engine());
    app.update(Message::SpawnSession);
    assert!(app.spawn_modal.is_some());
    app.update(Message::RawKey {
        key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
        modifiers: iced::keyboard::Modifiers::default(),
        text: None,
    });
    assert!(app.spawn_modal.is_none());
}

#[test]
fn shortcuts_do_not_fire_in_terminal_view() {
    let mut app = base(engine());
    seed_session(&mut app, "sess-a", SessionStatus::Working);
    app.update(Message::NavigateSession("sess-a".into()));
    press(&mut app, "1"); // must go to the terminal, not switch views
    assert!(matches!(app.view, View::SessionDetail { .. }));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ninox shortcuts_ -- --nocapture; cargo test -p ninox number_keys` → FAIL.

- [ ] **Step 3: Restructure the `RawKey` arm**

At the TOP of the existing `Message::RawKey { key, modifiers, text }` arm, before the current terminal routing logic, insert:

```rust
// Esc closes the spawn modal from anywhere.
if state.spawn_modal.is_some() {
    if matches!(key, iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)) {
        state.spawn_modal = None;
    }
    return Task::none();
}

let terminal_capturing = matches!(
    &state.view,
    View::SessionDetail { panel, .. }
        if matches!(panel, DetailPanel::Terminal | DetailPanel::Split)
);
if !terminal_capturing && !modifiers.command() && !modifiers.control() && !modifiers.alt() {
    if let iced::keyboard::Key::Character(c) = &key {
        match c.as_str() {
            "1" => return App::apply(state, Message::NavigateFleet { scope: None }),
            "2" => return App::apply(state, Message::NavigateLastSession),
            "3" => return App::apply(state, Message::NavigatePrList),
            "4" => return App::apply(state, Message::NavigateBrain), // exists on main (PR #5)
            "t" => {
                let next = match state.active_variant {
                    ThemeVariant::Dark | ThemeVariant::Ninox => ThemeVariant::Light,
                    ThemeVariant::Light => ThemeVariant::Dark,
                };
                return App::apply(state, Message::SwitchTheme(next));
            }
            _ => {}
        }
    }
    return Task::none();
}
// … existing terminal byte-routing logic continues below, now guarded by
// `terminal_capturing` (keep its current body; it already checks the view).
```

- [ ] **Step 4: Run tests** — `cargo test -p ninox` → all PASS (including the four new ones).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/app.rs
git commit -m "feat(native-app): keyboard shortcuts for views, theme toggle, modal escape"
```

---

### Task 6: Fleet board — folio header, attention banner, ledger columns

**Files:**
- Rewrite: `crates/ninox-app/src/components/fleet_board.rs` (keep `board_sessions`, `attention_count`, `filtered_sessions` helpers verbatim — tests use them)
- Rewrite: `crates/ninox-app/src/components/filter_bar.rs`
- Modify: `crates/ninox-app/Cargo.toml` (add `chrono = "0.4"`)
- Test: `fleet_board.rs` tests

**Interfaces:**
- Consumes: `stamp`, `stamp_word`, `card_style`, `hard_shadow`, `dotted_rule`, `hline`, `micro_label`, fonts (Task 3).
- Produces: `pub fn folio_title(hour: u32) -> String` (used only here, but tested); `filter_bar(app) -> Element` keeps its signature (now an underlined `⌕` field, placed inside the folio row by fleet_board).

- [ ] **Step 1: Write the failing test** (append to a `#[cfg(test)] mod tests` in `fleet_board.rs`)

```rust
#[test]
fn folio_title_follows_time_of_day() {
    assert_eq!(folio_title(6),  "Morning observations");
    assert_eq!(folio_title(13), "Afternoon observations");
    assert_eq!(folio_title(19), "Evening observations");
    assert_eq!(folio_title(2),  "Night observations");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p ninox folio_title` → FAIL.

- [ ] **Step 3: Implement**

`Cargo.toml`: add `chrono = "0.4"` under `[dependencies]`.

`filter_bar.rs` — full replacement:

```rust
use iced::{
    widget::{column, container, row, text, text_input},
    Alignment, Background, Border, Element, Length, Space,
};

use crate::app::{App, Message};
use crate::style::hline;

/// Underlined "⌕ filter the fleet…" field for the folio row.
pub fn filter_bar(app: &App) -> Element<'_, Message> {
    let s = &app.scheme;
    let input = text_input("filter the fleet…", &app.fleet_filter.query)
        .on_input(Message::FleetFilterQuery)
        .size(12)
        .padding([4, 2])
        .style(move |_t, _st| text_input::Style {
            background: Background::Color(iced::Color::TRANSPARENT),
            border: Border::default(),
            icon: s.faint,
            placeholder: s.faint,
            value: s.ink,
            selection: iced::Color { a: 0.35, ..s.accent },
        });
    column![
        row![text("⌕").size(13).color(s.faint), Space::new(6, 0), input]
            .align_y(Alignment::Center),
        hline(s.ink, 1.5),
    ]
    .width(Length::Fixed(230.0))
    .into()
}
```

`fleet_board.rs` — keep imports + `repo_short` + `board_sessions` + `attention_count` + `filtered_sessions` + `COLUMNS` (but MERGE Done and Terminated into one column — spec §5 shows six ledger columns and grey covers done/terminated):

```rust
const COLUMNS: &[Column] = &[
    Column { label: "Working",   status: SessionStatus::Working },
    Column { label: "PR Open",   status: SessionStatus::PrOpen },
    Column { label: "CI Failed", status: SessionStatus::CiFailed },
    Column { label: "Review",    status: SessionStatus::ReviewPending },
    Column { label: "Mergeable", status: SessionStatus::Mergeable },
    Column { label: "Done",      status: SessionStatus::Done },
];
```

and in `fleet_board()` the Done column concatenates `board_sessions(app, &SessionStatus::Done, …)` + `board_sessions(app, &SessionStatus::Terminated, …)`.

New rendering code:

```rust
/// "Morning observations" / … by local hour (spec §5 folio header).
pub fn folio_title(hour: u32) -> String {
    let period = match hour {
        5..=11  => "Morning",
        12..=16 => "Afternoon",
        17..=21 => "Evening",
        _       => "Night",
    };
    format!("{period} observations")
}

fn folio<'a>(app: &'a App, scope: Option<&'a OrchestratorId>) -> Element<'a, Message> {
    use chrono::{Datelike, Local, Timelike};
    let s = &app.scheme;
    let now = Local::now();
    let title = folio_title(now.hour());
    let month = ["JANUARY","FEBRUARY","MARCH","APRIL","MAY","JUNE","JULY",
                 "AUGUST","SEPTEMBER","OCTOBER","NOVEMBER","DECEMBER"][now.month0() as usize];
    let date_label = format!("VOL. I — {} {} {}", now.day(), month, now.year());

    let orch_ids: std::collections::HashSet<&str> =
        app.orchestrators.iter().map(|o| o.id.as_str()).collect();
    let total = app.sessions.values().filter(|w| !orch_ids.contains(w.id.as_str())).count();
    let shown = COLUMNS.iter()
        .map(|c| board_sessions(app, &c.status, scope.map(|x| x.as_str())).len())
        .sum::<usize>()
        + board_sessions(app, &SessionStatus::Terminated, scope.map(|x| x.as_str())).len();

    // Split the title so the last word is italic ("Morning *observations*").
    let (head, tail) = title.rsplit_once(' ').unwrap_or(("", title.as_str()));
    row![
        text(format!("{head} ")).size(34).font(crate::style::SERIF).color(s.ink),
        text(tail.to_owned()).size(34).font(crate::style::SERIF_ITALIC).color(s.ink),
        Space::new(18, 0),
        text(date_label).size(10.5).font(crate::style::MONO).color(s.faint),
        Space::new(Length::Fill, 0),
        filter_bar(app),
        Space::new(18, 0),
        text(format!("{shown}/{total} sessions")).size(10.5).font(crate::style::MONO).color(s.ink_2),
    ]
    .align_y(Alignment::End)
    .padding(iced::Padding { top: 22.0, right: 28.0, bottom: 8.0, left: 28.0 })
    .into()
}
```

Attention banner (replaces the old one — 1.5px vermilion border, `⚑`, "See marked entries →" is decorative text for now):

```rust
fn attention_banner<'a>(app: &'a App) -> Option<Element<'a, Message>> {
    if attention_count(app) == 0 { return None; }
    let s = &app.scheme;
    let ci = app.sessions.values().filter(|w| matches!(w.status, SessionStatus::CiFailed)).count();
    let review = app.sessions.values().filter(|w| matches!(w.status, SessionStatus::ReviewPending)).count();
    let mut parts = Vec::new();
    if ci > 0 { parts.push(format!("{ci} CI failure{}", if ci == 1 { "" } else { "s" })); }
    if review > 0 { parts.push(format!("{review} review{}", if review == 1 { "" } else { "s" })); }
    Some(
        container(
            row![
                text("⚑").size(13).color(s.accent),
                Space::new(10, 0),
                text(format!("{} require attention.", parts.join(" and ")))
                    .size(12).font(crate::style::SANS_BOLD).color(s.accent),
            ]
            .align_y(Alignment::Center),
        )
        .padding([8, 14])
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(iced::Color { a: 0.06, ..s.accent })),
            border: Border { color: s.accent, width: 1.5, radius: 2.0.into() },
            ..Default::default()
        })
        .into(),
    )
}
```

Cards + columns (ledger: columns separated by vertical rules, NOT boxes):

```rust
fn session_card<'a>(app: &'a App, session: &'a Session) -> Element<'a, Message> {
    let s = &app.scheme;
    let st_color = s.status_color(&session.status);
    let word = crate::style::stamp_word(&session.status);
    let (card_a, _, _) = crate::style::shadow_alpha(s);
    let repo_line = if session.repo.is_empty() {
        session.id.clone()
    } else {
        session.repo.clone()
    };
    button(
        column![
            text(&session.name).size(16).font(crate::style::SERIF_MEDIUM).color(s.ink),
            Space::new(0, 2),
            text(repo_line).size(9.5).font(crate::style::MONO).color(s.faint),
            Space::new(0, 9),
            crate::style::dotted_rule(s.rule_dark),
            row![
                crate::style::stamp(word, st_color),
                Space::new(Length::Fill, 0),
                text(format!("${:.2}", session.cost_usd))
                    .size(11.5).font(crate::style::MONO_MEDIUM).color(s.ink),
            ]
            .align_y(Alignment::Center),
        ]
        .padding(iced::Padding { top: 12.0, right: 13.0, bottom: 10.0, left: 13.0 }),
    )
    .on_press(Message::NavigateSession(session.id.clone()))
    .width(Length::Fill)
    .style(move |_t, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(Background::Color(s.card)),
            text_color: s.ink,
            border: Border { color: s.rule_dark, width: 1.0, radius: 2.0.into() },
            shadow: crate::style::hard_shadow(
                s,
                if hovered { 4.0 } else { 2.0 },
                if hovered { 6.0 } else { 3.0 },
                card_a + if hovered { 0.02 } else { 0.0 },
            ),
        }
    })
    .into()
}

fn ledger_column<'a>(
    app: &'a App,
    label: &'static str,
    cards: Vec<Element<'a, Message>>,
    first: bool,
) -> Element<'a, Message> {
    let s = &app.scheme;
    let count = cards.len();
    let head = column![
        row![
            text(label).size(16.5).font(crate::style::SERIF_MEDIUM_ITALIC).color(s.ink),
            Space::new(Length::Fill, 0),
            text(format!("№ {count}")).size(10).font(crate::style::MONO).color(s.faint),
        ]
        .align_y(Alignment::End),
        Space::new(0, 8),
        crate::style::hline(s.ink, 2.0),
    ];
    let body = scrollable(column(cards).spacing(12).padding(iced::Padding {
        top: 12.0, right: 2.0, bottom: 4.0, left: 0.0,
    }))
    .height(Length::Fill);

    let inner = column![head, body].width(Length::Fixed(220.0));
    if first {
        container(inner).padding(iced::Padding { top: 0.0, right: 12.0, bottom: 0.0, left: 0.0 }).into()
    } else {
        row![
            crate::style::vline(s.rule, 1.0), // add `vline` helper to style.rs, mirror of hline with Length::Fill height
            container(inner).padding([0, 12]),
        ]
        .into()
    }
}
```

Also add to `style.rs` (used above):

```rust
/// Vertical rule of the given color/thickness.
pub fn vline<'a, M: 'a>(color: Color, width: f32) -> Element<'a, M> {
    container(Space::new(0, 0))
        .width(Length::Fixed(width))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            ..Default::default()
        })
        .into()
}
```

`fleet_board()` assembly: folio → attention banner (padded `[8, 28]` horizontally, only when Some) → orchestrator scope chips (keep the existing `tab_chip` mechanism but restyle: active = ink bg + card text, inactive = transparent + 1.5px ink border, radius 2, `micro_label` typography, matching the mockup's `.bm` mode buttons) → horizontal-scrollable `row` of `ledger_column`s padded `[16, 28]`.

- [ ] **Step 4: Run tests + look** — `cargo test -p ninox` → PASS. `cargo run -p ninox` with a few sessions: folio title, vermilion banner, ruled ledger columns with stamped cards.

- [ ] **Step 5: Commit**

```bash
git add -A crates/ninox-app
git commit -m "feat(native-app): field notes fleet board (folio, attention banner, ledger)"
```

---

### Task 7: Session detail — header, italic tabs, terminal chrome, panels

**Files:**
- Modify: `crates/ninox-app/src/components/session_detail.rs`, `info_panel.rs`, `inspector_panel.rs`

**Interfaces:**
- Consumes: style helpers; `DetailPanel` enum unchanged (`Terminal · Split · Info · Inspector`, Split default — matches spec exactly).
- Produces: no API changes. Terminal wiring (canvas, resize, Jump-to-latest overlay, drag handle) is preserved untouched.

- [ ] **Step 1: Read `session_detail.rs`, `info_panel.rs`, `inspector_panel.rs` in full** — identify the header row, `panel_btn`, terminal container, and info/inspector composition before editing.

- [ ] **Step 2: Restyle the header** (`sd-head`): back button (30×30, 1.5px ink border, radius 2, 2×2 shadow, `←`), 10px status dot, serif 28px `SERIF_MEDIUM` session name over a mono 10px `{repo} · {branch-ish} · worker of {orchestrator}` line (compose from `session.repo`, `session.orchestrator_id`; omit missing parts), PR stamp when `session.pr_number.is_some()` — `stamp(&format!("PR #{n}"), s.status_ci_failed_or_ci_color)` using the CI status color if known else `status_pr_open`, mono 13px cost, Kill button:

```rust
let kill = button(crate::style::micro_label("Kill", s.accent).size(10.0))
    .on_press(/* keep the existing kill/remove message used today */)
    .padding([6, 16])
    .style(move |_t, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: hovered.then_some(iced::Background::Color(s.accent)),
            text_color: if hovered { s.card } else { s.accent },
            border: iced::Border { color: s.accent, width: 1.5, radius: 2.0.into() },
            shadow: crate::style::hard_shadow(s, 2.0, 2.0, crate::style::shadow_alpha(s).0),
        }
    });
```

(If today's header has no kill button, wire it to `Message::RemoveSession(session_id)`.)

- [ ] **Step 3: Panel switcher as italic serif tabs** — replace `panel_btn`:

```rust
fn panel_btn<'a>(app: &'a App, label: &'a str, panel: DetailPanel, active: bool) -> Element<'a, Message> {
    let s = &app.scheme;
    button(
        column![
            text(label).size(15)
                .font(if active { crate::style::SERIF_MEDIUM_ITALIC } else { crate::style::SERIF_ITALIC })
                .color(if active { s.ink } else { s.faint }),
            Space::new(0, 4),
            crate::style::hline(if active { s.accent } else { iced::Color::TRANSPARENT }, 2.0),
        ],
    )
    .on_press(Message::SwitchDetailPanel(panel))
    .style(|_t, _st| button::Style { background: None, border: iced::Border::default(), ..Default::default() })
    .padding([2, 2])
    .into()
}
```

Tabs sit in a row spacing 22, with a full-width `hline(s.ink, 2.0)` immediately below (the active tab's accent line overlaps visually by sitting flush above it), horizontal margin 28.

- [ ] **Step 4: Terminal chrome** — wrap the existing terminal canvas in the "dark object" frame:

```rust
let term_frame = container(
    column![
        // title bar
        container(
            row![
                container(Space::new(0,0)).width(8).height(Length::Fixed(8.0)).style(move |_| container::Style {
                    background: Some(iced::Background::Color(s.status_color(&session.status))),
                    border: iced::Border { radius: 4.0.into(), ..Default::default() },
                    ..Default::default()
                }),
                Space::new(10, 0),
                text(format!("tmux · {} · {}×{}", session.id, app.terminal_cols, app.terminal_rows))
                    .size(9.5).font(crate::style::MONO).color(s.status_done),
                Space::new(Length::Fill, 0),
                text(crate::style::stamp_word(&session.status).to_lowercase())
                    .size(9.5).font(crate::style::MONO).color(s.status_done),
            ].align_y(Alignment::Center),
        )
        .padding([7, 12])
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(iced::Background::Color(s.term_bar)),
            border: iced::Border { color: s.term_bar_border, width: 0.0, radius: 0.0.into() },
            ..Default::default()
        }),
        crate::style::hline(s.term_bar_border, 1.0),
        existing_terminal_canvas_element, // ← whatever the file builds today, unchanged
    ],
)
.style(move |_| container::Style {
    background: Some(iced::Background::Color(s.term_bg)),
    border: iced::Border { color: s.ink, width: 2.0, radius: 3.0.into() },
    shadow: crate::style::hard_shadow(s, 4.0, 5.0, crate::style::shadow_alpha(s).1),
    ..Default::default()
});
```

Split ratio: terminal ~62% (`flex 1.6` in mockup) — keep the existing drag-resizable `info_width` mechanism; just set its default closer to 0.38 of the window if it isn't.

- [ ] **Step 5: Info panels** — in `info_panel.rs`: each panel = `container(...).style(move |_| crate::style::card_style(s))`, padding `[14, 16]`; heading = serif italic 17px + dotted rule below (`h3` treatment); CI rows: `✓`(status_working) / `✗`(status_ci_failed) / `◌`(status_review) glyph column 14px, 12px Archivo label, mono 9.5px right-aligned duration. Review-comments panel is retitled **"Marginalia"** with mono `{n} comments` sub; comment rows separated by `dotted_rule`, author in `s.accent` `SANS_BOLD`, `file:line` refs in `MONO` `s.faint`. Keep the clickable PR-URL `rich_text` behavior.

- [ ] **Step 6: Inspector** — `inspector_panel.rs`: card-styled sheet padding `[18, 22]`; each field row = `micro_label(key, s.faint)` at fixed 180px + value `text(...).size(11.5).font(MONO).color(s.ink_2)`. Keep the current field list.

- [ ] **Step 7: Build + tests + eyeball all four panel modes** — `cargo test -p ninox` PASS; run app, click through Terminal/Split/Info/Inspector.

- [ ] **Step 8: Commit**

```bash
git add crates/ninox-app/src/components
git commit -m "feat(native-app): field notes session detail (tabs, terminal chrome, marginalia)"
```

---

### Task 8: Terminal ANSI palette + selection color

**Files:**
- Modify: `crates/ninox-app/src/components/terminal.rs:160-177` (DEFAULT_PALETTE), `:485` (selection), `session_detail.rs:162-171` (pass-through — already uses `term_bg`/`term_fg` after Task 2)

**Interfaces:**
- Consumes: `term_*` tokens. Terminal logic untouched — only color constants change.

- [ ] **Step 1: Replace `DEFAULT_PALETTE`** with a warm ink-and-ember set tuned to the spec's ANSI-ish accents (ok `#8fd37f`, error `#f08a72`, agent `#f0c069`, dim `#7a7260`):

```rust
/// Field Notes terminal palette — warm paper-lamplight ANSI
/// (spec §1 "Terminal": same in both themes; it is "the dark object").
const DEFAULT_PALETTE: [IcedColor; 16] = [
    rgb(0x2c2822), // black  (term bar tone)
    rgb(0xf08a72), // red    (spec error)
    rgb(0x8fd37f), // green  (spec ok)
    rgb(0xf0c069), // yellow (spec agent-voice)
    rgb(0x7ea9d4), // blue
    rgb(0xc876b4), // magenta
    rgb(0x4ab0a4), // cyan
    rgb(0xece4d0), // white  (spec text)
    rgb(0x7a7260), // bright black (spec dim)
    rgb(0xf4a58f), // bright red
    rgb(0xa8e29a), // bright green
    rgb(0xf5d08a), // bright yellow
    rgb(0x9dc1e4), // bright blue
    rgb(0xd996c8), // bright magenta
    rgb(0x6fc4ba), // bright cyan
    rgb(0xf5efdd), // bright white
];
```

Match the existing palette's construction style — if it's built with a helper/macro other than a `rgb` fn, keep that mechanism and only change values (read terminal.rs:160-177 first; add a small `const fn rgb(hex: u32) -> IcedColor` only if none exists).

- [ ] **Step 2: Selection color** (terminal.rs:485): replace the hardcoded blue `IcedColor { r: 0.27, g: 0.52, b: 0.80, a: 0.5 }` with amber `IcedColor { r: 0.941, g: 0.753, b: 0.412, a: 0.35 }` (= `#f0c069` at 35%).

- [ ] **Step 3: Run the terminal test suite** — `cargo test -p ninox terminal::` → all PASS (they assert buffer contents, not colors).

- [ ] **Step 4: Run the app, open a session terminal** — dark inset object with warm text; `ls`/vim colors look coherent in both themes (`t` to toggle).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/terminal.rs
git commit -m "feat(native-app): warm field-notes terminal palette and selection"
```

---

### Task 9: Pull-request ledger

**Files:**
- Rewrite rendering in: `crates/ninox-app/src/components/pr_list.rs` (keep data assembly + `ci_badge` logic source, restyle output)

**Interfaces:**
- Consumes: style helpers. `pr_list(app) -> Element` signature unchanged. Row click keeps navigating to the session.

- [ ] **Step 1: Restyle.** Structure per spec §5-III and mockup `.ledger`/`.prt-row`:

- Folio header: `Pull requests` with "requests" italic (reuse the folio pattern from Task 6 with title split), date label `LEDGER — {D MONTH YYYY}`, count `{n} open` in mono.
- Table: one `container` margin `[14, 28]` with `heavy_frame(s)` style.
- Header row: bg `paper_2`, `micro_label` columns, 2px ink `hline` below.
- Data rows: fixed grid via `row![]` widths — № 70px (`MONO_MEDIUM`, `s.accent`, `#214`), Title `Fill` (serif 15px ink, single line), Session 150px (status dot + 11.5px name), Repo 120px (mono 10px faint), CI 130px (a `stamp` with CI wording: running → `Running x/y` in `status_review` color… map from existing `ci_badge` logic: pass→`Passed x/y` `status_working`, fail→`Failed` `status_ci_failed`, running→`Running x/y` `status_review`, none→`—` plain faint text), Cost 70px right-aligned mono.
- Row separator `hline(s.rule, 1.0)`; row hover bg `s.paper` (via button style `Status::Hovered`).

- [ ] **Step 2: Build + tests + eyeball** — `cargo test -p ninox` PASS; view with PRs present.

- [ ] **Step 3: Commit**

```bash
git add crates/ninox-app/src/components/pr_list.rs
git commit -m "feat(native-app): field notes PR ledger table"
```

---

### Task 10: Enriched spawn modal

**Files:**
- Rewrite: `crates/ninox-app/src/components/spawn_modal.rs`
- Modify: `crates/ninox-app/src/app.rs` (SpawnForm messages + confirm handler)
- Test: `app.rs` tests

**Interfaces:**
- Consumes: `tmux::send_keys` (exists: `ninox_core::tmux::send_keys(&session_id, &message)` — see main.rs `Command::Send`), worker spawning via self-exec of `ninox spawn` (main.rs:104 `Command::Spawn { prompt, workspace, name, orchestrator_id }`).
- Produces:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AttachChoice { Standalone, Orchestrator { id: String, name: String } }
impl std::fmt::Display for AttachChoice { /* "Standalone (new orchestrator)" | "{name} (orchestrator)" */ }

#[derive(Debug, Clone)]
pub struct AgentPreset { pub label: &'static str, pub harness: &'static str, pub model: Option<&'static str> }
pub const AGENT_PRESETS: &[AgentPreset] = &[
    AgentPreset { label: "claude · fable-5",   harness: "claude-code", model: Some("claude-fable-5") },
    AgentPreset { label: "claude · opus-4.8",  harness: "claude-code", model: Some("claude-opus-4-8") },
    AgentPreset { label: "claude · haiku-4.5", harness: "claude-code", model: Some("claude-haiku-4-5") },
];

#[derive(Debug, Clone, Default)]
pub struct SpawnForm {
    pub name: String,
    pub repo: Option<String>,       // display metadata; stored on the session
    pub attach: Option<AttachChoice>, // None == Standalone
    pub agent_idx: usize,           // index into AGENT_PRESETS
    pub task: String,
}
```

New messages: `SpawnFormRepo(String)`, `SpawnFormAttach(AttachChoice)`, `SpawnFormAgent(usize)`, `SpawnFormTask(String)` (name/confirm/cancel already exist).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn spawn_form_field_messages_update_state() {
    let mut app = base(engine());
    app.update(Message::SpawnSession);
    app.update(Message::SpawnFormName("theme-tokens".into()));
    app.update(Message::SpawnFormTask("extract palettes".into()));
    app.update(Message::SpawnFormAgent(1));
    let f = app.spawn_modal.as_ref().unwrap();
    assert_eq!(f.name, "theme-tokens");
    assert_eq!(f.task, "extract palettes");
    assert_eq!(f.agent_idx, 1);
}

#[test]
fn spawn_standalone_uses_selected_agent_and_repo() {
    let mut app = base(engine());
    app.update(Message::SpawnSession);
    app.update(Message::SpawnFormName("theme-tokens".into()));
    app.update(Message::SpawnFormRepo("slievr/ninox".into()));
    app.update(Message::SpawnFormConfirm);
    let sess = app.sessions.get("theme-tokens").expect("session created");
    assert_eq!(sess.repo, "slievr/ninox");
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p ninox spawn_form_field` → FAIL.

- [ ] **Step 3: Implement state + handlers in `app.rs`**

- Add the four message variants; each mutates the corresponding `SpawnForm` field (mirror the existing `SpawnFormName` arm).
- `SpawnFormConfirm` — restructure:
  - **Standalone path** (attach None/Standalone): existing orchestrator-spawn flow, with three changes: (1) the session's `repo` field is set from `form.repo.unwrap_or_default()`; (2) the agent is `AGENT_PRESETS[form.agent_idx]` converted to `AgentConfig { harness: preset.harness.into(), model: preset.model.map(Into::into) }` instead of `state.orchestrator_agent`; (3) if `form.task` is non-empty, in the async block after the existing 300ms sleep + before `start_streaming`, send the task to the new tmux session:

```rust
if !task_text.is_empty() {
    if let Err(e) = ninox_core::tmux::send_keys(&tmux_id, &task_text).await {
        tracing::warn!("send task to {tmux_id}: {e}");
    }
}
```

  - **Attached path** (`AttachChoice::Orchestrator { id, .. }`): spawn a worker by self-exec so the whole `run_spawn` path (worktree, repo detection, orchestrator context prompt) is reused:

```rust
let workspace = state.sessions.get(&id)
    .and_then(|o| o.workspace_path.clone())
    .unwrap_or_else(|| state.orchestrator_root.join(&id).to_string_lossy().to_string());
let prompt = if form.task.trim().is_empty() { format!("Work on: {name}") } else { form.task.clone() };
let worker_name = name.clone();
return Task::future(async move {
    let exe = std::env::current_exe().unwrap_or_else(|_| "ninox".into());
    match tokio::process::Command::new(exe)
        .arg("spawn").arg(&prompt)
        .arg("--workspace").arg(&workspace)
        .arg("--name").arg(&worker_name)
        .arg("--orchestrator-id").arg(&id)
        .output().await
    {
        Ok(out) if out.status.success() => Message::PollSessions, // db-poll picks the row up; force one now
        Ok(out) => { tracing::error!("worker spawn failed: {}", String::from_utf8_lossy(&out.stderr)); Message::Noop }
        Err(e) => { tracing::error!("worker spawn exec: {e}"); Message::Noop }
    }
});
```

(Check `Command::Spawn`'s actual clap arg names in main.rs:34-40 and match them exactly — `prompt` may be positional.)

- [ ] **Step 4: Rewrite `spawn_modal.rs` rendering** — journal entry over a dimmed backdrop (no blur — iced has no backdrop-filter):

- Backdrop: `rgba(33,29,22,0.45)` light / `rgba(0,0,0,0.55)` dark → `Color { a: 0.45, ..s.shadow }`.
- Modal: width 470, `heavy_frame`-like but shadow `hard_shadow(s, 8.0, 10.0, modal_alpha)`.
- Header strip: bg `paper_2`, 2px ink bottom rule, `Spawn a` serif 23px + `session` serif-italic, right `ENTRY № {n}` mono 10px faint where `n = app.sessions.len() + 1`.
- Fields: label = `micro_label(…, s.ink_2)`; underlined inputs = the Task-6 pattern (`text_input` transparent + `hline(s.rule_dark, 1.5)` under it), input text serif 16px (`.font(SERIF)` on the text_input).
- Repository + Attach side-by-side (`row` of two equal columns, spacing 20): `pick_list` for repo over `Vec<String>` of distinct non-empty `session.repo` values (sorted, deduped; hide the field when empty), `pick_list` for attach over `vec![AttachChoice::Standalone] + orchestrators`.
- Agent · Model chips: `row` of buttons from `AGENT_PRESETS`; selected = ink bg, card text, `SANS_BOLD`; unselected = 1.5px `rule_dark` border, radius 14 (the ONE pill exception), `s.ink_2`. On press → `SpawnFormAgent(i)`.
- Task: `text_input` multiline is not in iced 0.13 — use `iced::widget::text_editor` if already feasible, otherwise a single-line `text_input` with placeholder "What should this session do?" (note the compromise in a comment).
- Footer: mono 10px `est. $2–4 / session`, ghost Cancel (1.5px rule_dark border → ink on hover), primary `SPAWN ⬡` (accent bg, card text, `hard_shadow(s,3,3,…)`, grows to 4×4 on hover).

- [ ] **Step 5: Run tests + manual spawn** — `cargo test -p ninox` PASS; spawn a standalone session with a task via the UI and confirm the task text arrives in the terminal; if an orchestrator exists, attach a worker and see it appear in the tree within ~3s.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app/src
git commit -m "feat(native-app): enriched spawn modal (repo, attach, agent, task)"
```

---

### Task 11: Brain view scaffold — mode toggle, folio, wikilink helpers

**IMPORTANT — main already has a brain browser (PR #5).** `View::Brain`, `components/brain_panel.rs` (list + `detail_pane`), `BrainViewState { entries, loaded, selected, filter }`, messages `NavigateBrain`/`BrainSelectEntry(String)`/`BrainFilterQuery(String)`/`BrainReindex`, and `App.brain: Arc<BrainIndex>` all exist. This task EXTENDS them — do not create parallel state or duplicate messages. Read `brain_panel.rs` and the brain arms in `app.rs` before writing anything.

**Files:**
- Modify: `crates/ninox-app/src/components/brain_panel.rs` (add helpers + folio/mode-toggle scaffold)
- Modify: `app.rs` (extend `BrainViewState`, add 3 messages)
- Modify: `crates/ninox-app/Cargo.toml` — iced features add `"markdown"`

**Interfaces:**
- Consumes: existing `BrainViewState` + messages listed above; `BrainEntry { id, entry_type, name, tags, repos, updated, body }`.
- Produces:

```rust
// app.rs — extend, don't replace
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrainMode { #[default] Pinboard, Catalogue }
pub struct BrainViewState {
    // existing fields stay: entries, loaded, selected, filter
    pub mode: BrainMode,
    pub open_drawers: std::collections::HashSet<String>,
    pub markdown: Vec<iced::widget::markdown::Item>, // parsed body of `selected`
}
// new messages (BrainSelectEntry/BrainFilterQuery/BrainReindex already exist)
BrainSetMode(BrainMode), BrainToggleDrawer(String),
BrainLinkClicked(iced::widget::markdown::Url),
```

- `BrainSelectEntry(id)` handler grows: also parse markdown (`preprocess_wikilinks` → `markdown::parse`), insert the entry's `entry_type` into `open_drawers`, and set `mode = Catalogue`.
- `brain_panel.rs` pure helpers (tested): `extract_wikilinks`, `preprocess_wikilinks`, `backlinks_for`, `resolve_link`, `category_color`, `categories`.

- [ ] **Step 1: Write the failing tests** (in `brain_panel.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, ty: &str, name: &str, body: &str) -> ninox_core::brain::BrainEntry {
        ninox_core::brain::BrainEntry {
            id: id.into(), entry_type: ty.into(), name: name.into(),
            tags: vec![], repos: vec![], updated: None, body: body.into(),
        }
    }

    #[test]
    fn extracts_wikilinks() {
        assert_eq!(
            extract_wikilinks("passes [[frame-alignment]] before [[errors/scrollback-dup]] runs"),
            vec!["frame-alignment".to_string(), "errors/scrollback-dup".to_string()]
        );
        assert!(extract_wikilinks("no links [here] or [[unclosed").is_empty());
    }

    #[test]
    fn preprocesses_wikilinks_to_brain_urls() {
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment]]."),
            "see [frame-alignment](ninox-brain:frame-alignment)."
        );
    }

    #[test]
    fn backlinks_match_by_name_or_id_stem() {
        let a = entry("symbols/scrollback-buffer.md", "symbols", "ScrollbackBuffer", "…");
        let b = entry("concepts/frame-alignment.md", "concepts", "frame-alignment",
                      "owned by [[ScrollbackBuffer]]");
        let c = entry("errors/scrollback-dup.md", "errors", "scrollback-dup", "unrelated");
        let all = vec![a.clone(), b.clone(), c];
        let backs = backlinks_for(&all, &a);
        assert_eq!(backs.len(), 1);
        assert_eq!(backs[0].id, b.id);
    }

    #[test]
    fn categories_are_counted_and_ordered_by_taxonomy() {
        let all = vec![
            entry("symbols/a.md", "symbols", "a", ""),
            entry("symbols/b.md", "symbols", "b", ""),
            entry("errors/x.md", "errors", "x", ""),
        ];
        let cats = categories(&all);
        assert_eq!(cats, vec![("symbols".to_string(), 2), ("errors".to_string(), 1)]);
    }
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p ninox brain_panel::` → FAIL.

- [ ] **Step 3: Implement the helpers** (top of `brain_panel.rs`)

```rust
use ninox_core::brain::BrainEntry;
use crate::theme::ColorScheme;

/// Order per spec §1 "brain category colors".
pub const TAXONOMY: &[&str] = &[
    "repos", "symbols", "concepts", "patterns",
    "decisions", "architecture", "relationships", "errors",
];

pub fn category_color(s: &ColorScheme, ty: &str) -> iced::Color {
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

/// `[[target]]` occurrences, in order.
pub fn extract_wikilinks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let target = after[..end].trim();
                if !target.is_empty() { out.push(target.to_string()); }
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    out
}

/// `[[x]]` → `[x](ninox-brain:x)` so the markdown widget renders clickable links.
pub fn preprocess_wikilinks(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let target = after[..end].trim();
                out.push_str(&format!("[{target}](ninox-brain:{target})"));
                rest = &after[end + 2..];
            }
            None => { out.push_str("[["); rest = after; }
        }
    }
    out.push_str(rest);
    out
}

/// Does `link` refer to `entry`? Match on name, full id, or id stem.
fn link_matches(link: &str, entry: &BrainEntry) -> bool {
    let stem = entry.id.rsplit('/').next().unwrap_or(&entry.id).trim_end_matches(".md");
    let link_stem = link.rsplit('/').next().unwrap_or(link);
    link == entry.name || link == entry.id || link_stem == stem || link_stem == entry.name
}

/// Resolve a clicked wikilink to an entry id.
pub fn resolve_link<'a>(entries: &'a [BrainEntry], link: &str) -> Option<&'a BrainEntry> {
    entries.iter().find(|e| link_matches(link, e))
}

/// Entries whose bodies wikilink to `target` ("Cited by").
pub fn backlinks_for<'a>(entries: &'a [BrainEntry], target: &BrainEntry) -> Vec<&'a BrainEntry> {
    entries.iter()
        .filter(|e| e.id != target.id)
        .filter(|e| extract_wikilinks(&e.body).iter().any(|l| link_matches(l, target)))
        .collect()
}

/// (category, count), taxonomy order first, then alphabetic for unknown types.
pub fn categories(entries: &[BrainEntry]) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for e in entries { *counts.entry(e.entry_type.as_str()).or_default() += 1; }
    let mut out: Vec<(String, usize)> = Vec::new();
    for t in TAXONOMY {
        if let Some(n) = counts.remove(t) { out.push(((*t).to_string(), n)); }
    }
    let mut rest: Vec<_> = counts.into_iter().collect();
    rest.sort_by(|a, b| a.0.cmp(b.0));
    out.extend(rest.into_iter().map(|(t, n)| (t.to_string(), n)));
    out
}
```

- [ ] **Step 4: Run the helper tests** — `cargo test -p ninox brain_panel::` → PASS.

- [ ] **Step 5: Wire the scaffold onto the existing brain view**

- `Cargo.toml`: iced features → `["tokio", "canvas", "advanced", "wgpu", "markdown"]`.
- `app.rs`: add the three new fields to `BrainViewState` (with `Default`), the three new messages. New/changed handlers:

```rust
Message::BrainSetMode(m) => { state.brain_view.mode = m; Task::none() }
Message::BrainToggleDrawer(cat) => {
    if !state.brain_view.open_drawers.remove(&cat) { state.brain_view.open_drawers.insert(cat); }
    Task::none()
}
// EXTEND the existing BrainSelectEntry arm:
Message::BrainSelectEntry(id) => {
    if let Some(e) = state.brain_view.entries.iter().find(|e| e.id == id) {
        state.brain_view.markdown = iced::widget::markdown::parse(
            &crate::components::brain_panel::preprocess_wikilinks(&e.body),
        ).collect();
        state.brain_view.open_drawers.insert(e.entry_type.clone());
        state.brain_view.mode = BrainMode::Catalogue;
    }
    state.brain_view.selected = Some(id);
    Task::none()
}
Message::BrainLinkClicked(url) => {
    if url.scheme() == "ninox-brain" {
        let link = url.path().to_string();
        if let Some(e) = crate::components::brain_panel::resolve_link(&state.brain_view.entries, &link) {
            let id = e.id.clone();
            return App::apply(state, Message::BrainSelectEntry(id));
        }
    } else {
        return App::apply(state, Message::OpenUrl(url.to_string()));
    }
    Task::none()
}
```

(`NavigateBrain`, `BrainFilterQuery`, `BrainReindex` handlers stay as they are.)

- `brain_panel(app)` for THIS task: replace the current header with the Field Notes folio (title `The brain` with "brain" italic, `SPECIMENS — {n}` in the date slot, the ✦/☰ mode toggle, search field reusing the Task-6 underline pattern wired to the EXISTING `Message::BrainFilterQuery`, plus keep a restyled Reindex affordance → `micro_label` button wired to existing `Message::BrainReindex`). Body: when `mode == Catalogue` keep rendering the existing list/detail for now (restyled in Task 12); when `Pinboard`, an empty `heavy_frame` placeholder (Task 13 fills it). Mode toggle (mockup `.brain-mode`): joined buttons in a 1.5px-ink-border container radius 2 with 2×2 shadow; active segment ink bg/card text; labels `✦ PINBOARD` / `☰ CATALOGUE` in `micro_label` typography, press → `BrainSetMode`.

- [ ] **Step 6: Add scaffold test** (main already tests NavigateBrain — don't duplicate; use its `base_with_brain` fixture)

```rust
#[test]
fn selecting_entry_opens_catalogue_and_drawer() {
    let (mut app, _brain) = /* build via the existing base_with_brain fixture with one seeded
                               entry id "symbols/x.md", entry_type "symbols" */;
    app.update(Message::BrainSetMode(BrainMode::Pinboard));
    app.update(Message::BrainSelectEntry("symbols/x.md".into()));
    assert_eq!(app.brain_view.mode, BrainMode::Catalogue);
    assert!(app.brain_view.open_drawers.contains("symbols"));
    assert_eq!(app.brain_view.selected.as_deref(), Some("symbols/x.md"));
}
```

Run: `cargo test -p ninox` → PASS. Run the app, press `4` — folio + toggle render over the existing list.

- [ ] **Step 7: Commit**

```bash
git add -A crates/ninox-app
git commit -m "feat(native-app): brain view field-notes scaffold and wikilink helpers"
```

---

### Task 12: Brain catalogue — drawers + reading pane

**Files:**
- Modify: `crates/ninox-app/src/components/brain_panel.rs` (replaces the PR-#5 list/`detail_pane` rendering; keep `matches_filter` if still useful)

**Interfaces:**
- Consumes: extended `BrainViewState` (Task 11), `markdown` widget, style helpers.
- Produces: `catalogue_body(app) -> Element` used inside `brain_panel` when `mode == Catalogue`.

- [ ] **Step 1: Implement the drawers rail** (left, 272px, `heavy_frame` style, scrollable):

Per category from `categories(&filtered_entries)` (filter entries by `app.brain_view.filter` matching name/id/tags case-insensitively when non-empty): a drawer header row — `▸`/`▾` caret 9px faint, 9px filled circle in `category_color`, serif 15px category name, mono 9.5px count, and a 22×7px "drawer pull" (container with 1.5px `rule_dark` border, radius 4) — press → `BrainToggleDrawer(cat)`. Open drawers (`app.brain_view.open_drawers.contains(cat)`) list entries of that type sorted by name: mono 10.5px name + right-aligned mono 8.5px updated date (raw `updated` string, take first 10 chars), left padding 47, selected = accent 3px left bar + `card` bg + `MONO_MEDIUM`; press → `BrainSelectEntry(id)`. Drawer separators `hline(s.rule, 1.0)`.

- [ ] **Step 2: Implement the reading pane** (right, Fill, `heavy_frame` style, scrollable, padding `[28, 36]`):

When `app.brain_view.selected` resolves to an entry:

```rust
let crumb = text(format!("brain / {} / {}", e.entry_type,
        e.id.rsplit('/').next().unwrap_or(&e.id)))
    .size(9.5).font(MONO).color(s.faint);
let title = row![
    text(&e.name).size(32).font(SERIF_MEDIUM).color(s.ink),
    Space::new(14, 0),
    stamp(&e.entry_type, category_color(s, &e.entry_type)),
].align_y(Alignment::Center);
// frontmatter dl: 2px ink rule above, 1px rule_dark below, rows of
// micro_label(key, s.faint) at 88px + mono 11px ink_2 value:
//   TYPE {entry_type} · TAGS {tags.join(", ")} · REPOS {repos.join(", ")} · UPDATED {updated}
// (omit empty rows)
let body = iced::widget::markdown::view(
        &app.brain_view.markdown,
        iced::widget::markdown::Settings::default(),
        iced::widget::markdown::Style::from_palette(app.scheme.iced_theme().palette()),
    )
    .map(Message::BrainLinkClicked);
// backlinks: dotted_rule, micro_label "CITED BY — {n} SPECIMENS", then chips:
// mono 10.5px entry ids in 1px rule_dark bordered buttons (radius 2, padding [3,9]),
// hover border s.ink; press → BrainSelectEntry(id). Hide the section when empty.
```

Empty state (nothing selected): centered serif-italic 15px `s.faint` line: `Nothing pinned tonight.` (spec §7 empty-state language). Body max width: wrap the markdown in `container(...).max_width(640)`.

- [ ] **Step 3: Manual test with a seeded brain** — create `~/.ninox/brain/symbols/scrollback-buffer.md` (or wherever `main.rs` points the brain path — check `run_brain`/`BrainIndex::open` call) with frontmatter + `[[wikilinks]]`, run `cargo run -p ninox -- brain index`, then the app → press `4` → `☰ Catalogue`: drawers show categories, clicking an entry renders markdown, wikilinks navigate, backlinks appear.

- [ ] **Step 4: `cargo test -p ninox`** → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/brain_panel.rs
git commit -m "feat(native-app): brain catalogue drawers and markdown reading pane"
```

---

### Task 13: Brain pinboard — canvas specimen board

**Files:**
- Create: `crates/ninox-app/src/components/brain_pinboard.rs`
- Modify: `brain_panel.rs` (mount it as the Pinboard body), `components/mod.rs`

**Interfaces:**
- Consumes: `BrainViewState.entries`, `BrainViewState.filter`, `category_color`, `extract_wikilinks`, `resolve_link`.
- Produces: `pub fn pinboard<'a>(app: &'a App) -> Element<'a, Message>` — left rail (215px card: serif-italic `brain/ — taxonomy` header + category rows with colored dot and mono count) + the canvas in a `heavy_frame` container; node click → `Message::BrainSelectEntry` (which switches to Catalogue — acceptable v1 of "click-to-focus"); a preview slip for the hovered/selected node is deferred (spec §5-IV marks scrubber/cluster as planned).

- [ ] **Step 1: Implement the canvas program**

```rust
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

use crate::app::{App, Message};
use crate::theme::ColorScheme;
use super::brain_panel::{category_color, extract_wikilinks, resolve_link};

struct Node { x: f32, y: f32, r: f32, color: Color, hit: bool, id: String }

/// Deterministic hash-based layout: stable across frames without storing
/// positions (Date/rand-free; same entry set → same board).
fn hash01(s: &str, salt: u64) -> f32 {
    let mut h: u64 = 1469598103934665603 ^ salt;
    for b in s.bytes() { h ^= b as u64; h = h.wrapping_mul(1099511628211); }
    ((h >> 11) as f64 / (1u64 << 53) as f64) as f32
}

pub struct Pinboard<'a> { pub app: &'a App }

impl<'a> Pinboard<'a> {
    fn nodes(&self, bounds: Rectangle) -> Vec<Node> {
        let s = &self.app.scheme;
        let q = self.app.brain_view.filter.to_lowercase();
        self.app.brain_view.entries.iter().map(|e| {
            let links = extract_wikilinks(&e.body).len() as f32;
            Node {
                x: bounds.width  * (0.05 + 0.90 * hash01(&e.id, 7)),
                y: bounds.height * (0.06 + 0.88 * hash01(&e.id, 13)),
                r: 3.0 + (links * 1.2).min(6.0),
                color: category_color(s, &e.entry_type),
                hit: !q.is_empty()
                    && (e.name.to_lowercase().contains(&q) || e.id.to_lowercase().contains(&q)),
                id: e.id.clone(),
            }
        }).collect()
    }
}

impl<'a> canvas::Program<Message> for Pinboard<'a> {
    type State = ();

    fn draw(&self, _st: &(), renderer: &Renderer, _t: &Theme,
            bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<Geometry> {
        let s = &self.app.scheme;
        let mut frame = Frame::new(renderer, bounds.size());
        let nodes = self.nodes(Rectangle { x: 0.0, y: 0.0, ..bounds });

        // dashed wikilink threads
        let ink_edge = Color { a: 0.18, ..s.ink };
        let by_id: std::collections::HashMap<&str, usize> =
            nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        for e in &self.app.brain_view.entries {
            let Some(&a) = by_id.get(e.id.as_str()) else { continue };
            for link in extract_wikilinks(&e.body) {
                let Some(target) = resolve_link(&self.app.brain_view.entries, &link) else { continue };
                let Some(&b) = by_id.get(target.id.as_str()) else { continue };
                let lit = nodes[a].hit || nodes[b].hit;
                frame.stroke(
                    &Path::line(Point::new(nodes[a].x, nodes[a].y),
                                Point::new(nodes[b].x, nodes[b].y)),
                    Stroke {
                        style: canvas::Style::Solid(if lit { Color { a: 0.55, ..s.accent } } else { ink_edge }),
                        width: 1.0,
                        line_dash: canvas::LineDash { segments: &[3.0, 3.0], offset: 0 },
                        ..Stroke::default()
                    },
                );
            }
        }
        // ink-outlined specimen dots
        for n in &nodes {
            let dot = Path::circle(Point::new(n.x, n.y), n.r);
            frame.fill(&dot, n.color);
            frame.stroke(&dot, Stroke::default()
                .with_color(Color { a: 0.75, ..s.ink }).with_width(1.2));
            if n.hit {
                frame.stroke(&Path::circle(Point::new(n.x, n.y), n.r + 4.0),
                             Stroke::default().with_color(s.accent).with_width(1.2));
            }
        }
        vec![frame.into_geometry()]
    }

    fn update(&self, _st: &mut (), event: canvas::Event, bounds: Rectangle,
              cursor: mouse::Cursor) -> (canvas::event::Status, Option<Message>) {
        if let canvas::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) = event {
            if let Some(pos) = cursor.position_in(bounds) {
                let nodes = self.nodes(Rectangle { x: 0.0, y: 0.0, ..bounds });
                if let Some(n) = nodes.iter().min_by(|a, b| {
                    let da = (a.x - pos.x).hypot(a.y - pos.y);
                    let db = (b.x - pos.x).hypot(b.y - pos.y);
                    da.partial_cmp(&db).unwrap()
                }) {
                    if (n.x - pos.x).hypot(n.y - pos.y) < n.r + 6.0 {
                        return (canvas::event::Status::Captured,
                                Some(Message::BrainSelectEntry(n.id.clone())));
                    }
                }
            }
        }
        (canvas::event::Status::Ignored, None)
    }
}

pub fn pinboard_canvas(app: &App) -> Element<'_, Message> {
    Canvas::new(Pinboard { app })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
```

(Adapt `canvas::Event`/`LineDash`/`Style` paths to the exact iced 0.13 API — check with `cargo doc` or the iced source if the names differ slightly; the structure stands.)

- [ ] **Step 2: Mount in `brain_panel.rs`** — Pinboard body = `row![rail, heavy_frame container(pinboard_canvas(app))]` spacing 16 padding `[16, 28]`. Rail: 215px `card_style` container, serif-italic 14px faint header `brain/ — taxonomy`, then per `categories(&entries)` a row (colored 9px dot, 12px `s.ink_2` name, right mono 9.5px faint count).

- [ ] **Step 3: Manual test** — seeded brain from Task 12: press `4` → dots + dashed threads render; searching rings matches in vermilion; clicking a dot opens it in Catalogue mode.

- [ ] **Step 4: `cargo test -p ninox && cargo build -p ninox`** → PASS.

- [ ] **Step 5: Commit**

```bash
git add -A crates/ninox-app/src
git commit -m "feat(native-app): brain pinboard specimen-board canvas"
```

---

### Task 14: Polish, dead code, QA sweep

**Files:**
- Modify: `notification_panel.rs`, various

- [ ] **Step 1: Notification panel** — restyle as journal-margin slips (spec §7): each notification = small `card_style` slip with a kind `stamp` (map `kind_color` to the status tokens), 12px Archivo message, mono timestamp; "Dismiss all" as a `micro_label` button.

- [ ] **Step 2: Empty states** — one italic-serif 15px `s.faint` line each: fleet board with zero sessions → `No sessions in the field.`; empty ledger column → nothing (spec: collapsed/empty columns not designed — leave the header only); PR ledger empty → `No pull requests on file.`; brain with no entries → `The specimen drawers are empty — run ninox brain index.`

- [ ] **Step 3: Remove dead code** — `SidebarState.show_theme_popout` + `Message::ToggleThemePopout` (unused since Task 4) and any orphaned helpers; run `cargo clippy -p ninox -- -D warnings` and fix everything it flags in ninox-app.

- [ ] **Step 4: Full QA sweep**

```bash
cargo fmt --all
cargo clippy -p ninox -- -D warnings
cargo test --workspace
cargo run -p ninox
```

Manual checklist, BOTH themes (`t` to toggle): sidebar TOC + tree + theme dots · fleet folio/banner/ledger/cards · card hover shadow grows · session tabs ×4 + terminal chrome + Marginalia + inspector · PR ledger rows navigate · spawn modal all fields + Esc + spawn works · brain pinboard + catalogue + wikilink navigation · keys 1/2/3/4/t · terminal typing still works and `1` in a shell types a `1`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(native-app): field notes polish — notifications, empty states, cleanup"
```

---

### Task 15: Config-file themes — palettes loadable from TOML

**User requirement (added mid-execution):** colors must be themable via config files so theming is really easy; one theme file stores BOTH the light and dark palettes.

**Files:**
- Modify: `crates/ninox-app/src/theme.rs` (theme-file loading, hex parsing, overlay)
- Modify: `crates/ninox-core/src/config.rs` (`AppConfig.theme_file: Option<String>`, serde-defaulted)
- Modify: `crates/ninox-app/src/app.rs` (App holds loaded `Themes`; `SwitchTheme`/startup read from it)
- Test: `theme.rs` tests

**Interfaces:**
- Consumes: `AppConfig` TOML mechanics (`toml` crate already a workspace dep; config at `~/.config/ninox/config.toml`, `AppConfig::config_path()`).
- Produces:

```rust
// theme.rs
pub struct Themes { pub light: ColorScheme, pub dark: ColorScheme }
impl Themes {
    /// Built-in Field Notes palettes.
    pub fn builtin() -> Self { Themes { light: light(), dark: dark() } }
    /// builtin() overlaid with the user's theme file (missing keys keep defaults).
    pub fn load(theme_file: Option<&str>) -> Self;
    pub fn scheme(&self, v: ThemeVariant) -> ColorScheme; // Light→light, Dark|Ninox→dark
}
pub fn parse_hex(s: &str) -> Option<Color>;               // "#rrggbb" | "#rrggbbaa"
pub(crate) fn apply_palette(base: &mut ColorScheme, table: &toml::Table); // by token name
pub const TOKEN_NAMES: &[&str]; // all 28 field names — single source of truth
pub fn write_default_theme_file(path: &std::path::Path) -> std::io::Result<()>; // full palettes
```

**Semantics:**
- Theme files live in `~/.config/ninox/themes/<name>.toml` (same parent dir as `config.toml`). `AppConfig.theme_file = Some("field-notes")` resolves to `themes/field-notes.toml`; an absolute-path value is used as-is; `None` → use `themes/field-notes.toml` if it exists, else pure builtins.
- File format — one file, both palettes, token = hex string:

```toml
# ~/.config/ninox/themes/field-notes.toml
[light]
paper = "#f5f0e4"
accent = "#c8451f"
# … any subset of tokens; omitted tokens keep the built-in Field Notes value

[dark]
paper = "#171410"
accent = "#e06038"
```

- Token keys = `ColorScheme` field names exactly (paper, paper_2, card, ink, ink_2, faint, rule, rule_dark, accent, shadow, status_working, status_pr_open, status_ci_failed, status_review, status_mergeable, status_done, cat_pattern, cat_decision, cat_relationship, cat_error, term_bg, term_bar, term_bar_border, term_fg, term_ok, term_err, term_agent, term_dim). Unknown keys → `tracing::warn!`, not an error. Malformed hex → warn + keep default. Missing/unreadable file → warn + builtins (never crash the app over a theme file).
- First run: if the themes dir doesn't exist, create it and write `themes/field-notes.toml` via `write_default_theme_file` containing the FULL default palettes (users edit a complete, working example).
- Single source of truth: a `fn token_slot<'a>(s: &'a mut ColorScheme, name: &str) -> Option<&'a mut Color>` match serves both `apply_palette` (write into the scheme) and `write_default_theme_file` (read each token out of a fresh default via the same names) — no 28-way duplication in two places.
- `App` gains `pub themes: Themes`, loaded once at startup from `config.theme_file`; the `SwitchTheme` handler and startup scheme selection use `state.themes.scheme(variant)` instead of `crate::theme::from_variant(variant)`. Keep `from_variant` as a thin wrapper over `Themes::builtin().scheme(v)` for tests/back-compat.

- [ ] **Step 1: Write failing tests** (in `theme.rs`)

```rust
#[cfg(test)]
mod theme_file_tests {
    use super::*;

    #[test]
    fn parse_hex_variants() {
        assert_eq!(parse_hex("#c8451f"), Some(iced::color!(0xc8451f)));
        assert!(parse_hex("#c8451f80").is_some()); // alpha form parses
        assert_eq!(parse_hex("c8451f"), parse_hex("#c8451f")); // leading # optional
        assert_eq!(parse_hex("#xyz"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn overlay_keeps_defaults_for_missing_keys() {
        let table: toml::Table = toml::from_str(r##"paper = "#101010""##).unwrap();
        let mut s = light();
        apply_palette(&mut s, &table);
        assert_eq!(s.paper, iced::color!(0x101010));   // overridden
        assert_eq!(s.accent, light().accent);           // untouched
    }

    #[test]
    fn default_theme_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("field-notes.toml");
        write_default_theme_file(&p).unwrap();
        let doc: toml::Table =
            toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let light_tbl = doc["light"].as_table().unwrap();
        assert_eq!(light_tbl.len(), TOKEN_NAMES.len()); // full palette written
        let mut s = dark();
        apply_palette(&mut s, light_tbl);
        assert_eq!(s.paper, light().paper); // applying the written light palette reproduces light()
        assert_eq!(s.term_dim, light().term_dim);
    }

    #[test]
    fn themes_load_missing_file_falls_back_to_builtin() {
        let t = Themes::load(Some("/nonexistent/path/nope.toml"));
        assert_eq!(t.light.paper, light().paper);
        assert_eq!(t.dark.accent, dark().accent);
    }
}
```

(`ColorScheme` needs `PartialEq` in its derive for these asserts. `tempfile` is already a dev-dependency of ninox-app. Add `toml = { workspace = true }` to ninox-app `[dependencies]`.)

- [ ] **Step 2: Verify failure** — `cargo test -p ninox theme_file` → FAIL (functions missing).

- [ ] **Step 3: Implement** per Interfaces/Semantics. `Themes::load` resolution: bare name → `AppConfig::config_path().parent().join("themes").join(format!("{name}.toml"))`; value containing `/` treated as a path (expand leading `~` via `dirs::home_dir()`).

- [ ] **Step 4: Wire into the app** — `AppConfig.theme_file: Option<String>` with `#[serde(default)]`; App startup: create themes dir + write default file if absent, then `Themes::load(config.theme_file.as_deref())`; `SwitchTheme` uses `state.themes.scheme(variant)`. Keyboard `t` toggle and theme dots unchanged (they switch variants within the loaded theme, not files).

- [ ] **Step 5: Run** — `cargo test -p ninox && cargo test -p ninox-core && cargo build -p ninox` → PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-app crates/ninox-core
git commit -m "feat(native-app): load theme palettes from config-dir TOML theme files"
```

---

## Self-Review Notes (already applied)

- Spec coverage: §1 tokens → Task 2; §2 fonts → Tasks 1/3; §3 texture rules → Task 3 helpers (grain + rotation consciously skipped per §8); §4 sidebar → Task 4; §5 fleet/session/PR/brain → Tasks 6/7/9/11–13; spawn modal → Task 10; §6 interactions → Tasks 5/6 (hover) — mockup deep-links are N/A for a native app; §7 empty states/notifications → Task 14; §7 items explicitly deferred by spec (ninox theme, temporal scrubber, drag-handle styling, ⬡ orchestrator badge) are deferred here too.
- Six-column merge (Done+Terminated) is an intentional deviation from the current 7-column board, matching spec §1's status mapping and §5's six ledger columns; stamps still distinguish Filed vs Closed.
- `folio_title`'s hour ranges and `stamp_word`'s vocabulary are locked by tests.
- iced API guesses that need on-the-ground verification are marked inline (canvas event paths, markdown Settings, text_editor availability).
