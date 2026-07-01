use std::collections::HashMap;
use crate::types::SessionId;

/// Per-session enrichment state tracked across poll cycles.
/// Used to detect transitions (was passing, now failing → send notification).
#[derive(Debug, Default, Clone)]
pub struct EnrichmentState {
    /// Number of failing checks seen last cycle. None = first cycle.
    pub prev_failing: Option<u32>,
    /// IDs of review comments already seen (to avoid re-dispatching).
    pub seen_comment_ids: std::collections::HashSet<i64>,
    /// Whether a reaction has already been sent for current CI failure set.
    pub ci_reaction_sent: bool,
    /// Whether a reaction has already been sent for current review set.
    pub review_reaction_sent: bool,
}

pub type EnrichmentCache = HashMap<SessionId, EnrichmentState>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_comments_detected_correctly() {
        let mut state = EnrichmentState::default();
        // First cycle: no comments seen yet
        let ids: Vec<i64> = vec![1, 2, 3];
        let new_ids: Vec<i64> = ids.iter()
            .filter(|id| !state.seen_comment_ids.contains(id))
            .copied()
            .collect();
        assert_eq!(new_ids.len(), 3);
        state.seen_comment_ids.extend(ids);

        // Second cycle: add one more
        let ids2: Vec<i64> = vec![1, 2, 3, 4];
        let new_ids2: Vec<i64> = ids2.iter()
            .filter(|id| !state.seen_comment_ids.contains(id))
            .copied()
            .collect();
        assert_eq!(new_ids2, vec![4]);
    }

    #[test]
    fn ci_transition_detected() {
        let mut state = EnrichmentState::default();
        // First cycle: 0 failures
        state.prev_failing = Some(0);
        // Second cycle: 2 failures — this is a new failure
        let is_new_failure = state.prev_failing.map_or(false, |prev| prev == 0) && 2 > 0;
        assert!(is_new_failure);
        state.prev_failing = Some(2);
        // Third cycle: still 2 failures — not a new failure
        let is_new_failure2 = state.prev_failing.map_or(false, |prev| prev == 0) && 2 > 0;
        assert!(!is_new_failure2);
    }
}
