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
}
