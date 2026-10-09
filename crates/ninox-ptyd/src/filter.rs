//! Strips the terminal queries the host's own emulator already answers from
//! the byte stream forwarded to `Raw` subscribers.
//!
//! Without this, an attached real terminal (or the Iced app's emulator
//! behind `ninox pane attach`) would answer DA/DSR/DECRQM a second time and
//! the application would read the duplicate reply as keyboard input. Only
//! sequences alacritty answers are removed; anything else (OSC colour
//! queries, XTVERSION, …) passes through so the real terminal can answer.

/// Longest CSI sequence buffered while deciding; anything longer is not a
/// query and is flushed verbatim.
const MAX_CSI: usize = 32;

#[derive(Default)]
pub struct QueryFilter {
    state: State,
    /// Bytes of a CSI sequence in progress (including `ESC [`).
    pending: Vec<u8>,
}

#[derive(Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl QueryFilter {
    /// Filters `input`, appending the forwarded bytes to `out`. Incomplete
    /// sequences at the end are held until the next call.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                        self.pending.clear();
                        self.pending.push(b);
                    } else {
                        out.push(b);
                    }
                }
                State::Esc => {
                    if b == b'[' {
                        self.state = State::Csi;
                        self.pending.push(b);
                    } else {
                        out.append(&mut self.pending);
                        if b == 0x1b {
                            self.pending.push(b);
                        } else {
                            out.push(b);
                            self.state = State::Ground;
                        }
                    }
                }
                State::Csi => {
                    self.pending.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        if !is_answered_query(&self.pending[2..]) {
                            out.extend_from_slice(&self.pending);
                        }
                        self.pending.clear();
                        self.state = State::Ground;
                    } else if !(0x20..=0x3f).contains(&b) || self.pending.len() > MAX_CSI {
                        // Not a well-formed CSI (or too long to be a query).
                        out.append(&mut self.pending);
                        self.state = State::Ground;
                    }
                }
            }
        }
    }
}

/// `body` = parameter/intermediate bytes plus the final byte.
fn is_answered_query(body: &[u8]) -> bool {
    let Some((&fin, params)) = body.split_last() else { return false };
    match fin {
        // DA1 `CSI c` / `CSI 0 c`, DA2 `CSI > c` / `CSI > 0 c`.
        b'c' => matches!(params, b"" | b"0" | b">" | b">0"),
        // DSR status / cursor position.
        b'n' => matches!(params, b"5" | b"6"),
        // Kitty keyboard flags query.
        b'u' => params == b"?",
        // DECRQM (ANSI and DEC private modes).
        b'p' => params.ends_with(b"$") && params[..params.len() - 1].iter().all(|c| c.is_ascii_digit() || *c == b'?'),
        // XTWINOPS text-area size in pixels / characters.
        b't' => matches!(params, b"14" | b"18"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = QueryFilter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.filter(c, &mut out);
        }
        out
    }

    #[test]
    fn strips_answered_queries_even_across_chunks() {
        assert_eq!(run(&[b"a\x1b[c b\x1b[6n c\x1b[?u\x1b[?2026$p d"]), b"a b c d");
        assert_eq!(run(&[b"x\x1b", b"[", b">0", b"c", b"y"]), b"xy");
    }

    #[test]
    fn passes_everything_else_through() {
        let s: &[u8] = b"\x1b[1;31mred\x1b[0m\x1b[?2026h\x1b[2J\x1b[H\x1b]11;?\x07\x1bM\x1b\x1b[K\xe2\x9c\x93";
        assert_eq!(run(&[s]), s);
        let split: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(&split), s);
    }
}
