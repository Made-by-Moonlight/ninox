//! Recognising "this harness isn't logged in" in whatever harness output
//! Ninox sees — a session's pane, a `claude -p` subprocess (brain harvest,
//! auto-planning), a spawn error — so the UIs can tell the user to log in
//! again. Ninox never runs the login itself.
//!
//! One row per harness in [`HARNESS_AUTH`]; a harness without a row is
//! never flagged.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessAuth {
    /// The harness name, as in `[harnesses.<name>]` and `Session::agent_type`.
    pub harness: &'static str,
    /// How the UIs name it: "Claude login expired".
    pub display: &'static str,
    /// What the user runs to log in again.
    pub login:   &'static str,
    /// Lowercase substrings of an auth failure.
    pub needles: &'static [&'static str],
    /// Lowercase prefixes a pane line must start with (after its leading
    /// box-drawing/bullet glyphs) to count. A pane is an agent's whole
    /// conversation, which can quote these errors without having them —
    /// only the harness's own error lines are trusted there.
    pub pane_line_prefixes: &'static [&'static str],
    /// Lowercase text only this harness's own auth errors carry; a pane
    /// line needs one too, so another tool's "Not logged in" or 401 in
    /// the agent's tool output doesn't count.
    pub pane_markers: &'static [&'static str],
}

pub const HARNESS_AUTH: &[HarnessAuth] = &[HarnessAuth {
    harness: "claude-code",
    display: "Claude",
    login:   "claude /login",
    needles: &[
        "not logged in",
        "please run /login",
        "invalid api key",
        "oauth token has expired",
        "authentication_error",
    ],
    pane_line_prefixes: &["api error", "invalid api key", "not logged in", "oauth token has expired"],
    pane_markers: &["please run /login"],
}];

pub fn for_harness(harness: &str) -> Option<&'static HarnessAuth> {
    HARNESS_AUTH.iter().find(|h| h.harness == harness)
}

impl HarnessAuth {
    fn matches(&self, line: &str) -> bool {
        let lower = line.to_lowercase();
        self.needles.iter().any(|n| lower.contains(n))
    }

    /// The first line of subprocess output that reports an auth failure.
    pub fn detect_output(&self, output: &str) -> Option<String> {
        output.lines().find(|l| self.matches(l)).map(|l| l.trim().to_string())
    }

    /// The auth-failure line among the bottom of a pane's screen, where the
    /// harness prints its latest error.
    pub fn detect_on_screen(&self, screen: &str) -> Option<String> {
        let tail: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
        tail.iter().rev().take(PANE_TAIL_LINES).find_map(|line| {
            let body = line.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
            let lower = line.to_lowercase();
            (self.pane_line_prefixes.iter().any(|p| body.starts_with(p))
                && self.matches(line)
                && self.pane_markers.iter().any(|m| lower.contains(m)))
                .then(|| line.trim().to_string())
        })
    }
}

/// Arbitrary: enough for an error block above the input box and footer.
const PANE_TAIL_LINES: usize = 15;

/// Any harness whose auth failure `output` reports — for errors that don't
/// say which harness produced them (a spawn failure).
pub fn detect_any(output: &str) -> Option<(&'static HarnessAuth, String)> {
    HARNESS_AUTH.iter().find_map(|h| h.detect_output(output).map(|line| (h, line)))
}

/// Record a subprocess/spawn auth failure in the store so every UI sees it,
/// whichever process hit it. Best effort.
pub fn record_output_failure(store: &crate::store::Store, harness: &str, source: &str, output: &str) {
    let Some(auth) = for_harness(harness) else { return };
    if let Some(line) = auth.detect_output(output) {
        if let Err(e) = store.record_harness_auth(&format!("{source}:{harness}"), harness, None, &line) {
            tracing::warn!("record harness auth failure: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> &'static HarnessAuth {
        for_harness("claude-code").unwrap()
    }

    #[test]
    fn claude_auth_failures_are_recognised_in_output() {
        for out in [
            "Not logged in · Please run /login",
            "Invalid API key · Please run /login",
            "API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}}",
        ] {
            assert!(claude().detect_output(out).is_some(), "{out}");
        }
        assert_eq!(claude().detect_output("API Error: 529 overloaded"), None);
    }

    #[test]
    fn a_pane_only_counts_the_harness_s_own_error_lines() {
        let quoting = "> the patterns are 'Not logged in' and 'Please run /login'\n│ > \n";
        assert_eq!(claude().detect_on_screen(quoting), None, "a conversation quoting the error isn't one");
        let failing = "● Working on it\n  ⎿  API Error: 401 {\"error\":{\"type\":\"authentication_error\"}} · Please run /login\n\n│ > \n";
        assert!(claude().detect_on_screen(failing).unwrap().contains("authentication_error"));
        assert!(claude().detect_on_screen("  ⎿  Invalid API key · Please run /login\n").is_some());
    }

    #[test]
    fn another_tool_s_auth_error_in_a_pane_is_not_claude_s() {
        for line in [
            "  ⎿  Not logged in. Run `npm login` first.",
            "  ⎿  Invalid API key provided: sk-proj-***",
            "  ⎿  API Error: 401 {\"error\":{\"type\":\"authentication_error\"}} from the vendor SDK",
        ] {
            assert_eq!(claude().detect_on_screen(line), None, "{line}");
        }
    }

    #[test]
    fn only_the_bottom_of_the_screen_is_read() {
        let mut screen = String::from("  ⎿  Invalid API key · Please run /login\n");
        for i in 0..PANE_TAIL_LINES {
            screen.push_str(&format!("line {i}\n"));
        }
        assert_eq!(claude().detect_on_screen(&screen), None, "an error scrolled up has been dealt with");
    }

    #[test]
    fn unknown_harnesses_are_never_flagged() {
        assert!(for_harness("some-other-harness").is_none());
        assert!(detect_any("Not logged in · Please run /login").is_some());
        assert!(detect_any("connection refused").is_none());
    }
}
