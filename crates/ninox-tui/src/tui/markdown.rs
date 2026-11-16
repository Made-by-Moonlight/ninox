//! The TUI's one Markdown renderer (Brain reading pane, Ninox prompt reply
//! panel): headings, paragraphs, lists, quotes, rules, and fenced code in a
//! dim box, pre-wrapped to a width by display columns so nothing ever
//! scrolls horizontally.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::palette::Palette;

/// Drops a leading `---`-fenced YAML frontmatter block.
pub fn strip_frontmatter(body: &str) -> &str {
    if let Some(rest) = body.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + 4..].trim_start_matches(['\n', '-']);
        }
    }
    body
}

pub fn render(body: &str, width: u16, p: &Palette) -> Vec<Line<'static>> {
    let width = (width as usize).max(8);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut code: Option<Vec<String>> = None;
    let mut lines = body.lines().peekable();
    let blank = |out: &mut Vec<Line<'static>>| {
        if out.last().is_some_and(|l| l.width() > 0) {
            out.push(Line::default());
        }
    };
    while let Some(raw) = lines.next() {
        if let Some(buf) = code.as_mut() {
            if raw.trim_start().starts_with("```") {
                let buf = code.take().unwrap_or_default();
                out.extend(code_box(&buf, width, p));
            } else {
                buf.push(raw.replace('\t', "    "));
            }
            continue;
        }
        let trimmed = raw.trim_start();
        let is_block_start = trimmed.is_empty()
            || trimmed.starts_with("```")
            || heading(trimmed).is_some()
            || trimmed.starts_with("> ")
            || list_marker(trimmed).is_some()
            || is_rule(trimmed);
        if !is_block_start {
            para.push(trimmed);
            if lines.peek().is_some() {
                continue;
            }
        }
        if !para.is_empty() {
            out.extend(wrap(inline(&para.join(" "), Style::default(), p), vec![], vec![], width));
            para.clear();
        }
        if is_block_start {
            if trimmed.is_empty() {
                blank(&mut out);
            } else if trimmed.starts_with("```") {
                code = Some(Vec::new());
            } else if is_rule(trimmed) {
                out.push(Line::from(Span::styled("─".repeat(width), Style::default().fg(p.rule))));
            } else if let Some((level, text)) = heading(trimmed) {
                let style = match level {
                    1 => Style::default().fg(p.ink).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    2 => Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                    _ => Style::default().add_modifier(Modifier::BOLD),
                };
                out.extend(wrap(inline(text, style, p), vec![], vec![], width));
            } else if let Some(q) = trimmed.strip_prefix("> ") {
                let style = Style::default().fg(p.ink_2).add_modifier(Modifier::ITALIC);
                let bar = || vec![Span::styled("│ ", Style::default().fg(p.faint))];
                out.extend(wrap(inline(q, style, p), bar(), bar(), width));
            } else if let Some((marker, text)) = list_marker(trimmed) {
                let indent = " ".repeat(raw.len() - trimmed.len());
                let lead = format!("{indent}{marker} ");
                let hang = " ".repeat(display_width(&lead));
                out.extend(wrap(
                    inline(text, Style::default(), p),
                    vec![Span::styled(lead, Style::default().fg(p.faint))],
                    vec![Span::raw(hang)],
                    width,
                ));
            }
        }
    }
    if let Some(buf) = code {
        out.extend(code_box(&buf, width, p));
    }
    while out.last().is_some_and(|l| l.width() == 0) {
        out.pop();
    }
    out
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    line[level..].strip_prefix(' ').map(|t| (level, t.trim()))
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

/// `- x`, `* x`, `+ x` → `•`; `12. x` / `12) x` keep their number.
fn list_marker(line: &str) -> Option<(String, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return Some(("•".to_string(), rest));
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &line[digits..];
    let sep = rest.chars().next().filter(|c| *c == '.' || *c == ')')?;
    rest[1..].strip_prefix(' ').map(|text| (format!("{}{sep}", &line[..digits]), text))
}

fn code_box(lines: &[String], width: usize, p: &Palette) -> Vec<Line<'static>> {
    let border = Style::default().fg(p.faint);
    let body = Style::default().fg(p.ink_2);
    let inner = width.saturating_sub(4).max(1);
    let mut out = vec![Line::from(Span::styled(format!("╭{}╮", "─".repeat(inner + 2)), border))];
    for line in lines {
        for chunk in hard_split(line, inner) {
            let pad = inner.saturating_sub(display_width(&chunk));
            out.push(Line::from(vec![
                Span::styled("│ ", border),
                Span::styled(format!("{chunk}{}", " ".repeat(pad)), body),
                Span::styled(" │", border),
            ]));
        }
    }
    out.push(Line::from(Span::styled(format!("╰{}╯", "─".repeat(inner + 2)), border)));
    out
}

/// Splits `line` into chunks no wider than `width`. Widths are measured on
/// the string so far, not per char, matching ratatui: an emoji plus U+FE0F
/// is two columns although each char alone measures one.
fn hard_split(line: &str, width: usize) -> Vec<String> {
    let mut chunks = vec![String::new()];
    for c in line.chars() {
        let cur = chunks.last_mut().expect("never empty");
        cur.push(c);
        if cur.width() > width && cur.chars().count() > 1 {
            cur.pop();
            chunks.push(c.to_string());
        }
    }
    chunks
}

fn display_width(s: &str) -> usize {
    s.width()
}

/// Plain (non-Markdown) text in one style, word-wrapped to `width`.
pub fn wrap_plain(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let chars = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().map(|c| (c, style)).collect();
    wrap(chars, vec![], vec![], (width as usize).max(1))
}

/// Inline Markdown to styled characters: `code`, **bold**, *italic*,
/// [text](url) and [[wikilinks]].
fn inline(text: &str, base: Style, p: &Palette) -> Vec<(char, Style)> {
    let link = Style::default().fg(p.accent).add_modifier(Modifier::UNDERLINED);
    let mut out = Vec::new();
    let (mut bold, mut italic) = (false, false);
    let style = |bold: bool, italic: bool| {
        let mut s = base;
        if bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let rest = &chars[i..];
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                let s = style(bold, italic).fg(p.ink_2);
                out.extend(chars[i + 1..i + 1 + end].iter().map(|&c| (c, s)));
                i += end + 2;
                continue;
            }
        } else if rest.starts_with(&['*', '*']) {
            bold = !bold;
            i += 2;
            continue;
        } else if rest.starts_with(&['[', '[']) {
            if let Some(end) = find(rest, &[']', ']']) {
                let target = &rest[2..end];
                let shown = match target.iter().position(|&c| c == '|') {
                    Some(bar) => &target[bar + 1..],
                    None => target,
                };
                out.extend(shown.iter().map(|&c| (c, link)));
                i += end + 2;
                continue;
            }
        } else if c == '[' {
            if let Some((label, consumed)) = md_link(rest) {
                out.extend(label.iter().map(|&c| (c, link)));
                i += consumed;
                continue;
            }
        } else if c == '*' && is_italic_toggle(&chars, i, italic) {
            italic = !italic;
            i += 1;
            continue;
        }
        out.push((c, style(bold, italic)));
        i += 1;
    }
    out
}

/// A lone `*` opens italic before a word character and closes it after one,
/// so `a * b` stays literal. `_` never toggles: identifiers like `__init__`
/// and `snake_case` are far commoner in agent answers than `_emphasis_`.
fn is_italic_toggle(chars: &[char], i: usize, open: bool) -> bool {
    let prev = i.checked_sub(1).map(|j| chars[j]);
    let next = chars.get(i + 1).copied();
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric());
    if open {
        word(prev) || prev.is_some_and(|c| c.is_ascii_punctuation())
    } else {
        word(next) && !word(prev)
    }
}

fn find(haystack: &[char], needle: &[char]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `[label](url)` at the start of `s`: the label and how many chars it spans.
fn md_link(s: &[char]) -> Option<(&[char], usize)> {
    let close = find(s, &[']', '('])?;
    let label = &s[1..close];
    if label.contains(&'[') {
        return None;
    }
    let end = s[close + 2..].iter().position(|&c| c == ')')? + close + 2;
    Some((label, end + 1))
}

/// Greedy word wrap of styled characters at `width` display columns;
/// `first`/`rest` prefix the first and continuation lines, and a word wider
/// than a whole line is split across lines.
fn wrap(text: Vec<(char, Style)>, first: Vec<Span<'static>>, rest: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut words: Vec<Vec<(char, Style)>> = Vec::new();
    let mut cur = Vec::new();
    for (c, s) in text {
        if c == ' ' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push((c, s));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let prefix_w = |spans: &[Span]| spans.iter().map(|s| s.width()).sum::<usize>();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut prefix = first.clone();
    let mut avail = width.saturating_sub(prefix_w(&first)).max(1);
    let mut used = 0;
    let flush = |line: &mut Vec<(char, Style)>, prefix: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>| {
        let mut spans = std::mem::replace(prefix, rest.clone());
        spans.extend(merge(std::mem::take(line)));
        lines.push(Line::from(spans));
    };
    let text_width = |chars: &[(char, Style)]| chars.iter().map(|(c, _)| *c).collect::<String>().width();
    for word in words {
        let w = text_width(&word);
        if used > 0 && used + 1 + w > avail {
            flush(&mut line, &mut prefix, &mut lines);
            avail = width.saturating_sub(prefix_w(&rest)).max(1);
            used = 0;
        }
        if used > 0 {
            line.push((' ', Style::default()));
            used += 1;
        }
        if used + w <= avail {
            line.extend(word);
            used += w;
            continue;
        }
        let mut chunk: Vec<(char, Style)> = Vec::new();
        for (c, s) in word {
            chunk.push((c, s));
            if used + text_width(&chunk) > avail && chunk.len() > 1 {
                chunk.pop();
                line.append(&mut chunk);
                flush(&mut line, &mut prefix, &mut lines);
                avail = width.saturating_sub(prefix_w(&rest)).max(1);
                used = 0;
                chunk.push((c, s));
            }
        }
        used += text_width(&chunk);
        line.append(&mut chunk);
    }
    if !line.is_empty() || lines.is_empty() {
        flush(&mut line, &mut prefix, &mut lines);
    }
    lines
}

fn merge(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut style = None;
    for (c, s) in chars {
        if style.is_some_and(|cur| cur != s) {
            spans.push(Span::styled(std::mem::take(&mut buf), style.unwrap_or_default()));
        }
        style = Some(s);
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style.unwrap_or_default()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANSWER: &str = "## Plan\n\nBellhop's dashboard work is spread over **two** sessions; I'd split the migration into one orchestrator with three workers, each owning a slice of the `resources` API port so they can land independently.\n\n1. Map every panel's query against the old API, including the long-tail admin pages nobody has opened since the last reorg.\n2. Port the panels.\n- a bullet with [a link](https://example.com) and [[wiki-entry]]\n\n```rust\nfn main() { println!(\"a very long line of code that will certainly not fit inside a narrow terminal column at all, not even close\"); }\n```\n\nThat's it.";

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn rendered(width: u16) -> Vec<String> {
        render(ANSWER, width, &Palette::default()).iter().map(text).collect()
    }

    #[test]
    fn nothing_exceeds_the_width_at_120_and_200_columns() {
        for width in [120u16, 200, 40] {
            for line in render(ANSWER, width, &Palette::default()) {
                assert!(line.width() <= width as usize, "{width}: {} is {} wide", text(&line), line.width());
            }
        }
    }

    #[test]
    fn paragraphs_wrap_at_word_boundaries() {
        let lines = rendered(120);
        let para: Vec<&String> = lines.iter().filter(|l| l.contains("Bellhop") || l.contains("so they can land")).collect();
        assert_eq!(para.len(), 2, "{lines:#?}");
        assert!(para.iter().all(|l| !l.ends_with(' ') && !l.starts_with(' ')));
        let wide = rendered(200);
        assert!(wide.iter().any(|l| l.contains("Bellhop") && l.contains("so they can land")), "fits on one 200-col line");
    }

    #[test]
    fn list_items_hang_under_their_marker() {
        let lines = rendered(60);
        let i = lines.iter().position(|l| l.starts_with("1. Map")).expect("numbered item");
        assert!(lines[i + 1].starts_with("   ") && !lines[i + 1].starts_with("    "), "{:?}", lines[i + 1]);
        assert!(lines.iter().any(|l| l.starts_with("• a bullet with a link and wiki-entry")));
    }

    #[test]
    fn code_sits_in_a_closed_box_that_wraps_long_lines() {
        for width in [120u16, 200] {
            let lines = rendered(width);
            let top = lines.iter().position(|l| l.starts_with('╭')).expect("box top");
            let bottom = lines.iter().position(|l| l.starts_with('╰')).expect("box bottom");
            assert!(bottom > top + 1);
            for l in &lines[top..=bottom] {
                assert_eq!(display_width(l), width as usize, "box rows span the width: {l:?}");
            }
            for l in &lines[top + 1..bottom] {
                assert!(l.starts_with("│ ") && l.ends_with(" │"));
            }
        }
        let narrow = rendered(120);
        let top = narrow.iter().position(|l| l.starts_with('╭')).unwrap();
        assert_eq!(narrow.iter().position(|l| l.starts_with('╰')).unwrap(), top + 3, "the long line wraps to two rows at 120");
    }

    #[test]
    fn inline_markup_is_styled_not_shown() {
        let lines = render(ANSWER, 200, &Palette::default());
        let all: String = lines.iter().map(text).collect::<Vec<_>>().join("\n");
        assert!(!all.contains("**") && !all.contains("`resources`") && !all.contains("](") && !all.contains("[["));
        let bold = lines.iter().flat_map(|l| l.spans.iter()).find(|s| s.content == "two").expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(rendered(200)[0] == "Plan");
    }

    #[test]
    fn literal_asterisks_and_underscores_survive() {
        let lines = render("a * b and snake_case_name and __init__", 80, &Palette::default());
        assert_eq!(text(&lines[0]), "a * b and snake_case_name and __init__");
    }

    #[test]
    fn hash_lines_that_are_not_headings_are_kept() {
        let lines: Vec<String> = render("#1099 is blocked\n#hashtag here\n# Real", 80, &Palette::default()).iter().map(text).collect();
        assert_eq!(lines, vec!["#1099 is blocked #hashtag here", "Real"]);
    }

    #[test]
    fn emoji_presentation_sequences_never_overflow() {
        let body = format!("{}\n\n```\n{}\n```", "⚠️ ".repeat(40), "✔️".repeat(40));
        for width in [20u16, 33] {
            for line in render(&body, width, &Palette::default()) {
                assert!(line.width() <= width as usize, "{width}: {} is {} wide", text(&line), line.width());
            }
        }
    }

    #[test]
    fn long_paragraphs_render_in_linear_time() {
        let body = "word **bold** `code` [link](u) ".repeat(4000);
        let started = std::time::Instant::now();
        render(&body, 120, &Palette::default());
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn frontmatter_is_stripped_only_on_request() {
        let body = "---\nname: x\n---\n# Title\nbody";
        assert_eq!(strip_frontmatter(body), "# Title\nbody");
        assert_eq!(strip_frontmatter("no front"), "no front");
    }
}
