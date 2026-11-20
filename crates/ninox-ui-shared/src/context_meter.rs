//! What the context meter shows for a session: the used share of its context
//! window, with the automatic reset's threshold marked and a tint as it nears
//! it. Built from the same reading the trigger uses
//! ([`crate::orchestrator_auto_reset::context_pct`]), so what you see is what
//! it acts on. The GUI and the TUI draw it.

use ninox_core::{
    config::{OrchestratorResetConfig, ResetMode},
    types::Session,
};

use crate::orchestrator_auto_reset::{context_pct, context_tokens_window};

/// How many points below the threshold the meter starts to warn.
pub const NEAR_BAND_PCT: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterTint {
    Normal,
    /// Within [`NEAR_BAND_PCT`] points of the threshold.
    Near,
    /// At or past the threshold: it resets or asks for a reset here.
    Over,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextMeter {
    /// `None` when unknown, including right after a reset until the new
    /// conversation reports.
    pub pct: Option<f64>,
    pub tokens: Option<u64>,
    pub window: Option<u64>,
    /// The reset threshold, only while the reset mode is not Off.
    pub threshold: Option<u8>,
    pub tint: MeterTint,
}

fn tokens_k(n: u64) -> String {
    format!("{}k", (n as f64 / 1000.0).round() as u64)
}

impl ContextMeter {
    pub fn for_session(s: &Session, cfg: &OrchestratorResetConfig) -> Self {
        let pct = context_pct(s);
        // The tokens and window belong to the same source as the percentage:
        // the statusline's figure accounts for the auto-compact buffer, so
        // beside it only the statusline's own tokens and window make sense,
        // not the transcript's count over a table window.
        let (tokens, window) = if s.context_used_pct.is_some() {
            (s.context_total_tokens, s.context_window_size)
        } else {
            context_tokens_window(s)
        };
        let threshold = (cfg.mode != ResetMode::Off).then(|| cfg.threshold());
        let tint = match (pct, threshold) {
            (Some(p), Some(t)) if p >= f64::from(t) => MeterTint::Over,
            (Some(p), Some(t)) if p >= f64::from(t) - NEAR_BAND_PCT => MeterTint::Near,
            _ => MeterTint::Normal,
        };
        Self { pct, tokens, window, threshold, tint }
    }

    /// `44%`, or `—` when unknown.
    pub fn label(&self) -> String {
        self.pct.map_or_else(|| "—".to_string(), |p| format!("{:.0}%", p.clamp(0.0, 100.0)))
    }

    /// The bar's fill, 0.0 to 1.0.
    pub fn fill(&self) -> f32 {
        (self.pct.unwrap_or(0.0).clamp(0.0, 100.0) / 100.0) as f32
    }

    /// Where the threshold tick sits on the bar, 0.0 to 1.0.
    pub fn threshold_at(&self) -> Option<f32> {
        self.threshold.map(|t| f32::from(t) / 100.0)
    }

    /// The hover text: `440k / 1000k tokens · resets at 40%`.
    pub fn detail(&self) -> String {
        let used = match (self.pct, self.tokens, self.window) {
            (None, ..) => "context usage not reported yet".to_string(),
            (Some(_), Some(t), Some(w)) => format!("{} used · {} / {} tokens", self.label(), tokens_k(t), tokens_k(w)),
            (Some(_), _, Some(w)) => format!("{} used · {} window", self.label(), tokens_k(w)),
            (Some(_), ..) => format!("{} used", self.label()),
        };
        match self.threshold {
            Some(t) => format!("{used} · reset at {t}%"),
            None => used,
        }
    }

    /// One line for the terminal: `ctx 44%`, `ctx 44% ▏40` with a threshold.
    pub fn compact(&self) -> String {
        match self.threshold {
            Some(t) => format!("ctx {} ▏{t}", self.label()),
            None => format!("ctx {}", self.label()),
        }
    }

    /// The inspector line: `44% (440k / 1000k) · reset at 40%`.
    pub fn sheet_line(&self) -> String {
        let mut line = self.label();
        if let (Some(t), Some(w)) = (self.tokens, self.window) {
            if self.pct.is_some() {
                line.push_str(&format!(" ({} / {})", tokens_k(t), tokens_k(w)));
            }
        }
        if let Some(t) = self.threshold {
            line.push_str(&format!(" · reset at {t}%"));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reset_from_plan::tests_support::session;

    fn cfg(mode: ResetMode, pct: u8) -> OrchestratorResetConfig {
        OrchestratorResetConfig { auto_at_context_pct: pct, mode }
    }

    fn with(f: impl FnOnce(&mut Session)) -> Session {
        let mut s = session("o", None, "/ws");
        s.model = None;
        f(&mut s);
        s
    }

    #[test]
    fn unknown_reads_as_a_dash_with_no_fill_and_no_tint() {
        let m = ContextMeter::for_session(&with(|_| {}), &cfg(ResetMode::Prompt, 40));
        assert_eq!((m.label().as_str(), m.fill(), m.tint), ("—", 0.0, MeterTint::Normal));
        assert!(m.detail().contains("not reported yet"));
        assert_eq!(m.compact(), "ctx — ▏40");
    }

    #[test]
    fn the_statusline_figure_wins_and_tokens_over_window_is_the_fallback() {
        let reported = with(|s| {
            s.context_used_pct = Some(44.0);
            s.context_total_tokens = Some(440_000);
            s.context_window_size = Some(1_000_000);
        });
        let m = ContextMeter::for_session(&reported, &cfg(ResetMode::Off, 40));
        assert_eq!((m.label().as_str(), m.fill()), ("44%", 0.44));
        assert_eq!(m.detail(), "44% used · 440k / 1000k tokens");

        let fallback = with(|s| {
            s.context_tokens = Some(80_000);
            s.model = Some("claude-opus-5-5".into());
        });
        let m = ContextMeter::for_session(&fallback, &cfg(ResetMode::Off, 40));
        assert_eq!(m.label(), "40%", "same source as the trigger");
    }

    #[test]
    fn tokens_and_window_come_from_the_same_source_as_the_percentage() {
        // A statusline percentage with only the transcript's count and a table window:
        // the two would disagree, so the hover shows just the percentage.
        let mixed = with(|s| {
            s.context_used_pct = Some(44.0);
            s.context_tokens = Some(300_000);
            s.model = Some("claude-opus-5-5".into());
        });
        let m = ContextMeter::for_session(&mixed, &cfg(ResetMode::Off, 40));
        assert_eq!((m.tokens, m.window), (None, None));
        assert_eq!(m.detail(), "44% used");
        assert_eq!(m.sheet_line(), "44%");

        // The statusline's own tokens and window go with it.
        let own = with(|s| {
            s.context_used_pct = Some(44.0);
            s.context_total_tokens = Some(440_000);
            s.context_window_size = Some(1_000_000);
            s.context_tokens = Some(300_000);
        });
        let m = ContextMeter::for_session(&own, &cfg(ResetMode::Off, 40));
        assert_eq!((m.tokens, m.window), (Some(440_000), Some(1_000_000)));

        // With no statusline figure, the percentage is derived from tokens and window, which are shown.
        let derived = with(|s| {
            s.context_tokens = Some(80_000);
            s.model = Some("claude-opus-5-5".into());
        });
        let m = ContextMeter::for_session(&derived, &cfg(ResetMode::Off, 40));
        assert_eq!((m.pct, m.tokens, m.window), (Some(40.0), Some(80_000), Some(200_000)));
    }

    #[test]
    fn the_threshold_is_shown_only_when_the_mode_is_not_off() {
        let s = with(|s| s.context_used_pct = Some(20.0));
        let off = ContextMeter::for_session(&s, &cfg(ResetMode::Off, 40));
        assert_eq!((off.threshold, off.threshold_at()), (None, None));
        assert_eq!(off.compact(), "ctx 20%");
        assert_eq!(off.sheet_line(), "20%");
        for mode in [ResetMode::Prompt, ResetMode::Auto] {
            let on = ContextMeter::for_session(&s, &cfg(mode, 40));
            assert_eq!((on.threshold, on.threshold_at()), (Some(40), Some(0.4)));
            assert_eq!(on.compact(), "ctx 20% ▏40");
            assert_eq!(on.sheet_line(), "20% · reset at 40%");
        }
    }

    #[test]
    fn it_tints_as_it_nears_and_passes_the_threshold_and_never_while_off() {
        let at = |pct: f64, mode| ContextMeter::for_session(&with(|s| s.context_used_pct = Some(pct)), &cfg(mode, 40)).tint;
        assert_eq!(at(29.9, ResetMode::Prompt), MeterTint::Normal);
        assert_eq!(at(30.0, ResetMode::Prompt), MeterTint::Near);
        assert_eq!(at(39.9, ResetMode::Auto), MeterTint::Near);
        assert_eq!(at(40.0, ResetMode::Auto), MeterTint::Over);
        assert_eq!(at(90.0, ResetMode::Prompt), MeterTint::Over);
        assert_eq!(at(90.0, ResetMode::Off), MeterTint::Normal, "no threshold, nothing to approach");
    }

    #[test]
    fn the_threshold_follows_the_clamped_config_value() {
        let s = with(|s| s.context_used_pct = Some(5.0));
        assert_eq!(ContextMeter::for_session(&s, &cfg(ResetMode::Prompt, 0)).threshold, Some(10));
        assert_eq!(ContextMeter::for_session(&s, &cfg(ResetMode::Prompt, 200)).threshold, Some(95));
    }

    #[test]
    fn a_reading_cleared_by_a_reset_reads_as_unknown_again() {
        let mut s = with(|s| s.context_used_pct = Some(55.0));
        assert_eq!(ContextMeter::for_session(&s, &cfg(ResetMode::Prompt, 40)).tint, MeterTint::Over);
        s.context_used_pct = None;
        s.context_tokens = None;
        s.context_total_tokens = None;
        s.context_window_size = None;
        let m = ContextMeter::for_session(&s, &cfg(ResetMode::Prompt, 40));
        assert_eq!((m.pct, m.tint), (None, MeterTint::Normal));
    }
}
