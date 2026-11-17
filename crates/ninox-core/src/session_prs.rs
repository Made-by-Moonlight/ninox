//! Every PR a session is responsible for, shared by the GUI, the TUI and
//! `ninox list`. A session row tracks one PR (`pr_number`); this adds the
//! PRs it watches as opener and the extra branch PRs recorded for it.

use std::collections::{BTreeMap, HashMap};

use crate::{
    github::parse_pr_url,
    pr_events::{CiVerdict, PrStatusView},
    store::Store,
    types::{Session, SessionId, PR},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionPr {
    pub repo:              String,
    pub number:            u64,
    pub url:               String,
    pub title:             String,
    pub state:             PrState,
    pub draft:             bool,
    pub ci:                CiVerdict,
    pub passing:           u32,
    pub failing:           u32,
    pub pending:           u32,
    pub changes_requested: bool,
    pub approved:          bool,
    pub tracked:           bool,
}

impl SessionPr {
    /// `repo#number` with the owner dropped: `bellhop#1466`.
    pub fn label(&self) -> String {
        let name = self.repo.rsplit('/').next().unwrap_or(&self.repo);
        if name.is_empty() { format!("#{}", self.number) } else { format!("{name}#{}", self.number) }
    }

    pub fn total(&self) -> u32 {
        self.passing + self.failing + self.pending
    }

    pub fn is_open(&self) -> bool {
        self.state == PrState::Open
    }

    /// Failing CI, by the verdict or by any failing check. Ordering, colours
    /// and summaries all use this one definition.
    pub fn is_failing(&self) -> bool {
        self.ci == CiVerdict::Failed || self.failing > 0
    }

    /// Checks still running, by the verdict or by any pending check.
    pub fn is_pending(&self) -> bool {
        self.ci == CiVerdict::Pending || self.pending > 0
    }

    /// `28/53 checks passing`, or `None` while no checks are known.
    pub fn checks_line(&self) -> Option<String> {
        (self.total() > 0).then(|| format!("{}/{} checks passing", self.passing, self.total()))
    }

    /// `open`, `draft`, `merged` or `closed`, plus the review verdict when there is one.
    pub fn state_line(&self) -> String {
        let base = match (self.state, self.draft) {
            (PrState::Merged, _) => "merged",
            (PrState::Closed, _) => "closed",
            (PrState::Open, true) => "draft",
            (PrState::Open, false) => "open",
        };
        if !self.is_open() {
            return base.to_string();
        }
        if self.changes_requested {
            format!("{base} · changes requested")
        } else if self.approved {
            format!("{base} · approved")
        } else {
            base.to_string()
        }
    }

    /// Lower sorts first: open PRs ahead of merged ahead of closed; within
    /// open ones, the ones that need attention first.
    fn rank(&self) -> (u8, u8) {
        match self.state {
            PrState::Merged => (1, 0),
            PrState::Closed => (2, 0),
            PrState::Open => (0, match (self.is_failing(), self.changes_requested, self.is_pending(), self.draft) {
                (true, _, _, _) => 0,
                (_, true, _, _) => 1,
                (_, _, true, false) => 2,
                (_, _, _, false) => 3,
                (_, _, _, true) => 4,
            }),
        }
    }
}

fn from_view(v: &PrStatusView, tracked: bool) -> SessionPr {
    let (mut passing, mut failing, mut pending) = (0, 0, 0);
    for c in &v.checks {
        if c.status != "completed" {
            pending += 1;
            continue;
        }
        match c.conclusion.as_deref() {
            Some("success" | "neutral" | "skipped") => passing += 1,
            Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure" | "stale") => failing += 1,
            _ => pending += 1,
        }
    }
    let state = if v.merged {
        PrState::Merged
    } else if v.state.eq_ignore_ascii_case("closed") {
        PrState::Closed
    } else {
        PrState::Open
    };
    SessionPr {
        repo: v.repo.clone(),
        number: v.number,
        url: v.url.clone(),
        title: v.title.clone(),
        state,
        draft: v.draft.unwrap_or(false),
        ci: v.ci,
        passing,
        failing,
        pending,
        changes_requested: v.reviews.values().any(|r| r == "CHANGES_REQUESTED"),
        approved: v.reviews.values().any(|r| r == "APPROVED") && !v.reviews.values().any(|r| r == "CHANGES_REQUESTED"),
        tracked,
    }
}

/// A PR with no cached sweep (not fetched yet, never swept, or pruned):
/// open unless the session says otherwise, with no CI known.
fn bare(repo: &str, number: u64, url: Option<&str>, title: &str, tracked: bool, end: Option<PrState>) -> SessionPr {
    SessionPr {
        repo: repo.to_string(),
        number,
        url: url.map(String::from).unwrap_or_else(|| format!("https://github.com/{repo}/pull/{number}")),
        title: title.to_string(),
        state: end.unwrap_or(PrState::Open),
        draft: false,
        ci: CiVerdict::None,
        passing: 0,
        failing: 0,
        pending: 0,
        changes_requested: false,
        approved: false,
        tracked,
    }
}

/// What a session's own state implies for its tracked PR once the sweep
/// cache has lost it. A stamped merge, or a `Done` row that never ran
/// `ninox complete` (the merge → auto-reap path, which clears `merged_at`),
/// means merged; any other `Done` row is no longer open but unknown.
fn session_end(session: &Session) -> Option<PrState> {
    if session.merged_at.is_some() {
        return Some(PrState::Merged);
    }
    (session.status == crate::types::SessionStatus::Done)
        .then(|| if session.completion_outcome.is_none() { PrState::Merged } else { PrState::Closed })
}

/// Sources, keyed by lowercase `(repo, number)` so each PR appears once.
struct Collector<'a> {
    store: &'a Store,
    /// What the session's own state says about its tracked PR when its cache entry is gone.
    session_end: Option<PrState>,
    found: BTreeMap<(String, u64), SessionPr>,
}

impl Collector<'_> {
    fn add(&mut self, repo: &str, number: u64, url: Option<&str>, title: &str, tracked: bool) {
        if repo.is_empty() {
            return;
        }
        let key = (repo.to_lowercase(), number);
        if let Some(existing) = self.found.get_mut(&key) {
            existing.tracked |= tracked;
            return;
        }
        let pr = match self.store.get_pr_status(&key.0, number) {
            Ok(Some(view)) => from_view(&view, tracked),
            _ => bare(repo, number, url, title, tracked, self.session_end.filter(|_| tracked)),
        };
        self.found.insert(key, pr);
    }
}

/// Sort in place: open first, then merged, then closed.
pub fn order(prs: &mut [SessionPr]) {
    prs.sort_by(|a, b| a.rank().cmp(&b.rank()).then(b.number.cmp(&a.number)).then(a.repo.cmp(&b.repo)));
}

/// Every session's PRs, from one read of the watch and PR tables.
pub fn for_sessions<'a>(store: &Store, sessions: impl IntoIterator<Item = &'a Session>) -> HashMap<SessionId, Vec<SessionPr>> {
    let watches = store.list_pr_watches().unwrap_or_default();
    let mut recorded: HashMap<String, Vec<PR>> = HashMap::new();
    for pr in store.list_prs().unwrap_or_default() {
        recorded.entry(pr.session_id.clone()).or_default().push(pr);
    }
    let mut out = HashMap::new();
    for session in sessions {
        let session_end = session_end(session);
        let mut c = Collector { store, session_end, found: BTreeMap::new() };
        // `prs` is keyed by bare PR number, so only trust a row recorded for this session.
        let tracked_row = session.pr_id.and_then(|id| store.get_pr(id).ok().flatten()).filter(|p| p.session_id == session.id);
        if let Some(n) = session.pr_number {
            let title = tracked_row.as_ref().map(|p| p.title.as_str()).unwrap_or("");
            let url = tracked_row.as_ref().map(|p| p.url.as_str());
            c.add(&session.repo, n, url, title, true);
        }
        for w in watches.iter().filter(|w| w.opener_session_id.as_deref() == Some(session.id.as_str())) {
            c.add(&w.repo, w.pr_number, Some(&w.pr_url), "", false);
        }
        for pr in recorded.get(&session.id).into_iter().flatten() {
            if let Some((repo, n)) = parse_pr_url(&pr.url) {
                c.add(&repo, n, Some(&pr.url), &pr.title, false);
            }
        }
        let mut prs: Vec<SessionPr> = c.found.into_values().collect();
        order(&mut prs);
        out.insert(session.id.clone(), prs);
    }
    out
}

pub fn for_session(store: &Store, session: &Session) -> Vec<SessionPr> {
    for_sessions(store, [session]).remove(&session.id).unwrap_or_default()
}

/// The PR a one-line view should name: the first in [`order`], so an open
/// one while any is open.
pub fn lead(prs: &[SessionPr]) -> Option<&SessionPr> {
    prs.first()
}

/// One-line summary for sidebars, cards and lists, `None` without PRs:
/// `PR #7`, `PR bellhop#1466 +3 · 1 failing`, `4 PRs merged`.
pub fn summary(prs: &[SessionPr]) -> Option<String> {
    let lead = lead(prs)?;
    if prs.len() == 1 {
        return Some(format!("PR #{}", lead.number));
    }
    let open = prs.iter().filter(|p| p.is_open()).count();
    if open == 0 {
        let merged = prs.iter().filter(|p| p.state == PrState::Merged).count();
        return Some(if merged == prs.len() {
            format!("{} PRs merged", prs.len())
        } else {
            format!("{} PRs · {merged} merged", prs.len())
        });
    }
    let mut s = format!("PR {} +{}", lead.label(), prs.len() - 1);
    let failing = prs.iter().filter(|p| p.is_open() && p.is_failing()).count();
    if failing > 0 {
        s.push_str(&format!(" · {failing} failing"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pr_events::CheckView, types::{PrWatch, SessionStatus}};

    fn store() -> Store {
        let _ = crate::test_isolation::isolate();
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn session(id: &str, repo: &str, pr: Option<u64>) -> Session {
        let mut s: Session = serde_json::from_value(serde_json::json!({
            "id": id, "orchestrator_id": null, "name": id, "repo": repo, "status": "working",
            "agent_type": "claude-code", "cost_usd": 0.0, "started_at": 0, "pr_number": pr, "pr_id": null,
            "workspace_path": null, "pid": null,
        })).unwrap();
        s.status = SessionStatus::PrOpen;
        s
    }

    fn view(repo: &str, n: u64, state: &str, merged: bool, checks: &[(&str, Option<&str>)]) -> PrStatusView {
        PrStatusView {
            repo: repo.into(), number: n, title: format!("t{n}"), state: state.into(), merged,
            ci: if checks.iter().any(|(_, c)| *c == Some("failure")) { CiVerdict::Failed } else { CiVerdict::Passed },
            checks: checks.iter().map(|(s, c)| CheckView { name: "c".into(), status: (*s).into(), conclusion: c.map(String::from) }).collect(),
            observed_at: 1, ..Default::default()
        }
    }

    fn watch(s: &Store, repo: &str, n: u64, opener: &str) {
        s.upsert_pr_watch(&PrWatch {
            repo: repo.into(), pr_number: n, pr_url: format!("https://github.com/{repo}/pull/{n}"),
            opener_session_id: Some(opener.into()), created_at: 0,
        }).unwrap();
    }

    #[test]
    fn tracked_pr_plus_every_watch_as_opener_and_nobody_elses() {
        let s = store();
        let sess = session("w1", "o/bellhop", Some(1466));
        s.upsert_session(&sess).unwrap();
        watch(&s, "o/bellhop", 1473, "w1");
        watch(&s, "o/manifests", 988, "w1");
        watch(&s, "o/manifests", 989, "w1");
        watch(&s, "o/other", 5, "someone-else");
        let prs = for_session(&s, &sess);
        let labels: Vec<_> = prs.iter().map(SessionPr::label).collect();
        assert_eq!(labels.len(), 4, "{labels:?}");
        assert!(!labels.contains(&"other#5".to_string()));
        assert!(prs.iter().any(|p| p.tracked && p.number == 1466));
    }

    #[test]
    fn dedups_the_tracked_pr_against_its_own_watch_case_insensitively() {
        let s = store();
        let sess = session("w1", "O/Repo", Some(7));
        watch(&s, "o/repo", 7, "w1");
        let prs = for_session(&s, &sess);
        assert_eq!(prs.len(), 1);
        assert!(prs[0].tracked);
    }

    #[test]
    fn open_first_failing_ahead_of_passing_then_merged_then_closed() {
        let s = store();
        let sess = session("w1", "o/r", Some(1));
        s.save_pr_status(&view("o/r", 1, "MERGED", true, &[("completed", Some("success"))])).unwrap();
        s.save_pr_status(&view("o/r", 2, "OPEN", false, &[("completed", Some("success"))])).unwrap();
        s.save_pr_status(&view("o/r", 3, "OPEN", false, &[("completed", Some("failure"))])).unwrap();
        s.save_pr_status(&view("o/r", 4, "CLOSED", false, &[])).unwrap();
        for n in [2, 3, 4] {
            watch(&s, "o/r", n, "w1");
        }
        let nums: Vec<u64> = for_session(&s, &sess).iter().map(|p| p.number).collect();
        assert_eq!(nums, vec![3, 2, 1, 4]);
    }

    #[test]
    fn failing_checks_rank_ahead_of_pending_ones_and_agree_with_the_chip_colour() {
        let s = store();
        let sess = session("w1", "o/bellhop", Some(1466));
        // #1466: 28/53 passing, the rest pending. #1473: 9/12 passing, 3 failing.
        let mut pending = view("o/bellhop", 1466, "OPEN", false, &[]);
        pending.checks = (0..53).map(|i| CheckView { name: "c".into(), status: if i < 28 { "completed" } else { "in_progress" }.into(), conclusion: (i < 28).then(|| "success".to_string()) }).collect();
        pending.ci = CiVerdict::Pending;
        let mut failing = view("o/bellhop", 1473, "OPEN", false, &[]);
        failing.checks = (0..12).map(|i| CheckView { name: "c".into(), status: "completed".into(), conclusion: Some(if i < 9 { "success" } else { "failure" }.into()) }).collect();
        failing.ci = CiVerdict::Failed;
        s.save_pr_status(&pending).unwrap();
        s.save_pr_status(&failing).unwrap();
        watch(&s, "o/bellhop", 1473, "w1");
        let prs = for_session(&s, &sess);
        assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), vec![1473, 1466]);
        assert!(prs[0].is_failing() && !prs[1].is_failing() && prs[1].is_pending());
        assert_eq!(summary(&prs).as_deref(), Some("PR bellhop#1473 +1 · 1 failing"));
    }

    #[test]
    fn a_failing_check_counts_as_failing_even_if_the_verdict_lags() {
        let mut p = from_view(&view("o/r", 1, "OPEN", false, &[("completed", Some("failure"))]), false);
        p.ci = CiVerdict::Pending;
        assert!(p.is_failing());
    }

    #[test]
    fn check_counts_and_review_state_come_from_the_cached_view() {
        let s = store();
        let sess = session("w1", "o/r", Some(1));
        let mut v = view("o/r", 1, "OPEN", false, &[("completed", Some("success")), ("in_progress", None), ("completed", Some("failure"))]);
        v.reviews.insert("a".into(), "CHANGES_REQUESTED".into());
        s.save_pr_status(&v).unwrap();
        let pr = &for_session(&s, &sess)[0];
        assert_eq!(pr.checks_line().as_deref(), Some("1/3 checks passing"));
        assert_eq!(pr.state_line(), "open · changes requested");
    }

    #[test]
    fn uncached_prs_still_listed_open_with_a_constructed_url() {
        let s = store();
        let sess = session("w1", "o/r", Some(9));
        let prs = for_session(&s, &sess);
        assert_eq!(prs[0].url, "https://github.com/o/r/pull/9");
        assert_eq!(prs[0].checks_line(), None);
    }

    #[test]
    fn an_uncached_tracked_pr_follows_the_sessions_own_end_state() {
        let s = store();
        let mut sess = session("w1", "o/r", Some(7));
        assert_eq!(for_session(&s, &sess)[0].state, PrState::Open);
        sess.merged_at = Some(1);
        assert_eq!(for_session(&s, &sess)[0].state, PrState::Merged);
        // Auto-reap claims then clears `merged_at`, leaving a plain Done row.
        sess.merged_at = None;
        sess.status = SessionStatus::Done;
        assert_eq!(for_session(&s, &sess)[0].state, PrState::Merged);
        // A Done row that ran `ninox complete` proves nothing about a merge.
        sess.completion_outcome = Some(crate::types::CompletionOutcome { summary: "x".into(), evidence: vec![], completed_at: 0 });
        assert_eq!(for_session(&s, &sess)[0].state, PrState::Closed);
    }

    #[test]
    fn another_sessions_prs_row_with_the_same_number_is_not_used() {
        let s = store();
        let mut sess = session("w1", "o/a", Some(5));
        sess.pr_id = Some(5);
        s.upsert_pr(&PR { id: 5, number: 5, title: "other".into(), url: "https://github.com/o/b/pull/5".into(), body: String::new(), session_id: "w2".into() }).unwrap();
        let prs = for_session(&s, &sess);
        assert_eq!(prs[0].url, "https://github.com/o/a/pull/5");
        assert!(prs[0].title.is_empty());
    }

    #[test]
    fn summary_names_the_lead_open_pr_count_and_failures() {
        let s = store();
        let sess = session("w1", "o/bellhop", Some(1466));
        s.save_pr_status(&view("o/bellhop", 1466, "OPEN", false, &[("completed", Some("failure"))])).unwrap();
        s.save_pr_status(&view("o/bellhop", 1473, "OPEN", false, &[("completed", Some("success"))])).unwrap();
        s.save_pr_status(&view("o/bellhop", 900, "MERGED", true, &[])).unwrap();
        watch(&s, "o/bellhop", 1473, "w1");
        watch(&s, "o/bellhop", 900, "w1");
        let prs = for_session(&s, &sess);
        assert_eq!(summary(&prs).as_deref(), Some("PR bellhop#1466 +2 · 1 failing"));
        assert_eq!(summary(&prs[..1]).as_deref(), Some("PR #1466"));
        assert_eq!(summary(&[]), None);
    }

    #[test]
    fn summary_never_leads_with_a_merged_pr_while_one_is_open() {
        let s = store();
        let sess = session("w1", "o/r", Some(894));
        s.save_pr_status(&view("o/r", 894, "MERGED", true, &[])).unwrap();
        s.save_pr_status(&view("o/r", 10, "OPEN", false, &[("completed", Some("success"))])).unwrap();
        watch(&s, "o/r", 10, "w1");
        assert_eq!(summary(&for_session(&s, &sess)).as_deref(), Some("PR r#10 +1"));
    }

    #[test]
    fn all_merged_summary() {
        let s = store();
        let sess = session("w1", "o/r", Some(1));
        s.save_pr_status(&view("o/r", 1, "MERGED", true, &[])).unwrap();
        s.save_pr_status(&view("o/r", 2, "MERGED", true, &[])).unwrap();
        watch(&s, "o/r", 2, "w1");
        assert_eq!(summary(&for_session(&s, &sess)).as_deref(), Some("2 PRs merged"));
    }

    #[test]
    fn extra_branch_prs_recorded_for_the_session_are_included() {
        let s = store();
        let sess = session("w1", "o/r", Some(1));
        s.upsert_pr(&PR { id: 55, number: 55, title: "dup".into(), url: "https://github.com/o/r/pull/55".into(), body: String::new(), session_id: "w1".into() }).unwrap();
        let nums: Vec<u64> = for_session(&s, &sess).iter().map(|p| p.number).collect();
        assert_eq!(nums, vec![55, 1]);
    }
}
