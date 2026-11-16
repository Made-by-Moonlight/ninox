/// Git- and store-derived facts about one worktree, gathered by
/// `gather_git_facts` (or synthesized directly in tests) and consumed by
/// `decide()`. Pure data — no IO lives on this type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceFacts {
    pub dirty: bool,
    pub has_branch: bool,
    pub has_upstream: bool,
    /// Commits on the branch not in its upstream. Meaningless if `!has_upstream`.
    pub unpushed_commit_count: u32,
    /// Commits on the branch not in the repo's base/default ref.
    pub commits_beyond_base: u32,
    /// True only when known-merged (e.g. `Session::merged_at.is_some()`) AND
    /// that merge is for the exact branch these facts describe — never set
    /// this from a session's `merged_at` alone without confirming the
    /// branch still matches, or an earlier task's merge in a long-lived,
    /// branch-switching session will wrongly vouch for a later one's work.
    pub merged: bool,
    /// Commits on this branch newer than the session's `merged_at` stamp.
    /// Meaningless if `!merged`. `merged` only proves that merge's content
    /// landed, not anything committed afterward (kept-alive post-merge
    /// validation, or follow-up work in a long-lived session) — so the
    /// bypass below only fires when this is `0`.
    pub commits_since_merge: u32,
    pub interrupted: bool,
    pub live_session_row: bool,
}

/// Whether this cleanup call is allowed to override the safety rules below.
/// Only ever `true` for an explicit `ninox reap --force`; every automatic
/// path (auto-reap-on-merge, retention sweep, orphan sweep) always passes
/// `false`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CleanupPolicy {
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkipReason {
    LiveSessionRow,
    Interrupted,
    Dirty,
    UnpushedCommits,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CleanupDecision {
    Proceed { delete_branch: bool },
    Skip(SkipReason),
}

/// The one place the brief's safety rules live. `force` gates all three
/// unsafe conditions uniformly (live session row, interrupted, dirty/
/// unpushed) — the same way `reap --force` already overrides them today.
/// Branch deletion is safe either when the branch is known merged, or when
/// it has made no commits beyond the repo's base (never when there's no
/// branch at all).
pub fn decide(facts: &WorkspaceFacts, policy: CleanupPolicy) -> CleanupDecision {
    // A known-merged session's local commits are exactly the ones that just
    // landed on the base branch — the merge itself is the backup, so a
    // squash-merge-then-prune (which can make `@{u}` stop resolving, or
    // leave the branch looking "ahead" of a base ref that doesn't contain
    // the squashed commit) must never be read as unsafe unpushed work.
    // That only holds while nothing has been committed to the branch since
    // the merge landed — `commits_since_merge` is how a caller that reused
    // this branch for later work (or kept it alive for post-merge
    // validation) says so.
    let safely_merged = facts.merged && facts.commits_since_merge == 0;
    if !policy.force {
        if facts.live_session_row {
            return CleanupDecision::Skip(SkipReason::LiveSessionRow);
        }
        if facts.interrupted {
            return CleanupDecision::Skip(SkipReason::Interrupted);
        }
        if facts.dirty {
            return CleanupDecision::Skip(SkipReason::Dirty);
        }
        // `dirty` is unaffected by `safely_merged`: genuinely uncommitted
        // changes were never part of the merge.
        let unsafe_unpushed = !safely_merged && if facts.has_upstream {
            facts.unpushed_commit_count > 0
        } else {
            facts.commits_beyond_base > 0
        };
        if unsafe_unpushed {
            return CleanupDecision::Skip(SkipReason::UnpushedCommits);
        }
    }
    CleanupDecision::Proceed {
        delete_branch: facts.has_branch && (safely_merged || facts.commits_beyond_base == 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(overrides: impl FnOnce(&mut WorkspaceFacts)) -> WorkspaceFacts {
        let mut f = WorkspaceFacts {
            dirty: false,
            has_branch: true,
            has_upstream: true,
            unpushed_commit_count: 0,
            commits_beyond_base: 0,
            merged: false,
            commits_since_merge: 0,
            interrupted: false,
            live_session_row: false,
        };
        overrides(&mut f);
        f
    }

    #[test]
    fn dirty_worktree_is_skipped_without_force() {
        let f = facts(|f| f.dirty = true);
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::Dirty));
    }

    #[test]
    fn dirty_worktree_proceeds_with_force() {
        let f = facts(|f| f.dirty = true);
        assert_eq!(decide(&f, CleanupPolicy { force: true }), CleanupDecision::Proceed { delete_branch: true });
    }

    #[test]
    fn unpushed_commits_with_upstream_is_skipped_without_force() {
        let f = facts(|f| f.unpushed_commit_count = 1);
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::UnpushedCommits));
    }

    #[test]
    fn no_upstream_with_commits_beyond_base_is_skipped_without_force() {
        let f = facts(|f| { f.has_upstream = false; f.commits_beyond_base = 3; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::UnpushedCommits));
    }

    #[test]
    fn no_upstream_and_empty_is_safe_to_proceed() {
        let f = facts(|f| { f.has_upstream = false; f.commits_beyond_base = 0; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Proceed { delete_branch: true });
    }

    #[test]
    fn merged_bypasses_unpushed_commits_check() {
        // A squash-merge-then-prune can leave @{u} unresolvable and/or
        // commits_beyond_base > 0 even though the branch's content is
        // already safely on the base — merged must not be read as unsafe
        // unpushed work (reviewer's Important 11).
        let f = facts(|f| { f.merged = true; f.has_upstream = false; f.commits_beyond_base = 3; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Proceed { delete_branch: true });
    }

    #[test]
    fn merged_with_commits_since_merge_does_not_bypass_unpushed_check() {
        // merged_at only proves THAT merge's content landed — a commit made
        // on the branch afterward (follow-up work, kept-alive validation)
        // was never part of it.
        let f = facts(|f| { f.merged = true; f.has_upstream = false; f.commits_beyond_base = 3; f.commits_since_merge = 1; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::UnpushedCommits));
    }

    #[test]
    fn merged_does_not_bypass_dirty_check() {
        // Genuinely uncommitted changes were never part of the merge.
        let f = facts(|f| { f.merged = true; f.dirty = true; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::Dirty));
    }

    #[test]
    fn interrupted_is_skipped_without_force() {
        let f = facts(|f| f.interrupted = true);
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::Interrupted));
    }

    #[test]
    fn interrupted_proceeds_with_force() {
        let f = facts(|f| f.interrupted = true);
        assert_eq!(decide(&f, CleanupPolicy { force: true }), CleanupDecision::Proceed { delete_branch: true });
    }

    #[test]
    fn live_session_row_is_skipped_without_force() {
        let f = facts(|f| f.live_session_row = true);
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Skip(SkipReason::LiveSessionRow));
    }

    #[test]
    fn live_session_row_proceeds_with_force() {
        let f = facts(|f| f.live_session_row = true);
        assert_eq!(decide(&f, CleanupPolicy { force: true }), CleanupDecision::Proceed { delete_branch: true });
    }

    #[test]
    fn merged_branch_is_deleted() {
        let f = facts(|f| { f.merged = true; f.commits_beyond_base = 5; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Proceed { delete_branch: true });
    }

    #[test]
    fn unmerged_branch_with_commits_beyond_base_is_kept() {
        let f = facts(|f| f.commits_beyond_base = 2);
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Proceed { delete_branch: false });
    }

    #[test]
    fn no_branch_never_deletes() {
        let f = facts(|f| { f.has_branch = false; f.merged = true; f.commits_beyond_base = 0; });
        assert_eq!(decide(&f, CleanupPolicy { force: false }), CleanupDecision::Proceed { delete_branch: false });
    }

    #[test]
    fn idempotent_decision_is_pure() {
        let f = facts(|_| {});
        assert_eq!(decide(&f, CleanupPolicy { force: false }), decide(&f, CleanupPolicy { force: false }));
    }
}
