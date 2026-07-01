use crate::{
    config::AppConfig,
    events::{Engine, Event},
    github::{split_repo, CheckRun},
    hooks,
    lifecycle::{
        enrichment::EnrichmentCache,
        probe::is_pid_alive,
    },
    types::{
        CIStatus, Comment, Notification, NotificationKind, PrId, SessionStatus, PR,
    },
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub struct Poller {
    engine:           Arc<Engine>,
    enrichment_cache: Arc<std::sync::Mutex<EnrichmentCache>>,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            enrichment_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(self, token: CancellationToken) {
        let mut pid_interval    = tokio::time::interval(Duration::from_secs(5));
        let mut github_interval = tokio::time::interval(Duration::from_secs(30));
        // Prevent a missed tick from causing back-to-back polls.
        github_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = token.cancelled()      => break,
                _ = pid_interval.tick()    => self.poll_pids().await,
                _ = github_interval.tick() => self.poll_github().await,
            }
        }
    }

    // ── PID liveness ────────────────────────────────────────────────────────

    async fn poll_pids(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        for mut session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            if let Some(pid) = session.pid {
                if !is_pid_alive(pid) {
                    session.status = SessionStatus::Terminated;
                    let _ = self.engine.store.upsert_session(&session);
                    self.engine.emit(Event::SessionUpdated(session));
                    continue;
                }
            }

            // Poll metadata files for PR number on working sessions that have none yet.
            if matches!(session.status, SessionStatus::Working | SessionStatus::Spawning)
                && session.pr_number.is_none()
            {
                let sessions_dir = AppConfig::sessions_dir();
                if let Ok(meta) = hooks::read_session_metadata(&sessions_dir, &session.id) {
                    if let Some(pr_num) = meta.pr_number {
                        session.pr_number = Some(pr_num);
                        session.status    = SessionStatus::PrOpen;
                        let _ = self.engine.store.upsert_session(&session);
                        self.engine.emit(Event::SessionUpdated(session.clone()));
                        tracing::info!(
                            "session {} PR #{pr_num} detected via metadata hook",
                            session.id
                        );
                    }
                }
            }
        }
    }

    // ── GitHub enrichment ────────────────────────────────────────────────────

    async fn poll_github(&self) {
        let Some(gh) = &self.engine.github else { return };
        let Ok(sessions) = self.engine.store.list_sessions() else { return };

        for session in sessions {
            if matches!(session.status, SessionStatus::Done | SessionStatus::Terminated) {
                continue;
            }
            let Some(pr_number) = session.pr_number else { continue };
            let Some((owner, repo)) = split_repo(&session.repo) else { continue };

            // -- PR state --
            let pr_status = match gh.get_pr_status(&owner, &repo, pr_number).await {
                Ok(s)  => s,
                Err(e) => { tracing::warn!("github pr status: {e}"); continue }
            };

            let pr_id: PrId = pr_number as i64;

            // -- Merge detection — handle before CI (no point polling CI on merged PR) --
            if pr_status.merged && !matches!(session.status, SessionStatus::Done) {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("merged-{}", session.id),
                    kind:       NotificationKind::WorkerDone,
                    title:      format!("PR merged — {}", session.name),
                    body:       format!("#{} merged successfully", pr_number),
                    session_id: Some(session.id.clone()),
                }));
                if let Err(e) = self.engine.cleanup_session(&session.id).await {
                    tracing::warn!("cleanup_session {}: {e}", session.id);
                }
                // Remove enrichment state for this session — it's done
                {
                    let mut cache = self.enrichment_cache.lock().unwrap();
                    cache.remove(&session.id);
                }
                continue; // skip further enrichment for this session
            }

            // Upsert PR record — only when not merged (merged sessions stay Done after cleanup)
            {
                let pr = PR {
                    id:         pr_id,
                    number:     pr_number,
                    title:      pr_status.title.clone(),
                    url:        format!("https://github.com/{owner}/{repo}/pull/{pr_number}"),
                    body:       String::new(),
                    session_id: session.id.clone(),
                };
                let _ = self.engine.store.upsert_pr(&pr);
                self.engine.emit(Event::PrOpened { session_id: session.id.clone(), pr });
            }

            // -- CI checks --
            let checks = match gh.get_ci_checks(&owner, &repo, &pr_status.head_sha).await {
                Ok(c)  => c,
                Err(e) => { tracing::warn!("github ci checks: {e}"); vec![] }
            };
            let ci = summarize_checks(pr_id, &checks);
            let _ = self.engine.store.upsert_ci_status(&ci);
            self.engine.emit(Event::CiUpdated { pr_id, status: ci.clone() });

            // -- Detect CI transition and update session status --
            let (newly_failing, ci_reaction_already_sent) = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(session.id.clone()).or_default();

                let newly_failing = state.prev_failing.is_none_or(|p| p == 0)
                    && ci.failing > 0;
                state.prev_failing = Some(ci.failing);

                let already_sent = state.ci_reaction_sent;
                if newly_failing && !already_sent {
                    state.ci_reaction_sent = true;
                }
                if ci.failing == 0 {
                    state.ci_reaction_sent = false;
                }
                (newly_failing, already_sent)
            };

            if newly_failing && !ci_reaction_already_sent {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("ci-{}", session.id),
                    kind:       NotificationKind::CiFailure,
                    title:      format!("CI failing — {}", session.name),
                    body:       format!("{}/{} checks failing", ci.failing, ci.total),
                    session_id: Some(session.id.clone()),
                }));
                // Send reaction to the agent in the tmux session
                let failing_names: Vec<String> = checks.iter()
                    .filter(|c| c.conclusion.as_deref() == Some("failure")
                             || c.conclusion.as_deref() == Some("timed_out"))
                    .map(|c| c.name.clone())
                    .collect();
                let msg = crate::lifecycle::reactions::format_ci_reaction(
                    &session, &ci, &failing_names
                );
                if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                    tracing::warn!("send ci reaction to {}: {e}", session.id);
                }
            }

            // -- Review threads (throttled via seen_comment_ids) --
            let threads = match gh.get_review_threads(&owner, &repo, pr_number).await {
                Ok(t)  => t,
                Err(e) => { tracing::warn!("github review threads: {e}"); vec![] }
            };

            let has_changes_requested = threads.iter().any(|t| t.state == "CHANGES_REQUESTED");

            let (has_new, review_reaction_already_sent, new_comments) = {
                let mut cache = self.enrichment_cache.lock().unwrap();
                let state = cache.entry(session.id.clone()).or_default();
                let mut has_new = false;
                let mut new_comments: Vec<Comment> = Vec::new();

                for thread in &threads {
                    if thread.state == "CHANGES_REQUESTED"
                        && !state.seen_comment_ids.contains(&thread.id)
                    {
                        state.seen_comment_ids.insert(thread.id);
                        has_new = true;
                        let comment = Comment {
                            id:         thread.id,
                            pr_id,
                            author:     thread.author.clone(),
                            body:       thread.body.clone(),
                            path:       thread.path.clone(),
                            line:       thread.line,
                            created_at: 0,
                        };
                        let _ = self.engine.store.upsert_comment(&comment);
                        self.engine.emit(Event::ReviewComment { pr_id, comment: comment.clone() });
                        new_comments.push(comment);
                    }
                }

                let already_sent = state.review_reaction_sent;
                if has_new && !already_sent {
                    state.review_reaction_sent = true;
                }
                // Reset when all CHANGES_REQUESTED are resolved
                if !has_changes_requested {
                    state.review_reaction_sent = false;
                }
                (has_new, already_sent, new_comments)
            };

            // Update session status in DB (after review threads so has_changes_requested is known)
            let new_status = derive_session_status(&session.status, &pr_status, &ci, has_changes_requested);
            let mut updated = session.clone();
            updated.status = new_status;
            if updated.status != session.status {
                let _ = self.engine.store.upsert_session(&updated);
                self.engine.emit(Event::SessionUpdated(updated.clone()));
            }

            if has_new && !review_reaction_already_sent {
                self.engine.emit(Event::Notification(Notification {
                    id:         format!("review-{}", session.id),
                    kind:       NotificationKind::PrNeedsAttention,
                    title:      format!("Review comments — {}", session.name),
                    body:       "Changes requested on your PR".to_string(),
                    session_id: Some(session.id.clone()),
                }));
                if !new_comments.is_empty() {
                    let msg = crate::lifecycle::reactions::format_review_reaction(
                        &session, &new_comments
                    );
                    if let Err(e) = self.engine.send_to_session(&session.id, &msg).await {
                        tracing::warn!("send review reaction to {}: {e}", session.id);
                    }
                }
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn summarize_checks(pr_id: PrId, checks: &[CheckRun]) -> CIStatus {
    let total   = checks.len() as u32;
    let failing = checks.iter().filter(|c| {
        c.conclusion.as_deref() == Some("failure")
            || c.conclusion.as_deref() == Some("timed_out")
    }).count() as u32;
    let passing = checks.iter().filter(|c| {
        c.conclusion.as_deref() == Some("success")
    }).count() as u32;
    let pending = total - failing - passing;
    CIStatus { pr_id, total, failing, passing, pending }
}

fn derive_session_status(
    current:               &SessionStatus,
    pr_status:             &crate::github::PrStatus,
    ci:                    &CIStatus,
    has_changes_requested: bool,
) -> SessionStatus {
    // Terminal states are never overwritten.
    if matches!(current, SessionStatus::Done | SessionStatus::Terminated) {
        return current.clone();
    }
    if pr_status.merged {
        return SessionStatus::Done;
    }
    if ci.failing > 0 {
        return SessionStatus::CiFailed;
    }
    if has_changes_requested {
        return SessionStatus::ReviewPending;
    }
    if pr_status.mergeable == Some(true) && ci.failing == 0 && ci.pending == 0 {
        return SessionStatus::Mergeable;
    }
    SessionStatus::PrOpen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SessionStatus;

    #[test]
    fn summarize_checks_counts_failures() {
        let checks = vec![
            CheckRun { name: "lint".into(), status: "completed".into(), conclusion: Some("success".into()) },
            CheckRun { name: "test".into(), status: "completed".into(), conclusion: Some("failure".into()) },
            CheckRun { name: "build".into(), status: "in_progress".into(), conclusion: None },
        ];
        let ci = summarize_checks(1, &checks);
        assert_eq!(ci.total,   3);
        assert_eq!(ci.passing, 1);
        assert_eq!(ci.failing, 1);
        assert_eq!(ci.pending, 1);
    }

    #[test]
    fn derive_status_merged_becomes_done() {
        let pr = crate::github::PrStatus {
            merged: true, state: "closed".into(), mergeable: None,
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Done));
    }

    #[test]
    fn derive_status_ci_failure_overrides_open() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 1, passing: 2, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::CiFailed));
    }

    #[test]
    fn derive_status_all_green_becomes_mergeable() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 0, passing: 3, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Mergeable));
    }

    #[test]
    fn derive_status_preserves_done() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::Done, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Done));
    }

    #[test]
    fn derive_status_preserves_terminated() {
        let pr = crate::github::PrStatus {
            merged: true, state: "closed".into(), mergeable: None,   // merged=true!
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 0, failing: 0, passing: 0, pending: 0 };
        let s  = derive_session_status(&SessionStatus::Terminated, &pr, &ci, false);
        assert!(matches!(s, SessionStatus::Terminated));  // must not become Done
    }

    #[test]
    fn derive_status_changes_requested_becomes_review_pending() {
        let pr = crate::github::PrStatus {
            merged: false, state: "open".into(), mergeable: Some(true),
            title: "t".into(), number: 1, head_sha: String::new(),
        };
        let ci = CIStatus { pr_id: 1, total: 3, failing: 0, passing: 3, pending: 0 };
        let s  = derive_session_status(&SessionStatus::PrOpen, &pr, &ci, true);
        assert!(matches!(s, SessionStatus::ReviewPending));
    }
}
