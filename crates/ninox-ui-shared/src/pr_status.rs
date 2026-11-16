//! `ninox pr status [<pr>]`: the PR watcher's cached view of a PR. Reads
//! only the store, never GitHub, so agents can run it as often as they like
//! instead of polling `gh`.

use anyhow::{anyhow, bail, Result};
use ninox_core::{pr_events::PrStatusView, store::Store};

/// `(repo, number)` targets for `target`, or for the caller's own PR plus
/// the PRs it watches when no target is given.
fn resolve_targets(store: &Store, target: Option<&str>, caller: Option<&str>) -> Result<Vec<(String, u64)>> {
    let caller_session = caller.and_then(|id| store.get_session(id).ok().flatten());
    let Some(target) = target else {
        let Some(session) = caller_session else {
            bail!("not running inside a Ninox session: pass a PR (`ninox pr status <url|owner/repo#N>`)");
        };
        let mut targets = Vec::new();
        if let (Some(n), false) = (session.pr_number, session.repo.is_empty()) {
            targets.push((session.repo.clone(), n));
        }
        for w in store.list_pr_watches()? {
            if w.opener_session_id.as_deref() == Some(session.id.as_str()) && !targets.contains(&(w.repo.clone(), w.pr_number)) {
                targets.push((w.repo, w.pr_number));
            }
        }
        if targets.is_empty() {
            bail!("session {} has no PR yet and watches none: pass a PR explicitly", session.id);
        }
        return Ok(targets);
    };

    if let Some(found) = ninox_core::github::parse_pr_url(target) {
        return Ok(vec![found]);
    }
    if let Some((repo, n)) = target.rsplit_once('#').filter(|(repo, _)| repo.contains('/')) {
        let n: u64 = n.parse().map_err(|_| anyhow!("not a PR number: {target}"))?;
        return Ok(vec![(repo.to_string(), n)]);
    }
    let n: u64 = target
        .trim_start_matches('#')
        .parse()
        .map_err(|_| anyhow!("expected a PR URL, owner/repo#N or #N, got {target:?}"))?;
    if let Some(s) = caller_session.filter(|s| !s.repo.is_empty()) {
        if store.get_pr_status(&s.repo.to_lowercase(), n)?.is_some() {
            return Ok(vec![(s.repo, n)]);
        }
    }
    match store.find_pr_status_by_number(n)?.as_slice() {
        [one] => Ok(vec![(one.repo.clone(), n)]),
        [] => bail!("no cached PR #{n}: pass owner/repo#{n} or the URL"),
        many => bail!(
            "#{n} is ambiguous ({}): pass owner/repo#{n}",
            many.iter().map(|v| v.repo.as_str()).collect::<Vec<_>>().join(", "),
        ),
    }
}

pub fn run_pr_status(store: &Store, target: Option<&str>, caller: Option<&str>, json: bool, now: i64) -> Result<String> {
    // The cache is keyed by lowercase slug (GitHub slugs are case-insensitive).
    let targets: Vec<(String, u64)> = resolve_targets(store, target, caller)?
        .into_iter()
        .map(|(repo, n)| (repo.to_lowercase(), n))
        .collect();
    let views: Vec<(String, u64, Option<PrStatusView>)> = targets
        .into_iter()
        .map(|(repo, n)| {
            let view = store.get_pr_status(&repo, n)?;
            Ok((repo, n, view))
        })
        .collect::<Result<_>>()?;

    if json {
        let out: Vec<serde_json::Value> = views
            .iter()
            .map(|(repo, n, view)| match view {
                Some(v) => serde_json::to_value(v).unwrap_or_default(),
                None => serde_json::json!({ "repo": repo, "number": n, "cached": false }),
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&out)?);
    }

    let mut text = String::new();
    for (repo, n, view) in &views {
        match view {
            Some(v) => text.push_str(&v.render(now)),
            None => text.push_str(&format!(
                "{repo}#{n}: not observed yet. Ninox caches PR state on its next GitHub sweep \
                 (needs a running daemon with a GitHub token; `ninox open --pr` watches need [pr_watch] enabled).\n",
            )),
        }
    }
    Ok(text.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{PrWatch, SessionStatus};

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn view(repo: &str, n: u64) -> PrStatusView {
        PrStatusView { repo: repo.into(), number: n, title: format!("{repo} pr"), state: "open".into(), observed_at: 1_000, ..Default::default() }
    }

    fn worker(store: &Store, id: &str, repo: &str, pr: Option<u64>) {
        let mut s = crate::cli::test_fixtures::session(id, Some("orch"), SessionStatus::PrOpen);
        s.repo = repo.into();
        s.pr_number = pr;
        store.upsert_session(&s).unwrap();
    }

    #[test]
    fn no_target_shows_own_pr_and_watched_prs_from_cache_only() {
        let _iso = ninox_core::test_isolation::isolate();
        let s = store();
        worker(&s, "w1", "o/r", Some(7));
        s.upsert_pr_watch(&PrWatch {
            repo: "o/dep".into(), pr_number: 3, pr_url: "https://github.com/o/dep/pull/3".into(),
            opener_session_id: Some("w1".into()), created_at: 0,
        }).unwrap();
        s.save_pr_status(&view("o/r", 7)).unwrap();
        let out = run_pr_status(&s, None, Some("w1"), false, 6_000).unwrap();
        assert!(out.contains("o/r#7 open"), "{out}");
        assert!(out.contains("updated:    5s ago"), "{out}");
        assert!(out.contains("o/dep#3: not observed yet"), "{out}");
    }

    #[test]
    fn explicit_targets_resolve_url_slug_and_bare_number() {
        let _iso = ninox_core::test_isolation::isolate();
        let s = store();
        s.save_pr_status(&view("o/r", 7)).unwrap();
        for t in ["https://github.com/o/r/pull/7", "O/R#7", "#7", "7"] {
            let out = run_pr_status(&s, Some(t), None, false, 1_000).unwrap();
            assert!(out.starts_with("o/r#7 open"), "{t}: {out}");
        }
        s.save_pr_status(&view("x/y", 7)).unwrap();
        let err = run_pr_status(&s, Some("#7"), None, false, 1_000).unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
        worker(&s, "w1", "x/y", Some(7));
        let out = run_pr_status(&s, Some("7"), Some("w1"), false, 1_000).unwrap();
        assert!(out.starts_with("x/y#7"), "the caller's own repo wins a bare number: {out}");
    }

    #[test]
    fn json_marks_uncached_prs() {
        let _iso = ninox_core::test_isolation::isolate();
        let s = store();
        s.save_pr_status(&view("o/r", 7)).unwrap();
        let out = run_pr_status(&s, Some("o/r#7"), None, true, 1_000).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["number"], 7);
        let out = run_pr_status(&s, Some("o/r#8"), None, true, 1_000).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v[0]["cached"], false);
    }

    #[test]
    fn errors_name_the_fix() {
        let _iso = ninox_core::test_isolation::isolate();
        let s = store();
        assert!(run_pr_status(&s, None, None, false, 0).unwrap_err().to_string().contains("pass a PR"));
        worker(&s, "w1", "o/r", None);
        assert!(run_pr_status(&s, None, Some("w1"), false, 0).unwrap_err().to_string().contains("no PR yet"));
        assert!(run_pr_status(&s, Some("nope"), None, false, 0).unwrap_err().to_string().contains("expected a PR URL"));
    }
}
