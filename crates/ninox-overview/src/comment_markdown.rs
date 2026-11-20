//! Cleans a PR/Marginalia review comment body before markdown rendering
//! (MLOPS-5339 slice 5 item 1) — bodies from GitHub review sync frequently
//! carry an `<!-- linear-linkback -->` marker comment and a
//! `<details><summary>…</summary>…</details>` ticket preview. An `<!--
//! …-->` HTML comment needs no special handling here: `pulldown_cmark`
//! already turns it into an opaque `Event::Html`/`Event::InlineHtml`, which
//! `markdown_blocks::parse_blocks` already drops — hand-rolling that same
//! drop here too previously matched `<!--` anywhere (even inside inline
//! code) and, for one with no closing `-->`, silently ate every byte after
//! it (code-review finding); removed in favor of just leaving it alone.
//! `<details>`/`<summary>` need real help, though: they're CommonMark
//! "type 6" HTML-block tags, so `pulldown_cmark` swallows the whole block
//! (including any markdown-looking text inside it) as one raw, unparsed
//! chunk with nothing worth keeping — collapsing it to just its summary
//! line keeps the one piece worth showing (e.g. a linked ticket title)
//! instead of either raw markup or nothing at all.

fn strip_html_tags(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn is_line_start(haystack: &str, idx: usize) -> bool {
    idx == 0 || haystack.as_bytes().get(idx - 1) == Some(&b'\n')
}

/// Finds the next `<details` at or after `from` that starts its own line —
/// CommonMark's HTML-block start rule — so a mention mid-sentence or inside
/// inline code (`` `<details>` ``) is never mistaken for a real block
/// (code-review finding: the original match-anywhere version did exactly
/// that).
fn find_details_open(lower: &str, from: usize) -> Option<usize> {
    let mut search_from = from;
    loop {
        let idx = search_from + lower[search_from..].find("<details")?;
        if is_line_start(lower, idx) {
            return Some(idx);
        }
        search_from = idx + "<details".len();
    }
}

/// Finds each `<details>…</details>` block (case-insensitive tag match —
/// lowercasing is ASCII-only, so it never shifts byte offsets) and replaces
/// it with just its `<summary>` text, HTML-tag-stripped. A `<details>`
/// block with no `<summary>` inside collapses to nothing. A block with no
/// `</details>` is left completely untouched instead of dropped (code-
/// review finding, Review Focus #3): the downstream CommonMark parser's own
/// HTML-block rule already ends an unterminated block at the next blank
/// line, not at end of document, so inventing a "drop everything after it"
/// rule here would wrongly swallow real content that follows.
fn collapse_details(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::new();
    let mut pos = 0usize;
    loop {
        let Some(start) = find_details_open(&lower, pos) else {
            out.push_str(&input[pos..]);
            break;
        };
        out.push_str(&input[pos..start]);
        let Some(rel_end_tag) = lower[start..].find("</details>") else {
            out.push_str(&input[start..]);
            break;
        };
        let end = start + rel_end_tag + "</details>".len();
        let block = &input[start..end];
        let block_lower = &lower[start..end];
        if let Some(s_rel) = block_lower.find("<summary") {
            if let Some(open_rel) = block[s_rel..].find('>') {
                let after_open = s_rel + open_rel + 1;
                if let Some(s_end_rel) = block_lower[after_open..].find("</summary>") {
                    let summary = strip_html_tags(&block[after_open..after_open + s_end_rel]);
                    let summary = summary.trim();
                    if !summary.is_empty() {
                        out.push_str(summary);
                        out.push('\n');
                    }
                }
            }
        }
        pos = end;
    }
    out
}

pub fn clean_comment_markdown(body: &str) -> String {
    collapse_details(body)
}

/// Removes each `<!-- … -->` HTML comment (e.g. a Linear linkback marker,
/// a bot's hidden automation id) as one unit. Unlike `clean_comment_markdown`
/// (whose output still goes through a real markdown renderer that already
/// drops an HTML comment node on its own), `plain_text`'s output has no
/// such renderer downstream, so the comment must actually be removed here.
/// An unterminated `<!--` is left untouched rather than eating everything
/// after it (same discipline as `collapse_details`'s own unterminated-block
/// handling).
fn strip_html_comments(input: &str) -> String {
    let mut out = String::new();
    let mut pos = 0usize;
    while let Some(rel_start) = input[pos..].find("<!--") {
        let start = pos + rel_start;
        out.push_str(&input[pos..start]);
        match input[start..].find("-->") {
            Some(rel_end) => pos = start + rel_end + "-->".len(),
            None => {
                out.push_str(&input[start..]);
                pos = input.len();
                break;
            }
        }
    }
    out.push_str(&input[pos..]);
    out
}

/// Drops the most common inline markdown syntax down to its bare text —
/// `#` heading markers, `**`/`__` bold and `*`/`_` italic markers,
/// backtick code fences/spans, and `[text](url)` links (kept as just
/// `text`) — for a plain-text context with no markdown renderer.
fn strip_markdown_syntax(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let line = line.trim_start_matches('#').trim_start_matches(' ');
        let mut i = 0usize;
        let bytes = line.as_bytes();
        while i < line.len() {
            if bytes[i] == b'[' {
                // Absolute byte positions throughout, rather than offsets
                // relative to `rest`/`after`, so there's exactly one place
                // that can be off by one, not several compounding ones.
                let bracket_close = line[i + 1..].find(']').map(|rel| i + 1 + rel);
                if let Some(bracket_close) = bracket_close {
                    let link_text = &line[i + 1..bracket_close];
                    let after_bracket = bracket_close + 1;
                    if line[after_bracket..].starts_with('(') {
                        let paren_open = after_bracket;
                        let paren_close = line[paren_open + 1..].find(')').map(|rel| paren_open + 1 + rel);
                        if let Some(paren_close) = paren_close {
                            out.push_str(link_text);
                            i = paren_close + 1;
                            continue;
                        }
                    }
                }
            }
            match bytes[i] {
                b'*' | b'_' | b'`' => i += 1,
                _ => {
                    let ch = line[i..].chars().next().expect("i < line.len()");
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        out.push('\n');
    }
    out
}

/// Flattens a PR/Marginalia review comment body to plain text for a context
/// with no markdown renderer at all (the TUI's one-line comment list, which
/// — unlike the GUI's Marginalia view — just prints the string): collapses
/// `<details>` blocks to their summary the same way `clean_comment_markdown`
/// does, then strips HTML comments and common markdown syntax, then
/// collapses all whitespace (including newlines) down to single spaces so
/// the result is safe to show on one line.
pub fn plain_text(body: &str) -> String {
    let collapsed = collapse_details(body);
    let without_comments = strip_html_comments(&collapsed);
    let without_markdown = strip_markdown_syntax(&without_comments);
    without_markdown.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{clean_comment_markdown, plain_text};

    #[test]
    fn plain_text_strips_an_html_comment() {
        let out = plain_text("cursor: <!-- CURSOR_AUTOMATION_ID: aafacb18-a2dd-11f1-b532-320a589b -->linked the ticket");
        assert!(!out.contains("<!--"), "{out}");
        assert!(!out.contains("CURSOR_AUTOMATION_ID"), "{out}");
        assert_eq!(out, "cursor: linked the ticket");
    }

    #[test]
    fn plain_text_strips_heading_and_bold_markers() {
        let out = plain_text("### Child Application name collides with parent\n**High Severity**");
        assert_eq!(out, "Child Application name collides with parent High Severity");
    }

    #[test]
    fn plain_text_keeps_a_links_visible_text_and_drops_its_url() {
        let out = plain_text("See [MLOPS-1234 Fix the thing](https://linear.app/example/issue/MLOPS-1234/x) for context.");
        assert_eq!(out, "See MLOPS-1234 Fix the thing for context.");
    }

    #[test]
    fn plain_text_collapses_multiline_input_to_one_line() {
        let out = plain_text("line one\n\nline two");
        assert_eq!(out, "line one line two");
    }

    #[test]
    fn plain_text_leaves_an_unterminated_html_comment_untouched() {
        let out = plain_text("before <!-- never closed");
        assert_eq!(out, "before <!-- never closed");
    }

    #[test]
    fn plain_text_still_collapses_a_details_block_like_clean_comment_markdown() {
        let out = plain_text(
            "<details><summary><a href=\"https://linear.app/x\">MLOPS-1234 Some title</a></summary>\n\nhidden body\n</details>",
        );
        assert_eq!(out, "MLOPS-1234 Some title");
    }

    #[test]
    fn collapses_a_details_block_to_its_summary_text() {
        let out = clean_comment_markdown(
            "<details><summary><a href=\"https://linear.app/x\">MLOPS-1234 Some title</a></summary>\n\nhidden body that should not appear\n</details>",
        );
        assert!(out.contains("MLOPS-1234 Some title"));
        assert!(!out.contains("hidden body"));
        assert!(!out.contains("<details"));
        assert!(!out.contains("<summary"));
        assert!(!out.contains("<a href"));
    }

    #[test]
    fn an_unterminated_details_block_is_left_untouched_for_the_markdown_renderer_to_drop() {
        // No closing tag anywhere: leave it byte-for-byte as-is rather than
        // dropping to end of input — the downstream CommonMark parser's own
        // HTML-block rule ends an unterminated block at the next blank
        // line, not at end of document, so real content after it (kept
        // here as real markdown text) survives through that later stage
        // instead of being eaten by this function (Review Focus #3).
        let input = "before\n<details><summary>oops no close\n\n## Why\nreal feedback here";
        assert_eq!(clean_comment_markdown(input), input);
    }

    #[test]
    fn a_details_mention_inside_inline_code_is_left_alone() {
        // "<details" only starts a real HTML block at the start of a line
        // (CommonMark's HTML-block rule) — a mention mid-sentence or inside
        // inline code must not be mistaken for one (code-review finding).
        let input = "Consider wrapping this in a `<details>` element.\n\nAlso fix the typo.";
        assert_eq!(clean_comment_markdown(input), input);
    }

    #[test]
    fn the_regression_example_collapses_details_and_keeps_the_heading() {
        let out = clean_comment_markdown(
            "<!-- linear-linkback -->\n<details><summary><a href=\"https://linear.app/example/issue/MLOPS-1234/x\">MLOPS-1234 Fix the thing</a></summary>\n\nlinked body\n</details>\n\n## Why\nThe actual review comment.",
        );
        // The leading HTML comment is deliberately left alone here — it's a
        // single-line HTML block that the downstream markdown renderer
        // already drops cleanly on its own (see this module's doc comment).
        assert!(!out.contains("<details"));
        assert!(!out.contains("<a href"));
        assert!(out.contains("MLOPS-1234 Fix the thing"));
        assert!(out.contains("## Why"));
        assert!(out.contains("The actual review comment."));
    }

    #[test]
    fn ordinary_markdown_with_no_html_passes_through_unchanged() {
        assert_eq!(clean_comment_markdown("## Why\n- a\n- b\n"), "## Why\n- a\n- b\n");
    }
}
