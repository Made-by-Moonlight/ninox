use super::*;
use crate::github::{PrStatus, ReviewThread};
use crate::github_graphql::PrMeta;
use crate::types::Comment;

const MIN: i64 = 60_000;
const STUCK: i64 = 45 * MIN;

fn snap() -> PrSnapshot {
    PrSnapshot {
        status: PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true), title: "t".into(),
            number: 7, head_sha: "sha1".into(), head_ref: String::new(), base_ref: String::new(),
        },
        closed:         false,
        checks:         vec![],
        threads:        vec![],
        issue_comments: vec![],
        meta:           PrMeta { is_draft: Some(false), merge_state: Some("CLEAN".into()), ..PrMeta::default() },
    }
}

fn check(name: &str, status: &str, conclusion: Option<&str>) -> CheckRun {
    CheckRun { name: name.into(), status: status.into(), conclusion: conclusion.map(Into::into) }
}

fn review(id: i64, author: &str, state: &str, body: &str, at: i64) -> ReviewThread {
    ReviewThread { id, author: author.into(), body: body.into(), path: None, line: None, state: state.into(), created_at: at }
}

fn inline(id: i64, author: &str, path: &str, line: u32, body: &str) -> ReviewThread {
    ReviewThread {
        id, author: author.into(), body: body.into(), path: Some(path.into()), line: Some(line),
        state: "COMMENTED".into(), created_at: id,
    }
}

fn issue_comment(id: i64, author: &str, body: &str) -> Comment {
    Comment { id, pr_id: 7, author: author.into(), body: body.into(), path: None, line: None, created_at: id }
}

fn kinds(events: &[PrEvent]) -> Vec<PrEventKind> {
    events.iter().map(|e| e.kind).collect()
}

/// A cursor that has already seen the plain `snap()` baseline.
fn primed() -> PrCursor {
    let mut c = PrCursor::default();
    assert!(detect(&mut c, &snap(), 0, STUCK).is_empty(), "a quiet PR's baseline reports nothing");
    c
}

/// Runs `s` twice: the second pass must be silent (dedupe).
fn once(c: &mut PrCursor, s: &PrSnapshot, now: i64) -> Vec<PrEvent> {
    let first = detect(c, s, now, STUCK);
    assert!(detect(c, s, now + 1, STUCK).is_empty(), "the same snapshot must not re-report: {first:?}");
    first
}

#[test]
fn ci_passed_once_and_skipped_counts_as_passed() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("build", "in_progress", None), check("lint", "completed", Some("skipped"))];
    assert!(once(&mut c, &s, 1).is_empty(), "pending CI reports nothing");
    s.checks[0] = check("build", "completed", Some("success"));
    assert!(detect(&mut c, &s, 2, STUCK).is_empty(), "the first all-green sweep is only a candidate");
    let ev = once(&mut c, &s, 3);
    assert_eq!(kinds(&ev), vec![PrEventKind::CiPassed]);
}

#[test]
fn ci_passed_waits_for_slower_workflows_to_register() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("lint", "completed", Some("success"))];
    assert!(detect(&mut c, &s, 1, STUCK).is_empty());
    // The test workflow registers on the next sweep: not green after all.
    s.checks.push(check("test", "queued", None));
    assert!(once(&mut c, &s, 2).is_empty());
    s.checks[1] = check("test", "completed", Some("success"));
    assert!(detect(&mut c, &s, 3, STUCK).is_empty());
    assert_eq!(kinds(&once(&mut c, &s, 4)), vec![PrEventKind::CiPassed], "reported once, when it is really green");
}

#[test]
fn ci_failed_once_names_the_failures() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("test", "completed", Some("failure")), check("lint", "completed", Some("success"))];
    let ev = once(&mut c, &s, 1);
    assert_eq!(kinds(&ev), vec![PrEventKind::CiFailed]);
    assert!(ev[0].detail.contains("1/2") && ev[0].detail.contains("test"), "{}", ev[0].detail);
}

#[test]
fn rerun_on_same_sha_reports_the_new_verdict() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("test", "completed", Some("failure"))];
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::CiFailed]);
    s.checks = vec![check("test", "queued", None)];
    assert!(once(&mut c, &s, 2).is_empty());
    s.checks = vec![check("test", "completed", Some("success"))];
    assert!(detect(&mut c, &s, 3, STUCK).is_empty());
    assert_eq!(kinds(&once(&mut c, &s, 4)), vec![PrEventKind::CiPassed]);
}

#[test]
fn a_new_push_resets_ci_state() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("test", "completed", Some("failure"))];
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::CiFailed]);
    // A new head that fails the same way is news: the fix didn't work.
    s.status.head_sha = "sha2".into();
    assert_eq!(kinds(&once(&mut c, &s, 2)), vec![PrEventKind::CiFailed]);
    assert_eq!(c.head_sha, "sha2");
}

#[test]
fn ci_pending_stuck_after_threshold_once_per_sha() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("e2e", "queued", None)];
    assert!(once(&mut c, &s, MIN).is_empty());
    assert!(detect(&mut c, &s, MIN + STUCK - 1, STUCK).is_empty(), "not stuck before the threshold");
    let ev = once(&mut c, &s, MIN + STUCK);
    assert_eq!(kinds(&ev), vec![PrEventKind::CiPendingStuck]);
    assert!(ev[0].detail.contains("e2e"));
    s.status.head_sha = "sha2".into();
    assert!(once(&mut c, &s, MIN + STUCK + 1).is_empty(), "a new push restarts the clock");
    let ev = detect(&mut c, &s, MIN + 2 * STUCK + 1, STUCK);
    assert_eq!(kinds(&ev), vec![PrEventKind::CiPendingStuck]);
}

#[test]
fn stuck_detection_can_be_disabled() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("e2e", "queued", None)];
    assert!(detect(&mut c, &s, 0, 0).is_empty());
    assert!(detect(&mut c, &s, 100 * STUCK, 0).is_empty());
}

#[test]
fn review_comment_from_a_bot_with_location_and_excerpt() {
    let mut c = primed();
    let mut s = snap();
    s.threads = vec![inline(11, "cursor", "src/lib.rs", 42, "Possible bug:\n  this unwrap panics on empty input")];
    let ev = once(&mut c, &s, 1);
    assert_eq!(kinds(&ev), vec![PrEventKind::ReviewComment]);
    assert_eq!(ev[0].detail, "cursor at src/lib.rs:42: \"Possible bug: this unwrap panics on empty input\"");
}

#[test]
fn comments_by_the_tokens_own_account_are_flagged() {
    let mut c = primed();
    let mut s = snap();
    s.meta.viewer_login = Some("slievr".into());
    s.threads = vec![inline(11, "slievr", "src/lib.rs", 1, "fixed in abc")];
    let ev = once(&mut c, &s, 1);
    assert!(ev[0].detail.contains("this account"), "{}", ev[0].detail);
}

#[test]
fn review_with_body_is_a_comment_and_empty_body_review_is_not() {
    let mut c = primed();
    let mut s = snap();
    s.threads = vec![review(20, "bob", "COMMENTED", "", 1), review(21, "bob", "COMMENTED", "nit: naming", 2)];
    let ev = once(&mut c, &s, 1);
    assert_eq!(kinds(&ev), vec![PrEventKind::ReviewComment]);
    assert!(ev[0].detail.contains("nit: naming"));
}

#[test]
fn approval_and_changes_requested() {
    let mut c = primed();
    let mut s = snap();
    s.threads = vec![review(30, "alice", "CHANGES_REQUESTED", "please split this", 1)];
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::ChangesRequested]);
    s.threads.push(review(31, "alice", "APPROVED", "", 2));
    assert_eq!(kinds(&once(&mut c, &s, 2)), vec![PrEventKind::ReviewApproved]);
}

#[test]
fn review_dismissed_when_a_seen_review_flips_to_dismissed() {
    let mut c = primed();
    let mut s = snap();
    s.threads = vec![review(40, "alice", "APPROVED", "", 1)];
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::ReviewApproved]);
    s.threads[0].state = "DISMISSED".into();
    let ev = once(&mut c, &s, 2);
    assert_eq!(kinds(&ev), vec![PrEventKind::ReviewDismissed]);
    assert!(ev[0].detail.contains("approved"), "{}", ev[0].detail);
}

#[test]
fn review_requested_for_new_reviewers_only() {
    let mut c = primed();
    let mut s = snap();
    s.meta.review_requests = Some(vec!["alice".into()]);
    let ev = once(&mut c, &s, 1);
    assert_eq!(kinds(&ev), vec![PrEventKind::ReviewRequested]);
    assert!(ev[0].detail.contains("alice"));
    s.meta.review_requests = Some(vec!["alice".into(), "infra".into()]);
    let ev = once(&mut c, &s, 2);
    assert!(ev[0].detail.contains("infra") && !ev[0].detail.contains("alice"), "{}", ev[0].detail);
}

#[test]
fn conflict_and_behind_base_report_on_transition() {
    let mut c = primed();
    let mut s = snap();
    s.status.mergeable = Some(false);
    s.meta.merge_state = Some("DIRTY".into());
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::MergeableConflict]);
    // UNKNOWN while GitHub recomputes after a push keeps the last known state.
    s.status.mergeable = None;
    s.meta.merge_state = Some("UNKNOWN".into());
    assert!(once(&mut c, &s, 2).is_empty());
    assert!(c.conflict);
    s.status.mergeable = Some(true);
    s.meta.merge_state = Some("BEHIND".into());
    assert_eq!(kinds(&once(&mut c, &s, 3)), vec![PrEventKind::BehindBase]);
    s.status.mergeable = Some(false);
    s.meta.merge_state = Some("DIRTY".into());
    assert_eq!(kinds(&once(&mut c, &s, 4)), vec![PrEventKind::MergeableConflict], "a fresh conflict is news");
}

#[test]
fn ready_for_review_on_draft_to_ready() {
    let mut c = PrCursor::default();
    let mut s = snap();
    s.meta.is_draft = Some(true);
    assert!(detect(&mut c, &s, 0, STUCK).is_empty());
    s.meta.is_draft = Some(false);
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::ReadyForReview]);
}

#[test]
fn pr_comment() {
    let mut c = primed();
    let mut s = snap();
    s.issue_comments = vec![issue_comment(50, "bob", "can you also update the docs?")];
    let ev = once(&mut c, &s, 1);
    assert_eq!(kinds(&ev), vec![PrEventKind::PrComment]);
    assert!(ev[0].detail.starts_with("bob: "));
}

#[test]
fn merged_and_closed_report_once() {
    let mut c = primed();
    let mut s = snap();
    s.status.merged = true;
    s.status.state = "closed".into();
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::Merged]);

    let mut c = primed();
    let mut s = snap();
    s.closed = true;
    s.status.state = "closed".into();
    assert_eq!(kinds(&once(&mut c, &s, 1)), vec![PrEventKind::Closed]);
    // Reopened, then closed again: a second, real close.
    assert!(detect(&mut c, &snap(), 2, STUCK).is_empty());
    assert_eq!(kinds(&once(&mut c, &s, 3)), vec![PrEventKind::Closed]);
}

#[test]
fn baseline_records_history_silently_but_reports_actionable_state() {
    let mut s = snap();
    s.checks = vec![check("test", "completed", Some("failure"))];
    s.status.mergeable = Some(false);
    s.meta.merge_state = Some("DIRTY".into());
    s.threads = vec![
        inline(1, "bob", "a.rs", 1, "old inline"),
        review(2, "bob", "COMMENTED", "old review", 1),
        review(3, "carol", "APPROVED", "", 2),
        review(4, "alice", "CHANGES_REQUESTED", "superseded", 3),
        review(5, "alice", "APPROVED", "", 4),
        review(6, "dave", "CHANGES_REQUESTED", "outstanding", 5),
    ];
    s.issue_comments = vec![issue_comment(7, "bob", "old comment")];
    s.meta.review_requests = Some(vec!["erin".into()]);

    let mut c = PrCursor::default();
    let ev = once(&mut c, &s, 1);
    let mut got = kinds(&ev);
    got.sort();
    assert_eq!(got, vec![PrEventKind::CiFailed, PrEventKind::ChangesRequested, PrEventKind::MergeableConflict]);
    assert!(ev.iter().any(|e| e.detail.contains("outstanding")));
    assert!(!ev.iter().any(|e| e.detail.contains("superseded")));
}

#[test]
fn seen_ids_are_pruned_to_the_current_window() {
    let mut c = primed();
    let mut s = snap();
    s.issue_comments = vec![issue_comment(1, "a", "x"), issue_comment(2, "a", "y")];
    detect(&mut c, &s, 1, STUCK);
    s.issue_comments = vec![issue_comment(2, "a", "y"), issue_comment(3, "a", "z")];
    assert_eq!(kinds(&detect(&mut c, &s, 2, STUCK)), vec![PrEventKind::PrComment]);
    assert_eq!(c.seen_issue_comments, BTreeSet::from([2, 3]));
    assert_eq!(c.issue_floor, 1);
}

#[test]
fn an_empty_or_null_list_never_replays_history() {
    let mut c = primed();
    let mut s = snap();
    s.issue_comments = vec![issue_comment(1, "a", "x")];
    s.threads = vec![inline(2, "b", "a.rs", 1, "y"), review(3, "c", "APPROVED", "", 1)];
    s.meta.review_requests = Some(vec!["erin".into()]);
    detect(&mut c, &s, 1, STUCK);
    // A partial GraphQL error nulls these lists.
    let mut broken = s.clone();
    broken.issue_comments.clear();
    broken.threads.clear();
    broken.meta.review_requests = None;
    assert!(detect(&mut c, &broken, 2, STUCK).is_empty());
    assert!(detect(&mut c, &s, 3, STUCK).is_empty(), "the good sweep after it must not replay anything");
}

#[test]
fn an_old_item_sliding_back_into_the_window_is_not_news() {
    let mut c = primed();
    let mut s = snap();
    s.issue_comments = vec![issue_comment(1, "a", "x"), issue_comment(2, "a", "y")];
    detect(&mut c, &s, 1, STUCK);
    // Window full: 1 falls out as 3 arrives.
    s.issue_comments = vec![issue_comment(2, "a", "y"), issue_comment(3, "a", "z")];
    assert_eq!(kinds(&detect(&mut c, &s, 2, STUCK)), vec![PrEventKind::PrComment]);
    // 3 is deleted, so 1 slides back in.
    s.issue_comments = vec![issue_comment(1, "a", "x"), issue_comment(2, "a", "y")];
    assert!(detect(&mut c, &s, 3, STUCK).is_empty());
}

#[test]
fn own_pr_baseline_reports_activity_since_the_grace_point() {
    let mut s = snap();
    s.threads = vec![inline(1, "cursor", "a.rs", 1, "early bot comment")];
    s.threads[0].created_at = 900;
    s.issue_comments = vec![issue_comment(2, "linear", "linked")];
    s.issue_comments[0].created_at = 100;
    let mut c = PrCursor::default();
    let ev = detect_since(&mut c, &s, 1_000, STUCK, Some(500));
    assert_eq!(kinds(&ev), vec![PrEventKind::ReviewComment], "only the comment after the grace point: {ev:?}");
}

#[test]
fn an_inline_reply_does_not_hide_an_outstanding_changes_request_on_baseline() {
    let mut s = snap();
    s.threads = vec![
        review(1, "alice", "CHANGES_REQUESTED", "split this", 1),
        review(2, "alice", "COMMENTED", "", 2),
    ];
    let mut c = PrCursor::default();
    assert_eq!(kinds(&detect(&mut c, &s, 1, STUCK)), vec![PrEventKind::ChangesRequested]);
}

#[test]
fn cursor_survives_a_serde_round_trip_without_replaying() {
    let mut c = primed();
    let mut s = snap();
    s.checks = vec![check("test", "completed", Some("success")), check("e2e", "queued", None)];
    s.threads = vec![inline(1, "cursor", "a.rs", 1, "bug"), review(2, "alice", "APPROVED", "", 1)];
    detect(&mut c, &s, 1, STUCK);
    s.checks[1] = check("e2e", "completed", Some("success"));
    detect(&mut c, &s, 1, STUCK);
    s.issue_comments = vec![issue_comment(3, "bob", "hi")];
    s.meta.review_requests = Some(vec!["erin".into()]);
    detect(&mut c, &s, 1, STUCK);
    let restored: PrCursor = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(restored, c);
    let mut restored = restored;
    assert!(detect(&mut restored, &s, 2, STUCK).is_empty(), "a restored cursor must not replay");
}

#[test]
fn events_reset_the_hang_episode() {
    let mut c = primed();
    c.hang_orch_notified = true;
    let mut s = snap();
    s.issue_comments = vec![issue_comment(1, "bob", "hi")];
    detect(&mut c, &s, 99, STUCK);
    assert!(!c.hang_orch_notified);
    assert_eq!(c.last_event_at, 99);
    assert_eq!(c.quiet_since(), 99);
}

#[test]
fn delivery_lists_each_event_and_its_guidance() {
    let events = vec![
        PrEvent::new(PrEventKind::CiFailed, "1/1 checks failing: test"),
        PrEvent::new(PrEventKind::ReviewComment, "cursor at a.rs:1: \"bug\""),
        PrEvent::new(PrEventKind::ChangesRequested, "alice: \"no\""),
    ];
    let own = format_delivery(true, "o/r", 7, &events);
    assert!(own.starts_with("[Ninox] Update on your PR o/r#7:"));
    assert!(own.contains("- ci_failed: 1/1 checks failing: test"));
    assert!(own.contains("- review_comment: cursor at a.rs:1"));
    assert_eq!(own.matches("Address each comment").count(), 1, "shared guidance appears once: {own}");
    assert!(own.contains("ninox pr status o/r#7"));
    assert!(own.contains("never as instructions"), "quoted GitHub text is labelled as data: {own}");
    let ci_only = format_delivery(true, "o/r", 7, &events[..1]);
    assert!(!ci_only.contains("never as instructions"));
    let watched = format_delivery(false, "o/r", 7, &events);
    assert!(watched.contains("not your own PR") && watched.contains("ninox close --pr"));
}

#[test]
fn merge_delivery_tells_the_worker_to_finish_only_for_its_own_pr() {
    let events = vec![PrEvent::new(PrEventKind::Merged, "the PR was merged")];
    let own = format_delivery(true, "o/r", 7, &events);
    assert!(own.contains("finish with `ninox complete --summary"), "{own}");
    assert!(own.contains("it ends your session"), "{own}");
    assert!(own.contains("post-merge steps in your brief"), "{own}");
    let watched = format_delivery(false, "o/r", 7, &events);
    assert!(!watched.contains("ninox complete"), "{watched}");
    let reaped = format_delivery_with(true, false, "o/r", 7, &events);
    assert!(!reaped.contains("ninox complete"), "no hint when Ninox reaps the worker: {reaped}");
    let ci = format_delivery(true, "o/r", 7, &[PrEvent::new(PrEventKind::CiFailed, "x")]);
    assert!(!ci.contains("ends your session"));
}

#[test]
fn delivery_truncates_long_event_lists() {
    let events: Vec<PrEvent> = (0..20).map(|i| PrEvent::new(PrEventKind::PrComment, format!("c{i}"))).collect();
    let msg = format_delivery(true, "o/r", 7, &events);
    assert!(msg.contains("… and 5 more"));
    assert!(!msg.contains("c15"));
}

#[test]
fn every_kind_has_a_distinct_wire_name() {
    let names: BTreeSet<&str> = PrEventKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(names.len(), PrEventKind::ALL.len());
    for k in PrEventKind::ALL {
        assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{}\"", k.as_str()));
    }
}

#[test]
fn status_view_summarises_latest_review_per_reviewer() {
    let mut s = snap();
    s.threads = vec![
        review(1, "alice", "CHANGES_REQUESTED", "x", 1),
        review(2, "alice", "APPROVED", "", 2),
        review(3, "bob", "COMMENTED", "nit", 3),
        inline(4, "cursor", "a.rs", 1, "bug"),
    ];
    s.checks = vec![check("test", "completed", Some("success"))];
    s.meta.unresolved_threads = vec![ThreadSummary {
        author: "cursor".into(), path: Some("a.rs".into()), line: Some(1), excerpt: "bug".into(),
    }];
    let v = PrStatusView::from_snapshot("o/r", 7, &s, 1_000);
    assert_eq!(v.reviews, BTreeMap::from([("alice".to_string(), "APPROVED".to_string())]));
    assert_eq!(v.ci, CiVerdict::Passed);
    assert_eq!(v.url, "https://github.com/o/r/pull/7");
    let text = v.render(6_000);
    assert!(text.contains("updated:    5s ago"), "{text}");
    assert!(text.contains("unresolved threads: 1") && text.contains("cursor at a.rs:1"), "{text}");
}

#[test]
fn excerpt_is_single_line_and_bounded() {
    assert_eq!(excerpt("a\n\n  b\tc"), "a b c");
    let long = "x".repeat(500);
    let e = excerpt(&long);
    assert_eq!(e.chars().count(), EXCERPT_CHARS + 1);
    assert!(e.ends_with('…'));
}
