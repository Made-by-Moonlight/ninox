//! Text transforms applied before `iced::widget::markdown::parse`.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Every pre-pass, in the order the reading pane and reply drawer need.
pub fn prepare(markdown: &str) -> String {
    task_checkboxes(&bold_headings(markdown))
}

/// `- [ ]` / `- [x]` → `- ☐` / `- ☑`. iced 0.13's markdown parser doesn't
/// enable task lists, so the markers would otherwise render as literal
/// brackets. Inter lacks both glyphs; the markdown view's Advanced shaping
/// falls back to the bundled symbol font for them.
pub fn task_checkboxes(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut last = 0;
    for (event, range) in Parser::new_ext(markdown, Options::ENABLE_TASKLISTS).into_offset_iter() {
        if let Event::TaskListMarker(checked) = event {
            out.push_str(&markdown[last..range.start]);
            out.push(if checked { '☑' } else { '☐' });
            last = range.end;
        }
    }
    out.push_str(&markdown[last..]);
    out
}

/// Wraps the text of every ATX h1–h3 in `**…**`. iced 0.13's markdown view
/// only sizes headings and never sets their weight, but it does render
/// strong spans in bold. Uses pulldown-cmark's source offsets, so headings
/// inside code, HTML blocks and setext headings are never touched.
pub fn bold_headings(markdown: &str) -> String {
    let mut inserts: Vec<(usize, usize)> = Vec::new();
    let mut heading: Option<Option<(usize, usize)>> = None;
    for (event, range) in Parser::new(markdown).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let atx = markdown[range.clone()].trim_start().starts_with('#');
                let shallow = matches!(level, HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3);
                heading = (atx && shallow).then_some(None);
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(Some((from, to))) = heading.take() {
                    if wrappable(&markdown[from..to]) {
                        inserts.push((from, to));
                    }
                }
            }
            _ => {
                if let Some(content) = heading.as_mut() {
                    *content = Some(match *content {
                        Some((from, to)) => (from.min(range.start), to.max(range.end)),
                        None => (range.start, range.end),
                    });
                }
            }
        }
    }

    let mut out = String::with_capacity(markdown.len() + inserts.len() * 4);
    let mut last = 0;
    for (from, to) in inserts {
        out.push_str(&markdown[last..from]);
        out.push_str("**");
        out.push_str(&markdown[from..to]);
        out.push_str("**");
        last = to;
    }
    out.push_str(&markdown[last..]);
    out
}

/// Delimiters next to `*`, `_` or a trailing `\` would merge with or be
/// escaped by the heading's own markup, so those headings stay as written.
fn wrappable(content: &str) -> bool {
    !content.is_empty()
        && !content.starts_with(['*', '_'])
        && !content.ends_with(['*', '_', '\\'])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether every text event inside each heading sits inside a strong span.
    fn headings_fully_strong(md: &str) -> Vec<(HeadingLevel, bool)> {
        let mut out = Vec::new();
        let mut current: Option<(HeadingLevel, bool)> = None;
        let mut strong_depth = 0;
        for event in Parser::new(md) {
            match event {
                Event::Start(Tag::Heading { level, .. }) => current = Some((level, true)),
                Event::End(TagEnd::Heading(_)) => out.extend(current.take()),
                Event::Start(Tag::Strong) => strong_depth += 1,
                Event::End(TagEnd::Strong) => strong_depth -= 1,
                Event::Text(_) | Event::Code(_) => {
                    if let Some((_, all)) = current.as_mut() {
                        *all &= strong_depth > 0;
                    }
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn task_list_markers_become_checkbox_glyphs() {
        let md = "- [ ] open\n- [x] done\n* [X] caps\n\n```\n- [ ] in code\n```\n\nnot [ ] a list\n";
        assert_eq!(
            task_checkboxes(md),
            "- ☐ open\n- ☑ done\n* ☑ caps\n\n```\n- [ ] in code\n```\n\nnot [ ] a list\n"
        );
        assert_eq!(prepare("# Todo\n- [ ] a\n"), "# **Todo**\n- ☐ a\n");
    }

    #[test]
    fn wraps_h1_to_h3_in_strong() {
        let out = bold_headings("# One\n\n## Two\n\n### Three\n\nbody\n");
        assert_eq!(out, "# **One**\n\n## **Two**\n\n### **Three**\n\nbody\n");
        assert_eq!(
            headings_fully_strong(&out),
            vec![(HeadingLevel::H1, true), (HeadingLevel::H2, true), (HeadingLevel::H3, true)]
        );
    }

    #[test]
    fn leaves_h4_and_deeper_alone() {
        assert_eq!(bold_headings("#### Four\n"), "#### Four\n");
    }

    #[test]
    fn leaves_non_headings_alone() {
        for md in ["#hashtag\n", "    # indented code\n", "text # not a heading\n", "#\n", "## ##\n"] {
            assert_eq!(bold_headings(md), md, "{md:?}");
        }
    }

    #[test]
    fn skips_fenced_code_blocks() {
        let md = "```sh\n# a shell comment\n```\n~~~\n## also code\n~~~\n# Real\n";
        assert_eq!(bold_headings(md), "```sh\n# a shell comment\n```\n~~~\n## also code\n~~~\n# **Real**\n");
    }

    #[test]
    fn keeps_closing_hash_run_outside_the_strong_span() {
        assert_eq!(bold_headings("## Title ##\n"), "## **Title** ##\n");
        assert_eq!(bold_headings("# C#\n"), "# **C#**\n");
    }

    #[test]
    fn keeps_line_endings() {
        assert_eq!(bold_headings("  # Indented\r\nx"), "  # **Indented**\r\nx");
    }

    #[test]
    fn leaves_headings_whose_edges_would_break_the_delimiters() {
        for md in ["# **Done**\n", "# **Note:** details\n", "# Foo\\\n", "# a*\n", "# _x_\n"] {
            assert_eq!(bold_headings(md), md, "{md:?}");
        }
    }

    #[test]
    fn skips_code_fenced_inside_list_items_and_html_blocks() {
        let md = "- ```\n  # comment\n  ```\n\n<div>\n# x\n</div>\n";
        assert_eq!(bold_headings(md), md);
    }

    #[test]
    fn skips_setext_headings_and_bolds_quoted_ones() {
        assert_eq!(bold_headings("Title\n=====\n"), "Title\n=====\n");
        assert_eq!(bold_headings("> ## Quoted\n"), "> ## **Quoted**\n");
    }

    #[test]
    fn inline_markup_inside_a_heading_stays_strong() {
        let out = bold_headings("# The `run` fn and **its** [link](https://x.dev)?\n");
        assert_eq!(headings_fully_strong(&out), vec![(HeadingLevel::H1, true)]);
    }
}
