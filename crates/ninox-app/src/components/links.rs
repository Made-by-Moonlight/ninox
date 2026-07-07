//! Clickable-link detection for terminal rows: OSC 8 hyperlinks emitted by
//! the running program, plus a fallback scan for bare `http(s)://` URLs in
//! plain text (most CLI tools never bother emitting OSC 8).

/// One clickable span within a single terminal row.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSpan {
    pub start_col: usize,
    pub end_col: usize, // inclusive
    pub url: String,
}

/// One cell's rendered character plus its OSC 8 hyperlink URI, if any. The
/// minimal view `find_links` needs — both the live alacritty grid and the
/// cached tmux scrollback can build a row of these without either depending
/// on the other's cell type.
#[derive(Clone, Copy)]
pub struct LinkCell<'a> {
    pub c: char,
    pub hyperlink: Option<&'a str>,
}

/// Find every clickable span in one row: contiguous same-URI OSC 8 runs
/// first, then a fallback scan for bare `http(s)://` URLs over the
/// remaining text.
pub fn find_links(row: &[LinkCell]) -> Vec<LinkSpan> {
    let mut spans = Vec::new();
    let mut col = 0;
    while col < row.len() {
        if let Some(uri) = row[col].hyperlink {
            let start = col;
            while col < row.len() && row[col].hyperlink == Some(uri) {
                col += 1;
            }
            spans.push(LinkSpan { start_col: start, end_col: col - 1, url: uri.to_string() });
        } else {
            col += 1;
        }
    }

    let text: String = row.iter().map(|cell| if cell.c == '\0' { ' ' } else { cell.c }).collect();
    for (start_col, url) in find_bare_urls(&text) {
        let end_col = start_col + url.chars().count() - 1;
        let overlaps = spans.iter().any(|s| start_col <= s.end_col && end_col >= s.start_col);
        if !overlaps {
            spans.push(LinkSpan { start_col, end_col, url });
        }
    }

    spans
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// Scan plain text for bare `http://`/`https://` URLs, trimming trailing
/// punctuation that's almost always sentence/bracket noise rather than part
/// of the link itself (e.g. a URL at the end of a sentence followed by '.').
fn find_bare_urls(text: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        let scheme_len = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            0
        };
        if scheme_len == 0 {
            i += 1;
            continue;
        }
        let mut end = i + scheme_len;
        while end < chars.len() && is_url_char(chars[end]) {
            end += 1;
        }
        while end > i + scheme_len
            && matches!(
                chars[end - 1],
                '.' | ',' | ')' | ']' | '>' | '"' | '\'' | ';' | ':' | '!' | '?'
            )
        {
            end -= 1;
        }
        if end > i + scheme_len {
            out.push((i, chars[i..end].iter().collect()));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

pub fn link_at(row: &[LinkCell], col: usize) -> Option<String> {
    find_links(row).into_iter().find(|s| col >= s.start_col && col <= s.end_col).map(|s| s.url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_from(text: &str) -> Vec<LinkCell<'static>> {
        text.chars().map(|c| LinkCell { c, hyperlink: None }).collect()
    }

    #[test]
    fn finds_bare_url_in_plain_text() {
        let row = row_from("see http://example.com/path for docs");
        let spans = find_links(&row);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "http://example.com/path");
        assert_eq!(spans[0].start_col, 4);
        assert_eq!(spans[0].end_col, 4 + "http://example.com/path".len() - 1);
    }

    #[test]
    fn trims_trailing_sentence_punctuation() {
        let row = row_from("visit https://example.com/x.");
        let spans = find_links(&row);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/x");
    }

    #[test]
    fn no_url_in_plain_text_returns_no_spans() {
        let row = row_from("nothing clickable here");
        assert!(find_links(&row).is_empty());
    }

    #[test]
    fn finds_osc8_hyperlink_span() {
        let mut row = row_from("click me");
        for cell in &mut row {
            cell.hyperlink = Some("http://example.com");
        }
        let spans = find_links(&row);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "http://example.com");
        assert_eq!(spans[0].start_col, 0);
        assert_eq!(spans[0].end_col, 7);
    }

    #[test]
    fn osc8_span_does_not_duplicate_as_bare_url() {
        let mut row = row_from("http://example.com");
        for cell in &mut row {
            cell.hyperlink = Some("http://example.com");
        }
        assert_eq!(find_links(&row).len(), 1);
    }

    #[test]
    fn link_at_finds_url_under_column() {
        let row = row_from("see http://example.com here");
        assert_eq!(link_at(&row, 5), Some("http://example.com".to_string()));
        assert_eq!(link_at(&row, 0), None);
    }
}
