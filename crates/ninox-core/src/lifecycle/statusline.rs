//! Parses Claude Code's `statusLine` hook JSON payload (see
//! <https://code.claude.com/docs/en/statusline#available-data>) and applies
//! it to Ninox's session store. See `docs/superpowers/specs/
//! 2026-07-06-statusline-context-cost-design.md` for the full design.

use serde_json::Value;

/// Everything this module extracts from one hook invocation's payload.
/// Every field is independently optional: the payload's `context_window`
/// and `cost` objects, and several of their sub-fields, may be `null` or
/// absent before the first API response in a session (or, for
/// `context_window.current_usage`, immediately after `/compact`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedPayload {
    pub workspace_dir:         Option<String>,
    pub model:                 Option<String>,
    pub cost_usd:              Option<f64>,
    pub context_used_pct:      Option<f64>,
    pub context_total_tokens:  Option<u64>,
    pub context_window_size:   Option<u64>,
}

/// Parse the hook's stdin JSON. Malformed JSON, or any missing/null field,
/// degrades to `None` for that field rather than an error — there is no
/// failure path here, only more or less complete data.
pub fn parse_payload(raw: &str) -> ParsedPayload {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return ParsedPayload::default();
    };

    let workspace_dir = v.pointer("/workspace/current_dir")
        .or_else(|| v.pointer("/cwd"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = v.pointer("/model/display_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cost_usd = v.pointer("/cost/total_cost_usd").and_then(Value::as_f64);
    let context_used_pct = v.pointer("/context_window/used_percentage").and_then(Value::as_f64);
    let context_total_tokens = v.pointer("/context_window/total_input_tokens").and_then(Value::as_u64);
    let context_window_size = v.pointer("/context_window/context_window_size").and_then(Value::as_u64);

    ParsedPayload {
        workspace_dir, model, cost_usd,
        context_used_pct, context_total_tokens, context_window_size,
    }
}

/// Find the session whose `workspace_path` matches the hook payload's
/// working directory (the same correlation `usage::ingest_usage_for_workspace`
/// already relies on — no session-ID plumbing needed) and apply whichever
/// fields the payload carries. Fields the payload doesn't carry (absent or
/// null in this invocation) are left untouched, never zeroed. An existing
/// `model` is never overwritten (mirrors `poller::poll_usage`'s behavior).
///
/// Returns `Ok(true)` iff a matching session was found, regardless of
/// whether any field actually changed value.
pub fn apply_update(store: &crate::store::Store, payload: &ParsedPayload) -> anyhow::Result<bool> {
    let Some(workspace) = &payload.workspace_dir else { return Ok(false) };

    let sessions = store.list_sessions()?;
    let Some(mut session) = sessions.into_iter()
        .find(|s| s.workspace_path.as_deref() == Some(workspace.as_str()))
    else {
        return Ok(false);
    };

    if let Some(cost) = payload.cost_usd {
        session.cost_usd = cost;
    }
    if let Some(pct) = payload.context_used_pct {
        session.context_used_pct = Some(pct);
    }
    if let Some(tokens) = payload.context_total_tokens {
        session.context_total_tokens = Some(tokens);
    }
    if let Some(size) = payload.context_window_size {
        session.context_window_size = Some(size);
    }
    if session.model.is_none() {
        if let Some(model) = &payload.model {
            session.model = Some(model.clone());
        }
    }

    store.upsert_session(&session)?;
    Ok(true)
}

/// Render the visible text for Claude Code's statusline row. Kept cheap and
/// dependency-free (no shell-outs) per Claude Code's own docs: slow scripts
/// block the line from updating and get cancelled mid-run if a new update
/// fires. Always returns a non-empty string — an all-absent payload still
/// renders `[Claude] 📁 ` rather than nothing, since empty stdout blanks
/// the statusline entirely.
pub fn render_line(payload: &ParsedPayload) -> String {
    let model = payload.model.as_deref().unwrap_or("Claude");
    let dir = payload.workspace_dir.as_deref()
        .and_then(|d| d.rsplit(['/', '\\']).next())
        .unwrap_or("");

    let mut line = format!("[{model}] \u{1F4C1} {dir}");

    if let Some(pct) = payload.context_used_pct {
        let pct = pct.round().clamp(0.0, 100.0) as u32;
        let filled = (pct / 10) as usize;
        let bar = "\u{2593}".repeat(filled) + &"\u{2591}".repeat(10 - filled);
        let color = if pct >= 90 { "\x1b[31m" } else if pct >= 70 { "\x1b[33m" } else { "\x1b[32m" };
        line.push_str(&format!(" | {color}{bar} {pct}%\x1b[0m"));
    }
    if let Some(cost) = payload.cost_usd {
        line.push_str(&format!(" | ${cost:.2}"));
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_PAYLOAD: &str = r#"{
        "cwd": "/current/working/directory",
        "workspace": { "current_dir": "/current/working/directory" },
        "model": { "id": "claude-opus-4-8", "display_name": "Opus 4.8" },
        "cost": { "total_cost_usd": 2.60, "total_duration_ms": 837000 },
        "context_window": {
            "total_input_tokens": 124000,
            "context_window_size": 200000,
            "used_percentage": 62,
            "remaining_percentage": 38
        }
    }"#;

    #[test]
    fn parses_full_payload() {
        let p = parse_payload(FULL_PAYLOAD);
        assert_eq!(p.workspace_dir.as_deref(), Some("/current/working/directory"));
        assert_eq!(p.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(p.cost_usd, Some(2.60));
        assert_eq!(p.context_used_pct, Some(62.0));
        assert_eq!(p.context_total_tokens, Some(124000));
        assert_eq!(p.context_window_size, Some(200000));
    }

    #[test]
    fn missing_context_window_and_cost_yield_none() {
        let p = parse_payload(r#"{"workspace": {"current_dir": "/x"}, "model": {"display_name": "Opus"}}"#);
        assert_eq!(p.workspace_dir.as_deref(), Some("/x"));
        assert_eq!(p.model.as_deref(), Some("Opus"));
        assert_eq!(p.cost_usd, None);
        assert_eq!(p.context_used_pct, None);
        assert_eq!(p.context_total_tokens, None);
        assert_eq!(p.context_window_size, None);
    }

    #[test]
    fn explicit_nulls_yield_none_not_error() {
        let p = parse_payload(r#"{
            "workspace": {"current_dir": "/x"},
            "context_window": null,
            "cost": null
        }"#);
        assert_eq!(p.workspace_dir.as_deref(), Some("/x"));
        assert_eq!(p.cost_usd, None);
        assert_eq!(p.context_used_pct, None);
    }

    #[test]
    fn used_percentage_explicitly_null_yields_none() {
        let p = parse_payload(r#"{
            "workspace": {"current_dir": "/x"},
            "context_window": {"used_percentage": null, "total_input_tokens": null}
        }"#);
        assert_eq!(p.context_used_pct, None);
        assert_eq!(p.context_total_tokens, None);
    }

    #[test]
    fn malformed_json_yields_all_none() {
        let p = parse_payload("not json at all {{{");
        assert_eq!(p, ParsedPayload::default());
    }

    #[test]
    fn cwd_fallback_used_when_workspace_absent() {
        let p = parse_payload(r#"{"cwd": "/fallback/dir"}"#);
        assert_eq!(p.workspace_dir.as_deref(), Some("/fallback/dir"));
    }

    use crate::store::Store;
    use crate::types::{Session, SessionStatus};

    fn test_session(id: &str, workspace: &str) -> Session {
        Session {
            id: id.into(), orchestrator_id: None, name: id.into(),
            repo: String::new(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None,
            workspace_path: Some(workspace.into()), pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None, gate_status: None,
        }
    }

    fn test_store() -> Store {
        let dir = tempfile::tempdir().unwrap();
        Store::open(dir.path().join("t.db")).unwrap()
    }

    #[test]
    fn apply_update_writes_matched_session() {
        let store = test_store();
        store.upsert_session(&test_session("s1", "/ws")).unwrap();

        let payload = ParsedPayload {
            workspace_dir: Some("/ws".into()),
            model: Some("Opus 4.8".into()),
            cost_usd: Some(2.60),
            context_used_pct: Some(62.0),
            context_total_tokens: Some(124_000),
            context_window_size: Some(200_000),
        };
        let found = apply_update(&store, &payload).unwrap();
        assert!(found);

        let s = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.cost_usd, 2.60);
        assert_eq!(s.context_used_pct, Some(62.0));
        assert_eq!(s.context_total_tokens, Some(124_000));
        assert_eq!(s.context_window_size, Some(200_000));
        assert_eq!(s.model.as_deref(), Some("Opus 4.8"));
    }

    #[test]
    fn apply_update_returns_false_for_unmatched_workspace() {
        let store = test_store();
        store.upsert_session(&test_session("s1", "/ws")).unwrap();

        let payload = ParsedPayload {
            workspace_dir: Some("/does/not/match".into()),
            ..Default::default()
        };
        let found = apply_update(&store, &payload).unwrap();
        assert!(!found);
        // Untouched.
        assert_eq!(store.get_session("s1").unwrap().unwrap().cost_usd, 0.0);
    }

    #[test]
    fn apply_update_with_no_workspace_dir_is_a_noop() {
        let store = test_store();
        store.upsert_session(&test_session("s1", "/ws")).unwrap();
        let found = apply_update(&store, &ParsedPayload::default()).unwrap();
        assert!(!found);
    }

    #[test]
    fn apply_update_leaves_absent_fields_untouched() {
        let store = test_store();
        let mut seed = test_session("s1", "/ws");
        seed.cost_usd = 1.0;
        seed.context_used_pct = Some(10.0);
        store.upsert_session(&seed).unwrap();

        // Payload only carries cost — context fields absent.
        let payload = ParsedPayload {
            workspace_dir: Some("/ws".into()),
            cost_usd: Some(5.0),
            ..Default::default()
        };
        apply_update(&store, &payload).unwrap();

        let s = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.cost_usd, 5.0);
        assert_eq!(s.context_used_pct, Some(10.0), "untouched, payload didn't carry this field");
    }

    #[test]
    fn apply_update_does_not_overwrite_existing_model() {
        let store = test_store();
        let mut seed = test_session("s1", "/ws");
        seed.model = Some("claude-opus-4-8".into());
        store.upsert_session(&seed).unwrap();

        let payload = ParsedPayload {
            workspace_dir: Some("/ws".into()),
            model: Some("Opus 4.8".into()), // hook's display name differs from the stored model id
            ..Default::default()
        };
        apply_update(&store, &payload).unwrap();

        let s = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"), "existing model id is preserved");
    }

    #[test]
    fn render_line_full_payload() {
        let payload = ParsedPayload {
            workspace_dir: Some("/Users/x/repo/.claude/worktrees/my-worktree".into()),
            model: Some("Opus 4.8".into()),
            cost_usd: Some(2.60),
            context_used_pct: Some(62.0),
            context_total_tokens: Some(124_000),
            context_window_size: Some(200_000),
        };
        let line = render_line(&payload);
        assert!(line.starts_with("[Opus 4.8] \u{1F4C1} my-worktree"), "got: {line}");
        assert!(line.contains("62%"), "got: {line}");
        assert!(line.contains("$2.60"), "got: {line}");
    }

    #[test]
    fn render_line_omits_context_bar_when_pct_absent() {
        let payload = ParsedPayload {
            workspace_dir: Some("/x/y".into()),
            model: Some("Opus".into()),
            cost_usd: Some(1.00),
            ..Default::default()
        };
        let line = render_line(&payload);
        assert!(line.contains("$1.00"));
        assert!(!line.contains('%'));
    }

    #[test]
    fn render_line_omits_cost_when_absent() {
        let payload = ParsedPayload {
            workspace_dir: Some("/x/y".into()),
            model: Some("Opus".into()),
            context_used_pct: Some(10.0),
            ..Default::default()
        };
        let line = render_line(&payload);
        assert!(line.contains("10%"));
        assert!(!line.contains('$'));
    }

    #[test]
    fn render_line_minimal_fallback_with_no_data() {
        let line = render_line(&ParsedPayload::default());
        assert_eq!(line, "[Claude] \u{1F4C1} ");
    }

    #[test]
    fn render_line_color_thresholds() {
        let payload_at = |pct: f64| ParsedPayload { context_used_pct: Some(pct), ..Default::default() };
        assert!(render_line(&payload_at(69.0)).contains("\x1b[32m"), "under 70 is green");
        assert!(render_line(&payload_at(70.0)).contains("\x1b[33m"), "70-89 is yellow");
        assert!(render_line(&payload_at(89.0)).contains("\x1b[33m"));
        assert!(render_line(&payload_at(90.0)).contains("\x1b[31m"), "90+ is red");
    }
}
