//! Brain tab dispatcher (v1 `Brain.dc.html`/`BrainGraph.dc.html`/
//! `BrainSources.dc.html`) plus the wikilink helpers shared by the reading
//! pane and `Message::BrainLinkClicked` — these are pure text transforms,
//! independent of the view's visual theme, so they outlived the Field-Notes
//! rewrite below unchanged. The List view's own rendering lives in
//! `brain_list`; Graph and Sources are placeholders until MLOPS-5339
//! PR2/PR3 land.

use iced::Element;

use crate::app::{App, BrainMode, Message};

// ---------------------------------------------------------------------------
// Pure helpers (wikilinks, backlinks) — TDD'd in `mod tests` at the bottom
// of this file.
// ---------------------------------------------------------------------------

/// Parse the raw text between `[[` and `]]`, handling Obsidian's optional
/// `|alias` display-text override and `#heading`/`#^block` suffix. Returns
/// `(target, display)`:
/// - `target` is used for resolution/backlink matching, with any
///   `#heading`/`#^block` suffix stripped (section-level navigation is out
///   of scope — links resolve to the whole entry).
/// - `display` is the text rendered in the reading pane: the alias when
///   present (`[[target#sec|label]]` → `label`), otherwise the original
///   text as typed, `#section` included (`[[note#sec]]` → `note#sec`).
///
/// Embeds (`![[x]]`) are not special-cased — the leading `!` is left alone
/// and `[[x]]` is parsed/linked normally, so an embed renders as a literal
/// `!` immediately followed by a clickable link (out of scope: no image/
/// block transclusion).
fn parse_wikilink_content(content: &str) -> (String, String) {
    let (before_pipe, display) = match content.find('|') {
        Some(i) => (&content[..i], content[i + 1..].trim().to_string()),
        None => (content, content.trim().to_string()),
    };
    let target = match before_pipe.find('#') {
        Some(i) => before_pipe[..i].trim().to_string(),
        None => before_pipe.trim().to_string(),
    };
    (target, display)
}

/// `[[x]]` → `[x](<brain link to x>)` (see `internal_link`) so the markdown
/// widget renders clickable links — plus Obsidian's `[[x|alias]]` (display =
/// alias) and `[[x#heading]]` (display keeps the `#heading`, link resolves to
/// `x`; see `parse_wikilink_content`).
#[cfg(test)]
pub fn preprocess_wikilinks(body: &str) -> String {
    preprocess_wikilinks_resolving(body, |_| true)
}

/// [`preprocess_wikilinks`], but a target `resolves` rejects becomes an
/// `InternalLink::Unresolved` link, which `brain_markdown` draws dim and inert.
pub fn preprocess_wikilinks_resolving(body: &str, resolves: impl Fn(&str) -> bool) -> String {
    use crate::components::internal_link::{encode, InternalLink};
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let (target, display) = parse_wikilink_content(&after[..end]);
                let link = if resolves(&target) { InternalLink::Brain(target) } else { InternalLink::Unresolved(target) };
                let url = encode(&link);
                out.push_str(&format!("[{display}]({url})"));
                rest = &after[end + 2..];
            }
            None => { out.push_str("[["); rest = after; }
        }
    }
    out.push_str(rest);
    out
}

/// Does `link` refer to `entry`? Match on name, full id, or id stem.
fn link_matches(link: &str, entry: &ninox_core::BrainEntry) -> bool {
    let stem = entry.id.rsplit('/').next().unwrap_or(&entry.id).trim_end_matches(".md");
    let link_stem = link.rsplit('/').next().unwrap_or(link);
    link == entry.name || link == entry.id || link_stem == stem || link_stem == entry.name
}

/// Resolve a clicked wikilink to an entry, searching `same_source` first —
/// in "All sources" mode `entries` spans every catalogue, so a link clicked
/// from an entry in source A must prefer a same-named entry also in A over
/// one in B (the merged-index equivalent, `MergedBrainIndex`'s
/// `backlinks`/`outlinks`, applies this same precedence at the data layer).
/// `None` (single-source mode, or no selection) skips straight to the
/// unscoped search.
pub fn resolve_link<'a>(
    entries: &'a [ninox_core::BrainEntry],
    link: &str,
    same_source: Option<&str>,
) -> Option<&'a ninox_core::BrainEntry> {
    if let Some(source) = same_source {
        if let Some(hit) = entries.iter().find(|e| e.source == source && link_matches(link, e)) {
            return Some(hit);
        }
    }
    entries.iter().find(|e| link_matches(link, e))
}

/// An entry's readable title (row titles, the reading pane's heading,
/// backlink/outlink chips) and the matching body-stripper, so the rendered
/// body doesn't repeat that title a second time as its own first heading.
/// Moved to `ninox-core::brain` (next to `normalize_kind`/`kind_label`) so
/// the TUI and CLI share the same rule instead of a second GUI-only copy —
/// re-exported here so `brain_list`'s and `app.rs`'s existing
/// `brain_panel::{display_title, strip_leading_h1}` call sites are
/// unaffected by the move.
pub use ninox_core::brain::{display_title, strip_leading_h1};

/// Longest run of non-whitespace characters left unbroken before a
/// zero-width space (U+200B) is inserted — see `insert_soft_breaks`.
const SOFT_BREAK_RUN: usize = 20;

/// Inserts invisible zero-width spaces into long unbroken runs of
/// non-whitespace text (URLs, file paths) so they have somewhere to wrap.
/// `iced::widget::markdown::view` always wraps at the word level with no
/// way to opt into glyph-level fallback (`iced_core::text::Wrapping::
/// WordOrGlyph` isn't exposed through the markdown widget's `Settings`),
/// so a token with no natural break point otherwise overflows the reading
/// pane uncut. Skips inline code spans/fenced blocks and markdown link
/// destinations (`](...)`) — breaking those would change what gets
/// copied, or the literal URL a click opens.
pub fn insert_soft_breaks(markdown: &str) -> String {
    let chars: Vec<char> = markdown.chars().collect();
    let mut out = String::with_capacity(markdown.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];

        if ch == '`' {
            let mut j = i;
            while j < chars.len() && chars[j] == '`' {
                j += 1;
            }
            let tick_len = j - i;
            let mut k = j;
            while k < chars.len() {
                if chars[k] == '`' {
                    let run_start = k;
                    while k < chars.len() && chars[k] == '`' {
                        k += 1;
                    }
                    if k - run_start == tick_len {
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            out.extend(&chars[i..k.min(chars.len())]);
            i = k;
            continue;
        }

        if ch == ']' && chars.get(i + 1) == Some(&'(') {
            let mut j = i + 2;
            while j < chars.len() && chars[j] != ')' {
                j += 1;
            }
            j = (j + 1).min(chars.len());
            out.extend(&chars[i..j]);
            i = j;
            continue;
        }

        // A `[[wikilink]]` target — copied verbatim, same reasoning as the
        // `](...)` link destination above: these are exactly the long,
        // no-whitespace ids this function would otherwise "fix" (a dated
        // slug like `decisions/2026-10-29-mlops-5339-brain-v1-graph.md` is
        // already over `SOFT_BREAK_RUN`), and `resolve_link` matches on the
        // exact target string, so a ZWSP inserted into it breaks the link
        // instead of just its wrapping.
        if ch == '[' && chars.get(i + 1) == Some(&'[') {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == ']' && chars[j + 1] == ']') {
                j += 1;
            }
            j = (j + 2).min(chars.len());
            out.extend(&chars[i..j]);
            i = j;
            continue;
        }

        if ch.is_whitespace() {
            out.push(ch);
            i += 1;
            continue;
        }

        // A run of plain, non-whitespace text — stop early at a backtick,
        // `]`, or `[` so the next iteration can re-evaluate it as a code
        // span, a link destination, or a wikilink instead of swallowing it
        // into this run. `j` starts one past `i`: `ch` itself is included
        // unconditionally (it may be a bare `]`/`[` that isn't actually
        // part of link syntax — the earlier branches already ruled the
        // syntactic cases out), so the stop check never fires on the run's
        // own first character and `i` always advances.
        let start = i;
        let mut j = i + 1;
        while j < chars.len() && !chars[j].is_whitespace() && chars[j] != '`' && chars[j] != ']' && chars[j] != '[' {
            j += 1;
        }
        let run = &chars[start..j];
        if run.len() > SOFT_BREAK_RUN {
            for (idx, c) in run.iter().enumerate() {
                if idx > 0 && idx % SOFT_BREAK_RUN == 0 {
                    out.push('\u{200B}');
                }
                out.push(*c);
            }
        } else {
            out.extend(run);
        }
        i = j;
    }
    out
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Full-centre Brain view: routes to the List/Graph/Sources body per
/// `app.brain_view.mode`.
pub fn brain_panel(app: &App) -> Element<'_, Message> {
    match app.brain_view.mode {
        BrainMode::List => super::brain_list::list_view(app),
        BrainMode::Graph => super::brain_graph::graph_view(app),
        BrainMode::Sources => super::brain_sources::sources_view(app),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_soft_breaks_leaves_short_tokens_untouched() {
        assert_eq!(insert_soft_breaks("see https://x.co for details"), "see https://x.co for details");
    }

    #[test]
    fn insert_soft_breaks_breaks_a_long_url_in_plain_prose() {
        let out = insert_soft_breaks("(https://app.notion.com/37dc16d22bf18088b6d3c4bd5fa5f7b1)");
        assert!(out.contains('\u{200B}'), "expected a zero-width space in {out:?}");
        // Invisible to the reader: stripping the ZWSPs round-trips exactly.
        assert_eq!(out.replace('\u{200B}', ""), "(https://app.notion.com/37dc16d22bf18088b6d3c4bd5fa5f7b1)");
    }

    #[test]
    fn insert_soft_breaks_skips_markdown_link_destinations() {
        let out = insert_soft_breaks("[2026-Q3 brainstorming](https://app.notion.com/37dc16d22bf18088b6d3c4bd5fa5f7b1)");
        assert!(!out.contains('\u{200B}'), "a link destination must stay exact so it still opens: {out:?}");
    }

    #[test]
    fn insert_soft_breaks_skips_inline_code_and_fences() {
        let inline = insert_soft_breaks("`a_very_long_identifier_that_would_otherwise_wrap_here`");
        assert!(!inline.contains('\u{200B}'));

        let fenced = insert_soft_breaks("```\na_very_long_identifier_that_would_otherwise_wrap_here\n```");
        assert!(!fenced.contains('\u{200B}'));
    }

    #[test]
    fn insert_soft_breaks_skips_wikilink_targets() {
        let out = insert_soft_breaks("see [[decisions/2026-07-06-very-long-wikilink-target-slug]] for context");
        assert!(
            !out.contains('\u{200B}'),
            "a wikilink target must stay exact so resolve_link still matches it: {out:?}"
        );
    }

    #[test]
    fn insert_soft_breaks_is_idempotent_on_ordinary_prose() {
        let prose = "A normal sentence with no long tokens at all.";
        assert_eq!(insert_soft_breaks(prose), prose);
    }

    #[test]
    fn preprocesses_wikilinks_to_brain_urls() {
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment]]."),
            "see [frame-alignment](https://ninox.invalid/brain/frame-alignment)."
        );
    }

    #[test]
    fn preprocesses_aliased_link_using_alias_as_display_text() {
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment|the pattern]]."),
            "see [the pattern](https://ninox.invalid/brain/frame-alignment)."
        );
    }

    #[test]
    fn preprocesses_heading_link_displaying_the_full_original_text() {
        // No alias — the display text is exactly as typed, `#section`
        // included, while the link destination resolves to the bare target.
        assert_eq!(
            preprocess_wikilinks("see [[frame-alignment#Rationale]]."),
            "see [frame-alignment#Rationale](https://ninox.invalid/brain/frame-alignment)."
        );
    }

    #[test]
    fn preprocesses_combined_heading_and_alias_link() {
        assert_eq!(
            preprocess_wikilinks("see [[a#b|c]]."),
            "see [c](https://ninox.invalid/brain/a)."
        );
    }

    #[test]
    fn embeds_render_as_literal_bang_plus_link_out_of_scope() {
        // `![[x]]` is not treated as a transclusion — the leading `!` is
        // left untouched and `[[x]]` is preprocessed normally, so an embed
        // renders as a literal "!" immediately followed by a clickable link.
        assert_eq!(
            preprocess_wikilinks("before ![[diagram]] after"),
            "before ![diagram](https://ninox.invalid/brain/diagram) after"
        );
    }

    #[test]
    fn preprocesses_wikilinks_with_spaces_to_valid_commonmark() {
        // A raw space in the destination would end the CommonMark link at
        // the first word ("[my target](https://ninox.invalid/brain/my target)." parses as a
        // link to "my" followed by literal text) — the destination must be
        // percent-encoded while the visible link text stays readable.
        assert_eq!(
            preprocess_wikilinks("see [[my target]]."),
            "see [my target](https://ninox.invalid/brain/my%20target)."
        );
    }

    #[test]
    fn a_preprocessed_wikilink_is_still_a_link_after_markdown_parse() {
        let md = preprocess_wikilinks("see [[my target (x)|it]] now");
        let style = iced::widget::markdown::Style {
            inline_code_highlight: iced::widget::markdown::Highlight {
                background: iced::Background::Color(iced::Color::BLACK),
                border: iced::Border::default(),
            },
            inline_code_padding: iced::Padding::ZERO,
            inline_code_color: iced::Color::WHITE,
            link_color: iced::Color::WHITE,
        };
        let links: Vec<_> = iced::widget::markdown::parse(&md)
            .filter_map(|item| match item {
                iced::widget::markdown::Item::Paragraph(text) => Some(text),
                _ => None,
            })
            .flat_map(|text| text.spans(style).iter().filter_map(|s| s.link.clone()).collect::<Vec<_>>())
            .collect();
        assert_eq!(links.len(), 1, "{md}");
        assert_eq!(
            crate::components::internal_link::decode(&links[0]),
            Some(crate::components::internal_link::InternalLink::Brain("my target (x)".into()))
        );
    }
}
