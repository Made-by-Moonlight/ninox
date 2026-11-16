//! Progress of a session view's terminal attach: what the "Terminal
//! connecting…" placeholder reports, when a failed attach is retried, and
//! when it gives up and offers Reconnect instead.

use std::time::{Duration, Instant};

/// Budget for finding the pane and its attach command, and for the attach
/// child's first output before it is killed and retried, so a busy or
/// stalled ptyd host can't hold the view at "connecting" for the sum of its
/// per-request timeouts (15 s each).
pub const PREPARE_BUDGET: Duration = Duration::from_secs(8);
/// An attach that dies sooner than this after spawning counts against the
/// retry budget instead of resetting it, so a flapping host can't loop.
pub const FAST_DEATH: Duration = Duration::from_secs(10);
/// A remote attach goes through ssh, whose cold connect can stay silent far
/// longer than a local ptyd request.
const REMOTE_BUDGET: Duration = Duration::from_secs(30);
pub const MAX_ATTEMPTS: u32 = 5;

pub fn budget(remote: bool) -> Duration {
    if remote { REMOTE_BUDGET } else { PREPARE_BUDGET }
}
const TAIL_BYTES: usize = 512;

/// Delay before retry number `attempt` (1-based): 0.5, 1, 2, 4, 8 s.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.saturating_sub(1).min(4))
}

pub struct AttachProgress {
    /// Identifies the current attempt; a message carrying another token is
    /// from one that was superseded and is ignored.
    pub token:      u64,
    /// Failed attempts since the last one that stayed up.
    pub attempt:    u32,
    /// Set once retries are exhausted; the view shows it with a Reconnect.
    pub failure:    Option<String>,
    pub spawned_at: Option<Instant>,
    pub got_output: bool,
    tail:           Vec<u8>,
    /// The in-flight prepare or the pending retry timer, aborted when this
    /// attempt is replaced or the view is left.
    pending:        Option<iced::task::Handle>,
}

impl AttachProgress {
    pub fn new(token: u64, attempt: u32, pending: Option<iced::task::Handle>) -> Self {
        Self { token, attempt, failure: None, spawned_at: None, got_output: false, tail: Vec::new(), pending }
    }

    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn cancel(&mut self) {
        if let Some(h) = self.pending.take() {
            h.abort();
        }
    }

    pub fn spawned(&mut self) {
        self.cancel();
        self.spawned_at = Some(Instant::now());
        self.got_output = false;
        self.failure = None;
        self.tail.clear();
    }

    pub fn push_output(&mut self, bytes: &[u8]) {
        self.got_output = true;
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > TAIL_BYTES {
            let cut = self.tail.len() - TAIL_BYTES;
            self.tail.drain(..cut);
        }
    }

    /// The last printable line the attach child wrote — its own error
    /// message when it dies (e.g. "Error: ninox-ptyd request timed out").
    pub fn last_line(&self) -> Option<String> {
        last_line(&self.tail)
    }

    /// Whether a child that died now counts as a failed attempt.
    pub fn died_fast(&self) -> bool {
        !self.got_output || self.spawned_at.is_none_or(|t| t.elapsed() < FAST_DEATH)
    }
}

impl Drop for AttachProgress {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn last_line(bytes: &[u8]) -> Option<String> {
    let mut plain = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1;
        } else {
            plain.push(bytes[i] as char);
            i += 1;
        }
    }
    plain.lines().map(str::trim).rfind(|l| !l.is_empty() && l.chars().all(|c| !c.is_control())).map(str::to_string)
}

#[derive(Debug, Clone)]
pub enum Prepared {
    Ready(Vec<String>),
    /// The pane is gone; the session status already says why.
    Gone,
    Unavailable(String),
}

/// The text the terminal pane shows while it has no terminal to draw.
pub fn placeholder(progress: Option<&AttachProgress>) -> String {
    match progress {
        Some(p) if p.attempt > 0 => format!("Terminal connecting… (retry {} of {MAX_ATTEMPTS})", p.attempt),
        _ => "Terminal connecting…".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_a_cap() {
        let secs: Vec<f32> = (1..=7).map(|a| backoff(a).as_secs_f32()).collect();
        assert_eq!(secs, [0.5, 1.0, 2.0, 4.0, 8.0, 8.0, 8.0]);
    }

    #[test]
    fn last_line_is_the_childs_error_without_escapes() {
        let out = b"\x1b[?2026h\x1b[0mError: ninox-ptyd request timed out after 15s\r\n";
        assert_eq!(last_line(out).as_deref(), Some("Error: ninox-ptyd request timed out after 15s"));
        assert_eq!(last_line(b"\x1b[2J"), None);
    }

    #[test]
    fn a_child_that_never_printed_died_fast() {
        let mut p = AttachProgress::new(1, 0, None);
        assert!(p.died_fast());
        p.spawned_at = Some(Instant::now());
        p.push_output(b"x");
        assert!(p.died_fast());
        p.spawned_at = Some(Instant::now() - FAST_DEATH * 2);
        assert!(!p.died_fast());
    }
}
