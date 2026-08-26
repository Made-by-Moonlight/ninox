//! Parses markdown into block-level structure (headings, paragraphs, list
//! items, code blocks) with inline formatting (bold/italic/code/links)
//! resolved to plain text + style ranges — no literal `#`/`**`/`` ` ``
//! syntax left in the output, unlike `selectable_markdown`'s line-scanner.
//!
//! This exists because a single `iced::widget::text_editor` can only have
//! one font size for its whole content, so "real" heading sizes require one
//! widget per block (see `plan_panel::plan_pane`) — this module produces the
//! plain-text + style data each of those per-block widgets renders from.
//! Reuses `pulldown_cmark` directly (re-exported transitively via
//! `iced::widget::markdown`) rather than iced's own `markdown::parse`,
//! because inline styling here needs byte-range spans into freshly-built
//! plain text, and checkbox support needs `Options::ENABLE_TASKLISTS`,
//! neither of which `iced::widget::markdown::parse` exposes.

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading(u8),
    Paragraph,
    /// `checked: None` = plain bullet/numbered item, `Some(_)` = GFM task
    /// list item (`- [ ]` / `- [x]`).
    ListItem { ordinal: Option<u64>, checked: Option<bool> },
    Code,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: bool,
    /// Route this span through the app's dingbat font (`style::GLYPH`)
    /// instead of whatever font the surrounding block uses — e.g. a
    /// rendered checkbox glyph (☐/☑), which isn't covered by the bundled
    /// Newsreader/Archivo/Spline Sans Mono fonts. Set by callers that
    /// construct display text with such glyphs (see
    /// `app::App::ensure_plan`'s checkbox-prefix handling), not by this
    /// parser itself.
    pub glyph: bool,
}

pub struct Block {
    pub kind: BlockKind,
    /// Plain display text — markdown control characters already stripped.
    pub text: String,
    pub spans: Vec<(Range<usize>, InlineStyle)>,
    /// Links encountered in this block: `(visible label, destination)`.
    /// Surfaced as clickable chips beneath the block, since `text_editor`
    /// has no click-through-link mechanism (see the design doc's follow-up
    /// note on this trade-off).
    pub links: Vec<(String, String)>,
}

pub fn parse_blocks(markdown: &str) -> Vec<Block> {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH,
    );

    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;
    let mut style = InlineStyle::default();
    let mut link_stack: Vec<(String, usize)> = Vec::new(); // (dest, text-start-offset)
    let mut list_ordinal: Vec<Option<u64>> = Vec::new();
    let mut pending_checked: Option<bool> = None;

    macro_rules! push_text {
        ($block:expr, $text:expr) => {
            let start = $block.text.len();
            $block.text.push_str($text);
            let end = $block.text.len();
            if style.bold || style.italic || style.code || style.link {
                $block.spans.push((start..end, style));
            }
        };
    }

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some(Block {
                    kind: BlockKind::Heading(heading_level_num(level)),
                    text: String::new(),
                    spans: Vec::new(),
                    links: Vec::new(),
                });
            }
            Event::Start(Tag::Paragraph) => {
                // A paragraph directly inside a list item continues that
                // item's block (already started by Tag::Item) rather than
                // opening a new one.
                if current.is_none() {
                    current = Some(Block {
                        kind: BlockKind::Paragraph,
                        text: String::new(),
                        spans: Vec::new(),
                        links: Vec::new(),
                    });
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                current = Some(Block {
                    kind: BlockKind::Code,
                    text: String::new(),
                    spans: Vec::new(),
                    links: Vec::new(),
                });
            }
            Event::Start(Tag::List(start)) => list_ordinal.push(start),
            Event::End(TagEnd::List(_)) => {
                list_ordinal.pop();
            }
            Event::Start(Tag::Item) => {
                let ordinal = list_ordinal.last().copied().flatten();
                if let Some(o) = ordinal {
                    if let Some(slot) = list_ordinal.last_mut() {
                        *slot = Some(o + 1);
                    }
                }
                current = Some(Block {
                    kind: BlockKind::ListItem { ordinal, checked: None },
                    text: String::new(),
                    spans: Vec::new(),
                    links: Vec::new(),
                });
            }
            Event::TaskListMarker(checked) => pending_checked = Some(checked),
            Event::End(TagEnd::Item) => {
                if let Some(block) = current.take() {
                    let block = if let Some(checked) = pending_checked.take() {
                        Block {
                            kind: BlockKind::ListItem {
                                ordinal: match block.kind {
                                    BlockKind::ListItem { ordinal, .. } => ordinal,
                                    _ => None,
                                },
                                checked: Some(checked),
                            },
                            ..block
                        }
                    } else {
                        block
                    };
                    blocks.push(block);
                }
            }
            Event::End(TagEnd::Heading(_)) | Event::End(TagEnd::Paragraph) | Event::End(TagEnd::CodeBlock) => {
                if let Some(block) = current.take() {
                    blocks.push(block);
                }
            }
            Event::Start(Tag::Strong) => style.bold = true,
            Event::End(TagEnd::Strong) => style.bold = false,
            Event::Start(Tag::Emphasis) => style.italic = true,
            Event::End(TagEnd::Emphasis) => style.italic = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                style.link = true;
                let offset = current.as_ref().map(|b| b.text.len()).unwrap_or(0);
                link_stack.push((dest_url.to_string(), offset));
            }
            Event::End(TagEnd::Link) => {
                style.link = false;
                if let (Some(block), Some((dest, start))) = (current.as_mut(), link_stack.pop()) {
                    let label = block.text[start..].to_string();
                    if !label.is_empty() {
                        block.links.push((label, dest));
                    }
                }
            }
            Event::Text(text) => {
                if let Some(block) = current.as_mut() {
                    push_text!(block, text.as_ref());
                }
            }
            Event::Code(text) => {
                if let Some(block) = current.as_mut() {
                    let was_code = style.code;
                    style.code = true;
                    push_text!(block, text.as_ref());
                    style.code = was_code;
                }
            }
            Event::SoftBreak => {
                if let Some(block) = current.as_mut() {
                    block.text.push(' ');
                }
            }
            Event::HardBreak => {
                if let Some(block) = current.as_mut() {
                    block.text.push('\n');
                }
            }
            _ => {}
        }
    }

    blocks
}

fn heading_level_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_and_paragraph_are_separate_blocks_with_stripped_text() {
        let blocks = parse_blocks("# Title\n\nSome text.");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Heading(1));
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(blocks[1].text, "Some text.");
    }

    #[test]
    fn bold_and_italic_produce_spans_over_stripped_text() {
        let blocks = parse_blocks("a **b** c *d* e");
        assert_eq!(blocks[0].text, "a b c d e");
        assert!(blocks[0].spans.iter().any(|(r, s)| &blocks[0].text[r.clone()] == "b" && s.bold));
        assert!(blocks[0].spans.iter().any(|(r, s)| &blocks[0].text[r.clone()] == "d" && s.italic));
    }

    #[test]
    fn inline_code_is_stripped_of_backticks_and_flagged() {
        let blocks = parse_blocks("run `cmd` now");
        assert_eq!(blocks[0].text, "run cmd now");
        assert!(blocks[0].spans.iter().any(|(r, s)| &blocks[0].text[r.clone()] == "cmd" && s.code));
    }

    #[test]
    fn link_is_stripped_to_label_text_and_recorded() {
        let blocks = parse_blocks("[text](http://x)");
        assert_eq!(blocks[0].text, "text");
        assert_eq!(blocks[0].links, vec![("text".to_string(), "http://x".to_string())]);
    }

    #[test]
    fn task_list_items_carry_checked_state() {
        let blocks = parse_blocks("- [x] done\n- [ ] todo\n");
        assert_eq!(
            blocks[0].kind,
            BlockKind::ListItem { ordinal: None, checked: Some(true) }
        );
        assert_eq!(blocks[0].text, "done");
        assert_eq!(
            blocks[1].kind,
            BlockKind::ListItem { ordinal: None, checked: Some(false) }
        );
        assert_eq!(blocks[1].text, "todo");
    }

    #[test]
    fn ordered_list_items_carry_incrementing_ordinals() {
        let blocks = parse_blocks("1. one\n2. two\n");
        assert_eq!(blocks[0].kind, BlockKind::ListItem { ordinal: Some(1), checked: None });
        assert_eq!(blocks[1].kind, BlockKind::ListItem { ordinal: Some(2), checked: None });
    }

    #[test]
    fn code_block_text_has_no_fence_markers() {
        let blocks = parse_blocks("```\nlet x = 1;\n```\n");
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].text, "let x = 1;\n");
    }
}
