//! Backoff policy for the shared GitHub GraphQL token (5,000 points/hour).
//!
//! `remaining` is read from GitHub's own `rateLimit`, so it reflects every
//! consumer of the token (agents' `gh` calls included), not only Ninox's
//! spend. Ninox stretches its interval as the budget drains and stops
//! polling before it, so agents keep a working budget.

use crate::github_graphql::RateLimitInfo;
use serde::{Deserialize, Serialize};

/// Below this fraction of the limit, polling slows down.
pub const RESERVE_FRACTION: f64 = 0.30;
/// Below this fraction, polling pauses until the window resets.
pub const PAUSE_FRACTION: f64 = 0.10;
/// Largest interval multiplier reached just above the pause threshold.
pub const MAX_STRETCH: f64 = 8.0;
const DEFAULT_LIMIT: u64 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLevel {
    Normal,
    BackingOff,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetPlan {
    pub level:          BudgetLevel,
    /// Multiplier on the configured poll interval (1.0 when healthy).
    pub factor:         f64,
    /// Unix millis to hold off until; set only when `level` is `Paused`.
    pub pause_until_ms: Option<i64>,
}

pub fn plan(rl: &RateLimitInfo, now_ms: i64) -> BudgetPlan {
    let healthy = BudgetPlan { level: BudgetLevel::Normal, factor: 1.0, pause_until_ms: None };
    // No reading yet, or the window already rolled over.
    if rl.reset_at <= now_ms || (rl.limit == 0 && rl.remaining == 0 && rl.cost == 0) {
        return healthy;
    }
    let limit = if rl.limit == 0 { DEFAULT_LIMIT } else { rl.limit };
    let frac = rl.remaining as f64 / limit as f64;
    if frac < PAUSE_FRACTION || rl.remaining < rl.cost.saturating_mul(3) {
        return BudgetPlan {
            level:          BudgetLevel::Paused,
            factor:         1.0,
            pause_until_ms: Some(rl.reset_at + 1_000),
        };
    }
    if frac < RESERVE_FRACTION {
        let depth = (RESERVE_FRACTION - frac) / (RESERVE_FRACTION - PAUSE_FRACTION);
        return BudgetPlan {
            level:          BudgetLevel::BackingOff,
            factor:         1.0 + (MAX_STRETCH - 1.0) * depth,
            pause_until_ms: None,
        };
    }
    healthy
}

/// The latest reading, persisted so the GUI and CLI can show it without
/// calling GitHub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GithubBudget {
    pub limit:         u64,
    pub remaining:     u64,
    pub cost:          u64,
    pub reset_at:      i64,
    pub observed_at:   i64,
    pub level:         BudgetLevel,
    pub interval_secs: u64,
}

impl GithubBudget {
    pub fn summary_now(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.summary(now)
    }

    pub fn summary(&self, now_ms: i64) -> String {
        let mins = ((self.reset_at - now_ms).max(0) + 59_999) / 60_000;
        let limit = if self.limit == 0 { DEFAULT_LIMIT } else { self.limit };
        let state = match self.level {
            BudgetLevel::Normal     => String::new(),
            BudgetLevel::BackingOff => format!(" — backing off, polling every {}s", self.interval_secs),
            BudgetLevel::Paused     => " — polling paused until reset".to_string(),
        };
        format!("GitHub budget: remaining {}/{limit}, resets in {mins}m{state}", self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(remaining: u64, cost: u64) -> RateLimitInfo {
        RateLimitInfo { cost, remaining, limit: 5000, reset_at: 10_000_000 }
    }

    #[test]
    fn healthy_budget_keeps_the_configured_interval() {
        let p = plan(&rl(4000, 40), 0);
        assert_eq!(p.level, BudgetLevel::Normal);
        assert_eq!(p.factor, 1.0);
    }

    #[test]
    fn below_reserve_stretches_the_interval_proportionally() {
        let at_25 = plan(&rl(1250, 40), 0);
        let at_15 = plan(&rl(750, 40), 0);
        assert_eq!(at_25.level, BudgetLevel::BackingOff);
        assert!(at_25.factor > 1.0 && at_15.factor > at_25.factor);
        assert!(at_15.factor < MAX_STRETCH);
    }

    #[test]
    fn below_ten_percent_pauses_until_reset() {
        let p = plan(&rl(400, 40), 0);
        assert_eq!(p.level, BudgetLevel::Paused);
        assert_eq!(p.pause_until_ms, Some(10_001_000));
    }

    #[test]
    fn remaining_under_three_ticks_of_cost_pauses_even_when_percentage_is_fine() {
        let big_limit = RateLimitInfo { cost: 200, remaining: 500, limit: 100_000, reset_at: 10_000_000 };
        assert_eq!(plan(&big_limit, 0).level, BudgetLevel::Paused);
    }

    #[test]
    fn an_elapsed_window_is_healthy_again() {
        assert_eq!(plan(&rl(10, 40), 10_000_001).level, BudgetLevel::Normal);
    }

    #[test]
    fn summary_reports_remaining_and_minutes_to_reset() {
        let b = GithubBudget {
            limit: 5000, remaining: 1234, cost: 40, reset_at: 600_000, observed_at: 0,
            level: BudgetLevel::Normal, interval_secs: 60,
        };
        assert_eq!(b.summary(0), "GitHub budget: remaining 1234/5000, resets in 10m");
    }
}
