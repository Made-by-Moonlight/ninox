use ninox_core::{config::AppConfig, types::SessionStatus, ThemeVariant};
use iced::{color, Color, Theme};
use std::path::{Path, PathBuf};

/// Field Notes design tokens — spec: docs/design-concepts/field-notes-design.md §1.
/// The dark theme is the same journal read by lamplight, not a separate design.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    // mode flag — NOT a theme-file token (see TOKEN_NAMES); user palettes
    // can't override it, it's never written to a theme file.
    pub dark: bool,
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

/// Maps a token name to the corresponding mutable `Color` slot in `s`.
/// Serves both `apply_palette` (write into a scheme) and
/// `write_default_theme_file` (read tokens out of a fresh default), so the
/// 28-way field list lives in exactly one place.
fn token_slot<'a>(s: &'a mut ColorScheme, name: &str) -> Option<&'a mut Color> {
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
pub fn parse_hex(s: &str) -> Option<Color> {
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
    Some(Color::from_rgba8(r, g, b, a as f32 / 255.0))
}

fn to_hex(c: Color) -> String {
    let r = (c.r * 255.0).round() as u8;
    let g = (c.g * 255.0).round() as u8;
    let b = (c.b * 255.0).round() as u8;
    if c.a != 1.0 {
        let a = (c.a * 255.0).round() as u8;
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}")
    }
}

/// Overlays `table` onto `base` by token name. Unknown keys and malformed
/// hex values are logged and skipped — never fatal.
pub(crate) fn apply_palette(base: &mut ColorScheme, table: &toml::Table) {
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
        dark: false,
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
        dark: true,
    }
}

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

    #[test]
    fn to_hex_round_trips_alpha() {
        // parse_hex quantizes alpha to u8 via `a as f32 / 255.0`, so encoding
        // the resulting color back should reproduce the exact same u8 octets
        // — hence exact equality, not epsilon comparison.
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
