use ninox_core::{config::AppConfig, types::SessionStatus, ThemeVariant};
use std::path::{Path, PathBuf};

/// Toolkit-neutral 8-bit RGBA color — the shared theme-token type both the
/// iced GUI (via a `From<Rgba> for iced::Color`) and the ratatui TUI (via a
/// direct u8 → `ratatui::style::Color` conversion) build their own
/// presentation types from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba { r: 0, g: 0, b: 0, a: 255 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

/// A `0xRRGGBB` literal as an opaque `Rgba` — the `color!`-macro replacement
/// for the builtin Field Notes palettes below.
const fn hex(v: u32) -> Rgba {
    Rgba::rgb(((v >> 16) & 0xFF) as u8, ((v >> 8) & 0xFF) as u8, (v & 0xFF) as u8)
}

/// Field Notes design tokens — spec: docs/design-concepts/field-notes-design.md §1.
/// The dark theme is the same journal read by lamplight, not a separate design.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorScheme {
    // surfaces & ink
    pub paper:     Rgba, // app background
    pub paper_2:   Rgba, // sidebar, modal header, table header
    pub card:      Rgba, // cards, panels, modals, reading pane
    pub ink:       Rgba, // primary text, heavy borders
    pub ink_2:     Rgba, // secondary text
    pub faint:     Rgba, // tertiary/metadata text
    pub rule:      Rgba, // light rules/separators
    pub rule_dark: Rgba, // stronger rules, input underlines, card borders
    pub accent:    Rgba, // vermilion
    pub shadow:    Rgba, // hard-offset shadow base (alpha applied per-use)
    // status
    pub status_working:   Rgba,
    pub status_pr_open:   Rgba,
    pub status_ci_failed: Rgba,
    pub status_review:    Rgba,
    pub status_mergeable: Rgba,
    pub status_done:      Rgba,
    // brain categories beyond the status palette
    pub cat_pattern:      Rgba,
    pub cat_decision:     Rgba,
    pub cat_relationship: Rgba,
    pub cat_error:        Rgba,
    // terminal — "the dark object" on the page
    pub term_bg:         Rgba,
    pub term_bar:        Rgba,
    pub term_bar_border: Rgba,
    pub term_fg:         Rgba,
    pub term_ok:         Rgba,
    pub term_err:        Rgba,
    pub term_agent:      Rgba,
    pub term_dim:        Rgba,
    /// 16-entry ANSI palette (0-7 normal, 8-15 bright) used to render
    /// terminal text when a cell requests a named color that isn't
    /// otherwise overridden by the emulator's dynamic color table — see
    /// `terminal::ansi_to_iced`. Field Notes: identical in both themes,
    /// the terminal is "the dark object" on the page (spec §1).
    pub ansi: [Rgba; 16],
    // mode flag — NOT a theme-file token (see TOKEN_NAMES); user palettes
    // can't override it, it's never written to a theme file.
    pub dark: bool,
}

impl ColorScheme {
    pub fn status_color(&self, status: &SessionStatus) -> Rgba {
        use SessionStatus::*;
        match status {
            Spawning | Working => self.status_working,
            PrOpen             => self.status_pr_open,
            CiFailed           => self.status_ci_failed,
            ReviewPending      => self.status_review,
            Mergeable          => self.status_mergeable,
            Done | Terminated  => self.status_done,
            Interrupted        => self.status_review,
            AwaitingReview     => self.status_review,
            Queued             => self.status_done,
        }
    }
}

/// Thin wrapper over `Themes::builtin().scheme(v)`, kept for tests/back-compat.
#[allow(dead_code)]
pub fn from_variant(v: ThemeVariant) -> ColorScheme {
    Themes::builtin().scheme(v)
}

// ---------------------------------------------------------------------------
// Config-file themes — palettes loadable from TOML.
//
// Users edit ONE theme file (`~/.config/ninox/themes/<name>.toml`) containing
// both a `[light]` and a `[dark]` table. Missing keys keep the built-in Field
// Notes values; a bad theme file never crashes the app — it's logged and we
// fall back to builtins.
// ---------------------------------------------------------------------------

/// All 28 `ColorScheme` field names — single source of truth for theme-file
/// token keys, shared by `apply_palette` and `write_default_theme_file`.
pub const TOKEN_NAMES: &[&str] = &[
    "paper", "paper_2", "card", "ink", "ink_2", "faint", "rule", "rule_dark",
    "accent", "shadow",
    "status_working", "status_pr_open", "status_ci_failed", "status_review",
    "status_mergeable", "status_done",
    "cat_pattern", "cat_decision", "cat_relationship", "cat_error",
    "term_bg", "term_bar", "term_bar_border", "term_fg", "term_ok", "term_err",
    "term_agent", "term_dim",
];

/// Maps a token name to the corresponding mutable `Rgba` slot in `s`.
/// Serves both `apply_palette` (write into a scheme) and
/// `write_default_theme_file` (read tokens out of a fresh default), so the
/// 28-way field list lives in exactly one place.
fn token_slot<'a>(s: &'a mut ColorScheme, name: &str) -> Option<&'a mut Rgba> {
    Some(match name {
        "paper" => &mut s.paper,
        "paper_2" => &mut s.paper_2,
        "card" => &mut s.card,
        "ink" => &mut s.ink,
        "ink_2" => &mut s.ink_2,
        "faint" => &mut s.faint,
        "rule" => &mut s.rule,
        "rule_dark" => &mut s.rule_dark,
        "accent" => &mut s.accent,
        "shadow" => &mut s.shadow,
        "status_working" => &mut s.status_working,
        "status_pr_open" => &mut s.status_pr_open,
        "status_ci_failed" => &mut s.status_ci_failed,
        "status_review" => &mut s.status_review,
        "status_mergeable" => &mut s.status_mergeable,
        "status_done" => &mut s.status_done,
        "cat_pattern" => &mut s.cat_pattern,
        "cat_decision" => &mut s.cat_decision,
        "cat_relationship" => &mut s.cat_relationship,
        "cat_error" => &mut s.cat_error,
        "term_bg" => &mut s.term_bg,
        "term_bar" => &mut s.term_bar,
        "term_bar_border" => &mut s.term_bar_border,
        "term_fg" => &mut s.term_fg,
        "term_ok" => &mut s.term_ok,
        "term_err" => &mut s.term_err,
        "term_agent" => &mut s.term_agent,
        "term_dim" => &mut s.term_dim,
        _ => return None,
    })
}

/// Parses a `"#rrggbb"` or `"#rrggbbaa"` hex color string. The leading `#`
/// is optional.
pub fn parse_hex(s: &str) -> Option<Rgba> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 && s.len() != 8 {
        return None;
    }
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    let a = if s.len() == 8 {
        u8::from_str_radix(&s[6..8], 16).ok()?
    } else {
        255
    };
    Some(Rgba { r, g, b, a })
}

fn to_hex(c: Rgba) -> String {
    if c.a != 255 {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    } else {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    }
}

/// Overlays `table` onto `base` by token name. Unknown keys and malformed
/// hex values are logged and skipped — never fatal.
pub fn apply_palette(base: &mut ColorScheme, table: &toml::Table) {
    for (key, value) in table {
        let Some(slot) = token_slot(base, key) else {
            tracing::warn!("unknown theme token '{key}'; ignoring");
            continue;
        };
        let Some(hex_str) = value.as_str() else {
            tracing::warn!("theme token '{key}' is not a string; keeping default");
            continue;
        };
        match parse_hex(hex_str) {
            Some(color) => *slot = color,
            None => tracing::warn!(
                "invalid hex color '{hex_str}' for theme token '{key}'; keeping default"
            ),
        }
    }
}

fn palette_table(scheme: ColorScheme) -> toml::Table {
    let mut scheme = scheme;
    let mut table = toml::Table::new();
    for name in TOKEN_NAMES {
        if let Some(slot) = token_slot(&mut scheme, name) {
            table.insert((*name).to_string(), toml::Value::String(to_hex(*slot)));
        }
    }
    table
}

/// Writes a complete default theme file (both palettes, all tokens) to
/// `path`, creating parent directories as needed. Users get a full, working
/// example to edit rather than a blank/partial file.
pub fn write_default_theme_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut doc = toml::Table::new();
    doc.insert("light".to_string(), toml::Value::Table(palette_table(light())));
    doc.insert("dark".to_string(), toml::Value::Table(palette_table(dark())));
    let contents = toml::to_string_pretty(&doc).map_err(std::io::Error::other)?;
    std::fs::write(path, contents)
}

/// First-run seeding: ensures `themes_dir/field-notes.toml` exists, writing a
/// complete default theme file if absent. Creates `themes_dir` if needed.
/// Never overwrites an existing file — a user's customized theme is never
/// clobbered by a later run. Returns the path to the (possibly pre-existing)
/// file.
pub fn ensure_default_theme_file(themes_dir: &Path) -> std::io::Result<PathBuf> {
    let default_path = themes_dir.join("field-notes.toml");
    if !default_path.exists() {
        write_default_theme_file(&default_path)?;
    }
    Ok(default_path)
}

/// Both loaded palettes — the builtin Field Notes defaults, optionally
/// overlaid with a user's config-dir theme file.
#[derive(Debug, Clone)]
pub struct Themes {
    pub light: ColorScheme,
    pub dark:  ColorScheme,
}

impl Themes {
    /// Built-in Field Notes palettes.
    pub fn builtin() -> Self {
        Themes { light: light(), dark: dark() }
    }

    /// `builtin()` overlaid with the user's theme file (missing keys keep
    /// defaults). Never fails — a missing/unreadable/malformed file just
    /// falls back to builtins, with a warning.
    pub fn load(theme_file: Option<&str>) -> Self {
        let mut themes = Self::builtin();

        let Some(path) = resolve_theme_path(theme_file) else {
            return themes;
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("theme file {} unreadable: {e}", path.display());
                return themes;
            }
        };

        let doc: toml::Table = match toml::from_str(&contents) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("theme file {} has invalid TOML: {e}", path.display());
                return themes;
            }
        };

        if let Some(light_tbl) = doc.get("light").and_then(|v| v.as_table()) {
            apply_palette(&mut themes.light, light_tbl);
        }
        if let Some(dark_tbl) = doc.get("dark").and_then(|v| v.as_table()) {
            apply_palette(&mut themes.dark, dark_tbl);
        }

        themes
    }

    /// Light → light, Dark|Ninox → dark.
    pub fn scheme(&self, v: ThemeVariant) -> ColorScheme {
        match v {
            ThemeVariant::Light => self.light,
            ThemeVariant::Dark | ThemeVariant::Ninox => self.dark,
        }
    }
}

/// Resolves a `theme_file` config value to a concrete path:
/// - `None` → `themes/field-notes.toml` next to `config.toml`, if it exists.
/// - A bare name (no `/`) → `themes/<name>.toml` next to `config.toml`.
/// - A value containing `/` is used as a path as-is, expanding a leading
///   `~/` via `dirs::home_dir()`.
fn resolve_theme_path(theme_file: Option<&str>) -> Option<PathBuf> {
    let themes_dir = || AppConfig::config_path().parent().map(|p| p.join("themes"));

    match theme_file {
        Some(s) if s.contains('/') => {
            if let Some(rest) = s.strip_prefix("~/") {
                dirs::home_dir().map(|h| h.join(rest))
            } else {
                Some(PathBuf::from(s))
            }
        }
        Some(name) => themes_dir().map(|d| d.join(format!("{name}.toml"))),
        None => {
            let p = themes_dir()?.join("field-notes.toml");
            p.exists().then_some(p)
        }
    }
}

pub fn light() -> ColorScheme {
    ColorScheme {
        paper:     hex(0xf5f0e4),
        paper_2:   hex(0xefe8d8),
        card:      hex(0xfbf7ee),
        ink:       hex(0x211d16),
        ink_2:     hex(0x5b5344),
        faint:     hex(0x968a72),
        rule:      hex(0xd9cfba),
        rule_dark: hex(0xb7ab90),
        accent:    hex(0xc8451f),
        shadow:    hex(0x211d16),
        status_working:   hex(0x3e7d34),
        status_pr_open:   hex(0x20629e),
        status_ci_failed: hex(0xc8451f),
        status_review:    hex(0xa97913),
        status_mergeable: hex(0x6d4fa3),
        status_done:      hex(0x8b8272),
        cat_pattern:      hex(0xa23f8c),
        cat_decision:     hex(0xc86a1f),
        cat_relationship: hex(0x2a8a80),
        cat_error:        hex(0xb3261e),
        term_bg:         hex(0x23201a),
        term_bar:        hex(0x2c2822),
        term_bar_border: hex(0x3a352c),
        term_fg:         hex(0xece4d0),
        term_ok:         hex(0x8fd37f),
        term_err:        hex(0xf08a72),
        term_agent:      hex(0xf0c069),
        term_dim:        hex(0x7a7260),
        ansi: [
            hex(0x2c2822), hex(0xf08a72), hex(0x8fd37f), hex(0xf0c069),
            hex(0x7ea9d4), hex(0xc876b4), hex(0x4ab0a4), hex(0xece4d0),
            hex(0x7a7260), hex(0xf4a58f), hex(0xa8e29a), hex(0xf5d08a),
            hex(0x9dc1e4), hex(0xd996c8), hex(0x6fc4ba), hex(0xf5efdd),
        ],
        dark: false,
    }
}

pub fn dark() -> ColorScheme {
    ColorScheme {
        paper:     hex(0x171410),
        paper_2:   hex(0x1f1b15),
        card:      hex(0x262119),
        ink:       hex(0xece3cd),
        ink_2:     hex(0xb5a98d),
        faint:     hex(0x83775c),
        rule:      hex(0x393227),
        rule_dark: hex(0x4e4534),
        accent:    hex(0xe06038),
        shadow:    hex(0x000000),
        status_working:   hex(0x7cc46a),
        status_pr_open:   hex(0x5ca8e8),
        status_ci_failed: hex(0xe86a4c),
        status_review:    hex(0xd8a83c),
        status_mergeable: hex(0xa184d6),
        status_done:      hex(0x7d7461),
        cat_pattern:      hex(0xc876b4),
        cat_decision:     hex(0xe08a4a),
        cat_relationship: hex(0x4ab0a4),
        cat_error:        hex(0xe0604a),
        term_bg:         hex(0x100d09),
        term_bar:        hex(0x191510),
        term_bar_border: hex(0x2c261d),
        term_fg:         hex(0xece4d0),
        term_ok:         hex(0x8fd37f),
        term_err:        hex(0xf08a72),
        term_agent:      hex(0xf0c069),
        term_dim:        hex(0x7a7260),
        ansi: [
            hex(0x2c2822), hex(0xf08a72), hex(0x8fd37f), hex(0xf0c069),
            hex(0x7ea9d4), hex(0xc876b4), hex(0x4ab0a4), hex(0xece4d0),
            hex(0x7a7260), hex(0xf4a58f), hex(0xa8e29a), hex(0xf5d08a),
            hex(0x9dc1e4), hex(0xd996c8), hex(0x6fc4ba), hex(0xf5efdd),
        ],
        dark: true,
    }
}

#[cfg(test)]
mod theme_file_tests {
    use super::*;

    #[test]
    fn parse_hex_variants() {
        assert_eq!(parse_hex("#c8451f"), Some(hex(0xc8451f)));
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
        assert_eq!(s.paper, hex(0x101010));   // overridden
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

    #[test]
    fn to_hex_round_trips_alpha() {
        // parse_hex/to_hex work in exact u8 octets now, so no epsilon needed.
        let c = parse_hex("#c8451f80").unwrap();
        assert_eq!(to_hex(c), "#c8451f80");
        assert_eq!(parse_hex(&to_hex(c)), Some(c));
    }

    #[test]
    fn to_hex_omits_alpha_when_opaque() {
        let c = parse_hex("#c8451f").unwrap();
        assert_eq!(to_hex(c), "#c8451f");
    }

    #[test]
    fn ensure_default_theme_file_seeds_fresh_dir() {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("themes");
        let path = ensure_default_theme_file(&themes_dir).unwrap();
        assert_eq!(path, themes_dir.join("field-notes.toml"));
        assert!(path.exists());
        let doc: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let light_tbl = doc["light"].as_table().unwrap();
        let dark_tbl = doc["dark"].as_table().unwrap();
        assert_eq!(light_tbl.len(), TOKEN_NAMES.len());
        assert_eq!(dark_tbl.len(), TOKEN_NAMES.len());
    }

    #[test]
    fn ensure_default_theme_file_never_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let themes_dir = dir.path().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        let path = themes_dir.join("field-notes.toml");
        std::fs::write(&path, "custom = true\n").unwrap();

        let returned = ensure_default_theme_file(&themes_dir).unwrap();
        assert_eq!(returned, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "custom = true\n");
    }
}
