//! The PRs tab's model: every PR a session owns plus every explicit
//! `ninox open --pr` watch, one row per PR, grouped by repo.

use ninox_core::types::{GateStatus, PrWatch, Session, SessionStatus, PR};

#[derive(Clone, Debug, PartialEq)]
pub struct PrRow {
    /// `owner/name` when known, else the session's repo as recorded.
    pub repo: String,
    pub number: u64,
    pub url: Option<String>,
    pub title: Option<String>,
    /// The session that owns the PR (`pr_number`), else a watch's opener.
    pub session: Option<String>,
    pub session_name: Option<String>,
    /// The owning session's status; `None` for a watch-only PR.
    pub status: Option<SessionStatus>,
    pub gate: Option<GateStatus>,
    /// Registered through `ninox open --pr`.
    pub watched: bool,
}

impl PrRow {
    pub fn key(&self) -> String {
        format!("{}#{}", self.repo.to_lowercase(), self.number)
    }
}

/// `https://github.com/o/n/pull/7` → `o/n`.
fn repo_of_url(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let mut parts = rest.split('/');
    let (_host, owner, name, kind) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (kind == "pull" && !owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// Merge session PRs (with their stored PR record, if any) and watches,
/// one row per (repo, number); sorted by repo, newest PR first.
pub fn collect(sessions: &[(Session, Option<PR>)], watches: &[PrWatch]) -> Vec<PrRow> {
    let mut rows: Vec<PrRow> = Vec::new();
    for (s, pr) in sessions {
        let Some(number) = s.pr_number else { continue };
        let url = pr.as_ref().map(|p| p.url.clone()).filter(|u| !u.is_empty()).or_else(|| super::report::github_pr_url(&s.repo, number));
        let repo = url.as_deref().and_then(repo_of_url).unwrap_or_else(|| s.repo.clone());
        let row = PrRow {
            repo,
            number,
            url,
            title: pr.as_ref().map(|p| p.title.clone()).filter(|t| !t.is_empty()),
            session: Some(s.id.clone()),
            session_name: Some(s.name.clone()),
            status: Some(s.status.clone()),
            gate: s.gate_status.clone(),
            watched: false,
        };
        // A PR handed from one session to another: the live one owns it.
        match rows.iter_mut().find(|r| r.key() == row.key()) {
            Some(old) if old.status.as_ref().is_some_and(|st| st.is_terminal()) && !s.status.is_terminal() => *old = row,
            Some(_) => {}
            None => rows.push(row),
        }
    }
    for w in watches {
        let repo = repo_of_url(&w.pr_url).unwrap_or_else(|| w.repo.clone());
        let key = format!("{}#{}", repo.to_lowercase(), w.pr_number);
        if let Some(r) = rows.iter_mut().find(|r| r.key() == key) {
            r.watched = true;
            if r.url.is_none() && !w.pr_url.is_empty() {
                r.url = Some(w.pr_url.clone());
            }
            continue;
        }
        let name = w.opener_session_id.as_ref().and_then(|id| sessions.iter().find(|(s, _)| &s.id == id)).map(|(s, _)| s.name.clone());
        rows.push(PrRow {
            repo,
            number: w.pr_number,
            url: Some(w.pr_url.clone()).filter(|u| !u.is_empty()),
            title: None,
            session: w.opener_session_id.clone(),
            session_name: name,
            status: None,
            gate: None,
            watched: true,
        });
    }
    rows.sort_by(|a, b| a.repo.to_lowercase().cmp(&b.repo.to_lowercase()).then(b.number.cmp(&a.number)));
    rows
}

/// One line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Repo { repo: String, count: usize },
    /// `rows[i]`.
    Pr(usize),
    Gap,
}

pub fn lines(rows: &[PrRow]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let repo = &rows[i].repo;
        let n = rows[i..].iter().take_while(|r| r.repo.eq_ignore_ascii_case(repo)).count();
        if !out.is_empty() {
            out.push(Line::Gap);
        }
        out.push(Line::Repo { repo: repo.clone(), count: n });
        out.extend((i..i + n).map(Line::Pr));
        i += n;
    }
    out
}

/// The PR's state in a word: what its session's status says about it.
pub fn state_word(r: &PrRow) -> &'static str {
    match &r.status {
        None => "watching",
        Some(SessionStatus::CiFailed) => "CI failed",
        Some(SessionStatus::ReviewPending) => "in review",
        Some(SessionStatus::Mergeable) => "mergeable",
        Some(SessionStatus::Done) => "merged",
        Some(SessionStatus::Terminated | SessionStatus::Interrupted) => "session ended",
        Some(_) => "open",
    }
}

#[derive(Debug, Default)]
pub struct PrsView {
    pub rows: Vec<PrRow>,
    /// The selected PR's `key()`; survives the list refreshing each tick.
    pub cursor: Option<String>,
    pub last_index: usize,
    /// `[pr_watch] enabled`, read when the tab opens.
    pub watching: Option<bool>,
}

impl PrsView {
    pub fn selected(&self) -> usize {
        self.cursor
            .as_ref()
            .and_then(|k| self.rows.iter().position(|r| &r.key() == k))
            .unwrap_or(self.last_index)
            .min(self.rows.len().saturating_sub(1))
    }

    pub fn selected_row(&self) -> Option<&PrRow> {
        self.rows.get(self.selected())
    }

    pub fn select(&mut self, i: usize) {
        if let Some(r) = self.rows.get(i) {
            self.cursor = Some(r.key());
            self.last_index = i;
        }
    }

    pub fn move_cursor(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let i = (self.selected() as i64 + delta).clamp(0, self.rows.len() as i64 - 1) as usize;
        self.select(i);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn session(id: &str, repo: &str, pr: Option<u64>, status: SessionStatus) -> Session {
        let mut s = crate::test_fixtures::session(id, None, status);
        s.repo = repo.into();
        s.pr_number = pr;
        s.name = format!("{id} name");
        s
    }

    fn watch(url: &str, n: u64, opener: Option<&str>) -> PrWatch {
        PrWatch { repo: repo_of_url(url).unwrap_or_default(), pr_number: n, pr_url: url.into(), opener_session_id: opener.map(Into::into), created_at: 0 }
    }

    pub(crate) fn sample() -> Vec<PrRow> {
        let pr = PR { id: 1, number: 142, title: "Fix token refresh".into(), url: "https://github.com/acme/web/pull/142".into(), body: String::new(), session_id: "w1".into() };
        collect(
            &[
                (session("w1", "/abs/web", Some(142), SessionStatus::CiFailed), Some(pr)),
                (session("w2", "acme/api", Some(7), SessionStatus::Mergeable), None),
                (session("w3", "acme/web", None, SessionStatus::Working), None),
            ],
            &[
                watch("https://github.com/acme/web/pull/142", 142, Some("o")),
                watch("https://github.com/zed/lib/pull/9", 9, None),
                watch("https://github.com/acme/web/pull/150", 150, Some("w3")),
            ],
        )
    }

    #[test]
    fn session_prs_and_watches_merge_one_row_per_pr() {
        let rows = sample();
        let keys: Vec<String> = rows.iter().map(|r| r.key()).collect();
        assert_eq!(keys, ["acme/api#7", "acme/web#150", "acme/web#142", "zed/lib#9"]);
        let fix = &rows[2];
        assert!(fix.watched, "the watch on a session's PR marks it, not a second row");
        assert_eq!((fix.title.as_deref(), fix.session.as_deref(), state_word(fix)), (Some("Fix token refresh"), Some("w1"), "CI failed"));
        assert_eq!(rows[0].url.as_deref(), Some("https://github.com/acme/api/pull/7"), "a slug repo gets a URL");
        assert_eq!((rows[1].session_name.as_deref(), state_word(&rows[1])), (Some("w3 name"), "watching"), "a watch's opener");
        assert_eq!(rows[3].session, None);
    }

    #[test]
    fn rows_group_by_repo() {
        let rows = sample();
        assert_eq!(
            lines(&rows),
            [
                Line::Repo { repo: "acme/api".into(), count: 1 },
                Line::Pr(0),
                Line::Gap,
                Line::Repo { repo: "acme/web".into(), count: 2 },
                Line::Pr(1),
                Line::Pr(2),
                Line::Gap,
                Line::Repo { repo: "zed/lib".into(), count: 1 },
                Line::Pr(3),
            ]
        );
    }

    #[test]
    fn the_cursor_follows_its_pr_across_refreshes() {
        let mut v = PrsView { rows: sample(), ..Default::default() };
        v.move_cursor(2);
        assert_eq!(v.selected_row().unwrap().number, 142);
        v.rows.remove(0);
        assert_eq!(v.selected_row().unwrap().number, 142, "by identity, not position");
        v.rows.clear();
        assert_eq!(v.selected_row(), None);
        v.move_cursor(1);
    }

    #[test]
    fn a_live_session_takes_over_an_ended_ones_pr() {
        let rows = collect(
            &[
                (session("old", "acme/web", Some(5), SessionStatus::Terminated), None),
                (session("new", "acme/web", Some(5), SessionStatus::PrOpen), None),
            ],
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].session.as_deref(), state_word(&rows[0])), (Some("new"), "open"));
    }
}
