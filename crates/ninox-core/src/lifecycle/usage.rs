//! Cost/token usage ingestion for interactive `claude-code` sessions.
//!
//! Ninox launches the real `claude` CLI interactively inside a tmux pane
//! (see `harness::HarnessRegistry::interactive_cmd`/`worker_cmd`) rather than driving it
//! through a scripted API, so there is no request/response boundary Ninox
//! controls where a cost or token count could be captured directly. What
//! *does* exist is the CLI's own on-disk transcript: every turn is appended
//! as a JSON line to
//!
//! ```text
//! ~/.claude/projects/<escaped-workspace-path>/<claude-session-uuid>.jsonl
//! ```
//!
//! where the directory name is the session's working directory with every
//! non-alphanumeric character replaced by `-`, and each `assistant`-type
//! line carries a `message.usage` object (`input_tokens`, `output_tokens`,
//! `cache_creation_input_tokens`, `cache_read_input_tokens`) plus
//! `message.model`. Since each Ninox session (orchestrator subdirectory,
//! standalone worktree, or CLI worker worktree) runs in its own dedicated
//! workspace directory, that directory is a reliable 1:1 key back to the
//! session — no `NINOX_SESSION`-style attribution needed for this part.
//!
//! There is no on-disk USD figure to read, so cost is estimated from token
//! counts against a small built-in pricing table. These are **rough
//! priors, not live pricing** — good enough to answer "is this session
//! burning money" and to seed the spawn-modal's data-driven estimate, not
//! to reconcile an invoice.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

/// A snapshot of a workspace's accumulated `claude` usage, computed by
/// summing every turn recorded in its transcript(s).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    /// Estimated total spend across every turn in every transcript found
    /// for the workspace (a workspace may accumulate multiple transcripts
    /// across resumed/restarted `claude` invocations).
    pub cost_usd:       f64,
    /// Context-window occupancy (input + cache-read + cache-creation
    /// tokens) as of the most recent turn — an approximation of "how full
    /// is the context window right now", not a cumulative token count.
    pub context_tokens: u64,
    /// Model of the most recent turn, if any usage was found.
    pub model:          Option<String>,
}

/// Directory Ninox expects `claude`'s own per-project transcripts to live
/// under. Honors `NINOX_CLAUDE_PROJECTS_DIR` as a test/override seam
/// (mirrors `AppConfig::resolved_brain_path`'s `NINOX_BRAIN` pattern);
/// otherwise `~/.claude/projects`.
pub fn claude_projects_dir() -> PathBuf {
    if let Ok(p) = std::env::var("NINOX_CLAUDE_PROJECTS_DIR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

/// The directory-name `claude` derives from a working-directory path: every
/// byte that isn't an ASCII letter or digit becomes `-`.
///
/// Verified against real `~/.claude/projects/*` directory names, e.g.
/// `/Users/x/proj/.claude/worktrees/y` → `-Users-x-proj--claude-worktrees-y`
/// (the doubled dash comes from the `/` before `.claude` *and* the `.`
/// itself each becoming their own `-`).
pub fn claude_project_slug(workspace: &str) -> String {
    workspace
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Rough (input $/1M tokens, output $/1M tokens) pricing priors per model
/// family. Not live pricing — see module docs.
fn price_per_million(model: &str) -> (f64, f64) {
    if model.contains("fable") {
        (20.0, 100.0)
    } else if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("sonnet") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (0.8, 4.0)
    } else {
        // Unknown/unrecognized model — assume sonnet-tier as a middle-of-
        // the-road default rather than under- or over-counting wildly.
        (3.0, 15.0)
    }
}

/// Cost of a single turn given its raw usage counters. Cache reads are
/// billed at a fraction of the base input rate and cache writes at a
/// premium over it, mirroring the ratios real prompt-caching pricing uses.
fn turn_cost_usd(model: &str, input: u64, output: u64, cache_creation: u64, cache_read: u64) -> f64 {
    let (in_rate, out_rate) = price_per_million(model);
    let cache_read_rate  = in_rate * 0.1;
    let cache_write_rate = in_rate * 1.25;
    (input as f64          * in_rate
        + output as f64        * out_rate
        + cache_read as f64    * cache_read_rate
        + cache_creation as f64 * cache_write_rate)
        / 1_000_000.0
}

/// One turn's usage fields, extracted from a transcript line.
struct TurnUsage {
    model:          String,
    input:          u64,
    output:         u64,
    cache_creation: u64,
    cache_read:     u64,
    timestamp:      String,
}

fn parse_turn(line: &str) -> Option<TurnUsage> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let usage = v.pointer("/message/usage")?;
    let get_u64 = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    Some(TurnUsage {
        model:          v.pointer("/message/model").and_then(|m| m.as_str()).unwrap_or("").to_string(),
        input:          get_u64("input_tokens"),
        output:         get_u64("output_tokens"),
        cache_creation: get_u64("cache_creation_input_tokens"),
        cache_read:     get_u64("cache_read_input_tokens"),
        timestamp:      v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("").to_string(),
    })
}

/// Sum usage across every `*.jsonl` transcript in `dir` (non-recursive).
/// Returns `None` if the directory doesn't exist or no assistant-usage
/// lines were found in it.
fn ingest_dir(dir: &Path) -> Option<UsageSnapshot> {
    let entries = std::fs::read_dir(dir).ok()?;

    let mut total_cost = 0.0f64;
    let mut latest_ts   = String::new();
    let mut latest_context: u64 = 0;
    let mut latest_model: Option<String> = None;
    let mut found = false;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else { continue };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Some(turn) = parse_turn(&line) else { continue };
            found = true;
            total_cost += turn_cost_usd(&turn.model, turn.input, turn.output, turn.cache_creation, turn.cache_read);
            if turn.timestamp >= latest_ts {
                latest_ts      = turn.timestamp;
                latest_context = turn.input + turn.cache_creation + turn.cache_read;
                latest_model   = Some(turn.model);
            }
        }
    }

    found.then_some(UsageSnapshot {
        cost_usd:       total_cost,
        context_tokens: latest_context,
        model:          latest_model,
    })
}

/// Compute the current usage snapshot for a session's workspace directory,
/// by locating and summing `claude`'s own transcript(s) for that directory.
/// Returns `None` when no transcripts exist yet (e.g. the agent hasn't sent
/// its first turn) or the workspace can't be resolved.
pub fn ingest_usage_for_workspace(workspace: &str) -> Option<UsageSnapshot> {
    let dir = claude_projects_dir().join(claude_project_slug(workspace));
    ingest_dir(&dir)
}

/// Serializes tests that mutate `NINOX_CLAUDE_PROJECTS_DIR` (process-global
/// env state) against each other — shared across this module's tests *and*
/// `lifecycle::poller`'s, since both mutate the same env var and Rust runs
/// test fns on parallel threads by default.
#[cfg(test)]
pub(crate) static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn slug_replaces_non_alphanumeric_with_dash() {
        assert_eq!(
            claude_project_slug("/Users/ethan.brodie/Library/Application Support/ninox/orchestrator/2"),
            "-Users-ethan-brodie-Library-Application-Support-ninox-orchestrator-2",
        );
    }

    #[test]
    fn slug_handles_nested_dot_worktree_dirs() {
        assert_eq!(
            claude_project_slug("/Users/x/proj/iac/.claude/worktrees/y"),
            "-Users-x-proj-iac--claude-worktrees-y",
        );
    }

    #[test]
    fn price_ordering_matches_fleet_ranking() {
        // fable-5 highest, opus-4-8 middle, haiku-4-5 lowest — matches the
        // spawn-modal preset ordering (AGENT_PRESETS in ninox-app).
        let (fable_in, fable_out)   = price_per_million("claude-fable-5");
        let (opus_in, opus_out)     = price_per_million("claude-opus-4-8");
        let (haiku_in, haiku_out)   = price_per_million("claude-haiku-4-5");
        assert!(fable_in > opus_in && opus_in > haiku_in);
        assert!(fable_out > opus_out && opus_out > haiku_out);
    }

    #[test]
    fn turn_cost_is_positive_and_scales_with_tokens() {
        let small = turn_cost_usd("claude-opus-4-8", 100, 100, 0, 0);
        let large = turn_cost_usd("claude-opus-4-8", 10_000, 10_000, 0, 0);
        assert!(small > 0.0);
        assert!(large > small);
    }

    #[test]
    fn cache_read_is_cheaper_than_fresh_input() {
        let fresh = turn_cost_usd("claude-sonnet-4-5", 1000, 0, 0, 0);
        let cached = turn_cost_usd("claude-sonnet-4-5", 0, 0, 0, 1000);
        assert!(cached < fresh);
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn ingest_dir_sums_cost_and_uses_latest_turn_for_context() {
        let dir = tempdir().unwrap();
        write_jsonl(dir.path(), "s1.jsonl", &[
            r#"{"type":"assistant","timestamp":"2026-07-05T13:00:00.000Z","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":1000}}}"#,
            r#"{"type":"assistant","timestamp":"2026-07-05T13:01:00.000Z","message":{"model":"claude-fable-5","usage":{"input_tokens":2,"output_tokens":200,"cache_creation_input_tokens":500,"cache_read_input_tokens":45000}}}"#,
            // Non-assistant / no-usage lines must be ignored, not error out.
            r#"{"type":"system","subtype":"turn_duration"}"#,
            "not even json",
        ]);

        let snap = ingest_dir(dir.path()).expect("usage found");
        assert!(snap.cost_usd > 0.0);
        // Latest turn's context = input + cache_creation + cache_read.
        assert_eq!(snap.context_tokens, 2 + 500 + 45000);
        assert_eq!(snap.model.as_deref(), Some("claude-fable-5"));
    }

    #[test]
    fn ingest_dir_sums_across_multiple_transcript_files() {
        let dir = tempdir().unwrap();
        write_jsonl(dir.path(), "a.jsonl", &[
            r#"{"type":"assistant","timestamp":"2026-07-05T13:00:00.000Z","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":10,"output_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        ]);
        write_jsonl(dir.path(), "b.jsonl", &[
            r#"{"type":"assistant","timestamp":"2026-07-05T14:00:00.000Z","message":{"model":"claude-haiku-4-5","usage":{"input_tokens":20,"output_tokens":20,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        ]);
        let one_file = ingest_dir(dir.path()).unwrap();
        // Cost is the sum of both files' turns, not just one.
        let single = turn_cost_usd("claude-haiku-4-5", 10, 10, 0, 0)
            + turn_cost_usd("claude-haiku-4-5", 20, 20, 0, 0);
        assert!((one_file.cost_usd - single).abs() < 1e-12);
        // Latest turn (by timestamp) is in b.jsonl.
        assert_eq!(one_file.context_tokens, 20);
    }

    #[test]
    fn ingest_dir_returns_none_for_missing_directory() {
        let dir = tempdir().unwrap();
        assert!(ingest_dir(&dir.path().join("does-not-exist")).is_none());
    }

    #[test]
    fn ingest_dir_returns_none_when_no_assistant_usage_lines() {
        let dir = tempdir().unwrap();
        write_jsonl(dir.path(), "empty.jsonl", &[r#"{"type":"system"}"#]);
        assert!(ingest_dir(dir.path()).is_none());
    }

    #[test]
    fn ingest_usage_for_workspace_resolves_via_slug_and_projects_dir_override() {
        let dir = tempdir().unwrap();
        let workspace = "/tmp/my-workspace";
        let project_dir = dir.path().join(claude_project_slug(workspace));
        std::fs::create_dir_all(&project_dir).unwrap();
        write_jsonl(&project_dir, "s.jsonl", &[
            r#"{"type":"assistant","timestamp":"2026-07-05T13:00:00.000Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        ]);

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("NINOX_CLAUDE_PROJECTS_DIR").ok();
        std::env::set_var("NINOX_CLAUDE_PROJECTS_DIR", dir.path());

        let snap = ingest_usage_for_workspace(workspace);

        match prior {
            Some(v) => std::env::set_var("NINOX_CLAUDE_PROJECTS_DIR", v),
            None    => std::env::remove_var("NINOX_CLAUDE_PROJECTS_DIR"),
        }

        let snap = snap.expect("usage found via projects-dir override");
        assert!(snap.cost_usd > 0.0);
        assert_eq!(snap.model.as_deref(), Some("claude-opus-4-8"));
    }
}
