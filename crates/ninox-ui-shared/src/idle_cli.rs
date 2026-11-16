//! `ninox idle [--json]` and `ninox idle --prompt [--orchestrator <id>]`:
//! the Idle view's list and its "prompt orchestrators" action from the shell.
//! Reads only the store and its cached disk numbers — no GitHub, no disk walk.

use anyhow::{bail, Result};
use ninox_core::config::AppConfig;
use ninox_core::idle_workers::{self, IdleReport, PromptOutcome};
use ninox_core::store::Store;

pub fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn format_report(report: &IdleReport, now: i64) -> String {
    if report.total == 0 {
        return "no idle workers".to_string();
    }
    let mut out = format!(
        "{} idle workers · {} of which {} is build output\n",
        report.total,
        idle_workers::format_bytes(report.total_bytes),
        idle_workers::format_bytes(report.build_bytes),
    );
    for g in &report.groups {
        out.push_str(&format!("\n{} ({})", g.orchestrator_id.as_deref().unwrap_or("no orchestrator"), g.workers.len()));
        if let Some(t) = g.prompted_at {
            out.push_str(&format!(" · prompted {} ago", idle_workers::format_duration(now.saturating_sub(t))));
        }
        out.push('\n');
        for w in &g.workers {
            out.push_str(&format!(
                "  {}  idle {}  {}  {}\n",
                w.id,
                w.idle_ms.map(idle_workers::format_duration).unwrap_or_else(|| "?".into()),
                w.waiting_on.label(),
                idle_workers::disk_label(w, now),
            ));
        }
    }
    out.trim_end().to_string()
}

/// `caller` is the live session id the process belongs to, `None` for a person.
pub async fn run_idle(
    store: &Store, config: &AppConfig, json: bool, prompt: bool, only: Option<&str>, caller: Option<&str>, port: u16,
) -> Result<String> {
    let now = now_millis();
    let report = idle_workers::load_report(store, &config.idle_workers, now)?;
    if !prompt {
        if only.is_some() {
            bail!("--orchestrator only applies with --prompt");
        }
        return if json { Ok(serde_json::to_string_pretty(&report)?) } else { Ok(format_report(&report, now)) };
    }
    // An orchestrator may only prompt itself; fleet-wide prompting is the user's.
    let only = match caller {
        Some(c) if store.list_orchestrators()?.iter().any(|o| o.id == c) => {
            if only.is_some_and(|o| o != c) {
                bail!("an orchestrator can only prompt itself about its own idle workers");
            }
            Some(c)
        }
        Some(_) => bail!("only an orchestrator or a person can prompt about idle workers; a worker should run `ninox complete` or `ninox request-work`"),
        None => only,
    };
    if let Some(o) = only {
        if !store.list_orchestrators()?.iter().any(|x| x.id == o) {
            bail!("no orchestrator {o}");
        }
    }
    let outcomes = send_prompts(store, &report, only, config, port, now).await;
    if json {
        let rows: Vec<_> = outcomes.iter().map(outcome_json).collect();
        return Ok(serde_json::to_string_pretty(&rows)?);
    }
    if outcomes.is_empty() {
        return Ok("no orchestrator has idle workers".to_string());
    }
    Ok(outcomes.iter().map(PromptOutcome::describe).collect::<Vec<_>>().join("\n"))
}

/// The one send path behind the CLI, GUI and TUI "prompt orchestrators".
pub async fn send_prompts(
    store: &Store, report: &IdleReport, only: Option<&str>, config: &AppConfig, port: u16, now: i64,
) -> Vec<PromptOutcome> {
    let acp = ninox_core::acp_client::HttpAcpRouter { port };
    idle_workers::prompt_orchestrators(store, report, only, &config.idle_workers, now, |orch, text| {
        let acp = &acp;
        async move { idle_workers::send_prompt(store, acp, &orch, &text).await }
    })
    .await
}

fn outcome_json(o: &PromptOutcome) -> serde_json::Value {
    match o {
        PromptOutcome::Sent { orchestrator_id, workers, receipt } => {
            serde_json::json!({"orchestrator": orchestrator_id, "result": "sent", "workers": workers, "receipt": receipt})
        }
        PromptOutcome::RateLimited { orchestrator_id, prompted_ago_ms } => {
            serde_json::json!({"orchestrator": orchestrator_id, "result": "rate_limited", "prompted_ago_ms": prompted_ago_ms})
        }
        PromptOutcome::Failed { orchestrator_id, error } => {
            serde_json::json!({"orchestrator": orchestrator_id, "result": "failed", "error": error})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, Orchestrator, Session, SessionStatus};

    fn session(id: &str, orch: Option<&str>, now: i64) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(Into::into), name: id.into(),
            repo: "r".into(), status: SessionStatus::PrOpen, agent_type: "c".into(), cost_usd: 0.0, started_at: 0,
            pr_number: Some(1), pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::Idle, activity_note: None, activity_since: Some(now - 60 * 60_000),
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn store_with_fleet() -> Store {
        let store = Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap();
        let now = now_millis();
        store.upsert_orchestrator(&Orchestrator { id: "o1".into(), name: "o1".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session("o1", None, now)).unwrap();
        store.upsert_session(&session("w1", Some("o1"), now)).unwrap();
        store
    }

    #[tokio::test]
    async fn lists_text_and_json_without_prompting_and_never_lists_orchestrators() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store_with_fleet();
        let config = AppConfig::default();
        let text = run_idle(&store, &config, false, false, None, None, 1).await.unwrap();
        assert!(text.starts_with("1 idle workers"), "{text}");
        assert!(text.contains("w1") && text.contains("PR awaiting review") && !text.contains("\n  o1 "), "{text}");
        let json: serde_json::Value = serde_json::from_str(&run_idle(&store, &config, true, false, None, None, 1).await.unwrap()).unwrap();
        assert_eq!(json["total"], 1);
        assert_eq!(json["groups"][0]["workers"][0]["id"], "w1");
        assert!(store.idle_prompt_records().unwrap().is_empty(), "listing never records a prompt");
    }

    #[tokio::test]
    async fn prompting_is_refused_for_workers_and_unknown_orchestrators() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store_with_fleet();
        let config = AppConfig::default();
        let err = run_idle(&store, &config, false, true, None, Some("w1"), 1).await.unwrap_err().to_string();
        assert!(err.contains("only an orchestrator or a person"), "{err}");
        let err = run_idle(&store, &config, false, true, Some("nope"), None, 1).await.unwrap_err().to_string();
        assert!(err.contains("no orchestrator nope"), "{err}");
        assert!(run_idle(&store, &config, false, false, Some("o1"), None, 1).await.is_err());
        assert!(store.idle_prompt_records().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_orchestrator_can_only_prompt_itself() {
        let _iso = ninox_core::test_isolation::isolate();
        let store = store_with_fleet();
        store.upsert_orchestrator(&Orchestrator { id: "o2".into(), name: "o2".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session("o2", None, now_millis())).unwrap();
        store.upsert_session(&session("w2", Some("o2"), now_millis())).unwrap();
        let config = AppConfig::default();
        let err = run_idle(&store, &config, false, true, Some("o2"), Some("o1"), 1).await.unwrap_err().to_string();
        assert!(err.contains("can only prompt itself"), "{err}");
        let out = run_idle(&store, &config, false, true, None, Some("o1"), 1).await.unwrap();
        assert!(out.starts_with("o1:") && !out.contains("o2"), "unscoped prompt is forced to the caller: {out}");
        assert_eq!(store.idle_prompt_records().unwrap().keys().collect::<Vec<_>>(), ["o1"]);
    }

    #[test]
    fn empty_report_says_so() {
        assert_eq!(format_report(&IdleReport::default(), 0), "no idle workers");
    }
}
