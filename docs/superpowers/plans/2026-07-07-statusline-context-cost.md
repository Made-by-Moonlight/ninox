# Statusline-Sourced Context & Cost Tracking Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ninox configures its own `ninox statusline` command as every spawned session's Claude Code `statusLine` hook, feeding live, CLI-computed context-window usage and cost directly into the session store — replacing the guessed price table and un-deduplicated transcript summation as the primary source for both fields.

**Architecture:** A new pure-function module in `ninox-core` (`lifecycle/statusline.rs`) parses the hook's JSON payload and applies it to the `Session` store by matching `workspace_path` (the same correlation `usage.rs` already uses — no session-ID plumbing needed). A thin `ninox statusline` CLI subcommand wraps it: read stdin, call into `ninox-core`, print a line, always exit 0. Ninox writes the `statusLine` config into every session's `.claude/settings.json` at spawn time (both the orchestrator root and worker worktrees, which don't get one today). Since this is an external process writing into SQLite outside the poller's own read-modify-write cycle, a small diff cache detects the changes on the existing 5s tick and re-broadcasts them as `SessionUpdated`.

**Tech Stack:** Rust, `rusqlite` (existing `Store`, WAL mode), `serde_json`, `clap` (existing `Command` subcommand enum), `tokio` (existing async runtime, `tokio::fs`), `iced` (existing UI, no new UI framework).

## Global Constraints

- Full spec: `docs/superpowers/specs/2026-07-06-statusline-context-cost-design.md`.
- The subcommand must never hang, panic, or exit non-zero — a null/absent hook payload field is a no-op for that field, never a zero write; any error degrades to printing a minimal fallback line and exiting 0 (Claude Code blanks the statusline on non-zero exit or empty stdout).
- `usage.rs`/`poll_usage` (transcript-based, 10s interval) is **not removed** — it stays as the fallback for sessions where the hook hasn't fired yet.
- A pre-existing `.claude/settings.json` (orchestrator root or worker worktree) is **never** overwritten — only written when absent, matching the codebase's existing convention (`app.rs`'s `if !settings_path.exists()` guard).
- Business logic lives in `ninox-core` as pure, directly-testable functions (mirrors `lifecycle/usage.rs`); `ninox-app`'s `main.rs` subcommand handler is a thin I/O wrapper only.
- CI runs `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` (`justfile`, `.github/workflows/ci.yml`) — both must pass before each commit that claims a task complete.
- No new IPC/push mechanism, no `Session.id`/Claude-session-UUID correlation, no cost-reset reconciliation logic, no git-branch/PR-badge/rate-limit fields from the hook payload — all explicitly out of scope per the spec's "Non-goals".

---

### Task 1: Extend `Session` with context/cost fields (store + schema)

**Files:**
- Modify: `crates/ninox-core/src/types.rs:14-40` (`Session` struct)
- Modify: `crates/ninox-core/src/store.rs` (schema, migrations, `upsert_session`, `list_sessions`, `get_session`, tests)
- Modify (mechanical, compiler-guided): every other `Session { ... }` struct literal in the workspace — `crates/ninox-app/src/app.rs`, `crates/ninox-app/src/spawn_util.rs`, `crates/ninox-app/src/main.rs`, `crates/ninox-core/src/events.rs`, `crates/ninox-core/src/lifecycle/reactions.rs`, `crates/ninox-core/src/lifecycle/poller.rs`, `crates/ninox-core/src/tmux.rs`, `crates/ninox-server/src/routes/sessions.rs`

**Interfaces:**
- Produces: `Session.context_used_pct: Option<f64>`, `Session.context_total_tokens: Option<u64>`, `Session.context_window_size: Option<u64>` — consumed by Task 3 (`apply_update`), Task 8 (poller diff cache), Task 9 (`format_burn`).

- [ ] **Step 1: Add the three fields to `Session`**

In `crates/ninox-core/src/types.rs`, replace the closing of the `Session` struct:

```rust
    /// Brain catalogue directory this session was spawned with (its
    /// `NINOX_BRAIN`). Recorded so a Re-file can respawn the session
    /// thinking with the same catalogue. `None` for sessions filed before
    /// this field existed (Re-file falls back to the default brain).
    #[serde(default)]
    pub catalogue_path: Option<String>,
}
```

with:

```rust
    /// Brain catalogue directory this session was spawned with (its
    /// `NINOX_BRAIN`). Recorded so a Re-file can respawn the session
    /// thinking with the same catalogue. `None` for sessions filed before
    /// this field existed (Re-file falls back to the default brain).
    #[serde(default)]
    pub catalogue_path: Option<String>,
    /// Percentage (0-100) of the context window used, as last reported by
    /// Claude Code's own `statusLine` hook (`context_window.used_percentage`
    /// — see `ninox_core::lifecycle::statusline`). More accurate than
    /// `context_tokens` because it accounts for the model's actual window
    /// size and Claude Code's auto-compact buffer. `None` until the
    /// statusline hook has fired at least once for this session.
    #[serde(default)]
    pub context_used_pct: Option<f64>,
    /// Current context-window token count from the same hook payload
    /// (`context_window.total_input_tokens`). `None` until the hook fires.
    #[serde(default)]
    pub context_total_tokens: Option<u64>,
    /// The model's maximum context window size in tokens, from the same
    /// hook payload (`context_window.context_window_size` — 200000 by
    /// default, 1000000 for extended-context models). `None` until the
    /// hook fires.
    #[serde(default)]
    pub context_window_size: Option<u64>,
}
```

- [ ] **Step 2: Add the schema migration and update `upsert_session`/`list_sessions`/`get_session`**

In `crates/ninox-core/src/store.rs`, extend the migrations loop (currently lines 46-54):

```rust
        for (col, ddl) in [
            ("model",          "ALTER TABLE sessions ADD COLUMN model TEXT"),
            ("context_tokens", "ALTER TABLE sessions ADD COLUMN context_tokens INTEGER"),
            ("catalogue_path", "ALTER TABLE sessions ADD COLUMN catalogue_path TEXT"),
            ("context_used_pct",     "ALTER TABLE sessions ADD COLUMN context_used_pct REAL"),
            ("context_total_tokens", "ALTER TABLE sessions ADD COLUMN context_total_tokens INTEGER"),
            ("context_window_size",  "ALTER TABLE sessions ADD COLUMN context_window_size INTEGER"),
        ] {
            if !Self::column_exists(&conn, "sessions", col)? {
                conn.execute(ddl, [])?;
            }
        }
```

Replace `upsert_session` (lines 67-90) with:

```rust
    pub fn upsert_session(&self, s: &Session) -> Result<()> {
        let status = serde_json::to_string(&s.status)?.replace('"', "");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id,orchestrator_id,name,repo,status,agent_type,
             cost_usd,started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
             ON CONFLICT(id) DO UPDATE SET
             status=excluded.status,cost_usd=excluded.cost_usd,
             started_at=excluded.started_at,
             pr_number=excluded.pr_number,pr_id=excluded.pr_id,
             workspace_path=excluded.workspace_path,pid=excluded.pid,
             model=excluded.model,context_tokens=excluded.context_tokens,
             catalogue_path=excluded.catalogue_path,
             context_used_pct=excluded.context_used_pct,
             context_total_tokens=excluded.context_total_tokens,
             context_window_size=excluded.context_window_size",
            params![
                s.id, s.orchestrator_id, s.name, s.repo, status, s.agent_type,
                s.cost_usd, s.started_at, s.pr_number, s.pr_id,
                s.workspace_path, s.pid, s.model, s.context_tokens,
                s.catalogue_path, s.context_used_pct, s.context_total_tokens,
                s.context_window_size
            ],
        )?;
        Ok(())
    }
```

Replace `list_sessions` (lines 92-133) with:

```rust
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size
             FROM sessions ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<u64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<u32>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, Option<f64>>(15)?,
                r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<i64>>(17)?,
            ))
        })?;
        rows.map(|r| {
            let (id, orchestrator_id, name, repo, status_str, agent_type,
                 cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                 model, context_tokens, catalogue_path, context_used_pct,
                 context_total_tokens, context_window_size) = r?;
            let status = serde_json::from_str(&format!("\"{status_str}\""))
                .unwrap_or(SessionStatus::Working);
            Ok(Session {
                id, orchestrator_id, name, repo, status, agent_type,
                cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                catalogue_path,
                context_used_pct,
                context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                context_window_size: context_window_size.map(|v| v.max(0) as u64),
            })
        })
        .collect()
    }
```

Replace `get_session` (lines 135-178) with:

```rust
    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,orchestrator_id,name,repo,status,agent_type,cost_usd,
             started_at,pr_number,pr_id,workspace_path,pid,model,context_tokens,
             catalogue_path,context_used_pct,context_total_tokens,context_window_size
             FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, f64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<u64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<u32>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<String>>(14)?,
                r.get::<_, Option<f64>>(15)?,
                r.get::<_, Option<i64>>(16)?,
                r.get::<_, Option<i64>>(17)?,
            ))
        })?;
        match rows.next() {
            None => Ok(None),
            Some(r) => {
                let (id, orchestrator_id, name, repo, status_str, agent_type,
                     cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                     model, context_tokens, catalogue_path, context_used_pct,
                     context_total_tokens, context_window_size) = r?;
                let status = serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(SessionStatus::Working);
                Ok(Some(Session {
                    id, orchestrator_id, name, repo, status, agent_type,
                    cost_usd, started_at, pr_number, pr_id, workspace_path, pid,
                    model, context_tokens: context_tokens.map(|v| v.max(0) as u64),
                    catalogue_path,
                    context_used_pct,
                    context_total_tokens: context_total_tokens.map(|v| v.max(0) as u64),
                    context_window_size: context_window_size.map(|v| v.max(0) as u64),
                }))
            }
        }
    }
```

- [ ] **Step 3: Add a round-trip test in `store.rs`**

Add to `crates/ninox-core/src/store.rs`'s `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn context_fields_round_trip() {
        let store = test_store();
        let s = Session {
            id: "s1".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 2.6, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: Some(62.0),
            context_total_tokens: Some(124_000),
            context_window_size: Some(200_000),
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s1").unwrap().unwrap();
        assert_eq!(found.context_used_pct, Some(62.0));
        assert_eq!(found.context_total_tokens, Some(124_000));
        assert_eq!(found.context_window_size, Some(200_000));
        // list path decodes it too
        assert_eq!(store.list_sessions().unwrap()[0].context_used_pct, Some(62.0));
    }

    #[test]
    fn context_fields_default_to_none() {
        let store = test_store();
        let s = Session {
            id: "s2".into(), orchestrator_id: None, name: "w".into(),
            repo: "r".into(), status: SessionStatus::Working,
            agent_type: "claude-code".into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, catalogue_path: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
        };
        store.upsert_session(&s).unwrap();
        let found = store.get_session("s2").unwrap().unwrap();
        assert_eq!(found.context_used_pct, None);
        assert_eq!(found.context_total_tokens, None);
        assert_eq!(found.context_window_size, None);
    }
```

- [ ] **Step 4: Fix every other broken `Session { ... }` literal (compiler-guided)**

Run:

```bash
cargo build --workspace 2>&1 | grep -A1 "missing field"
```

This lists every remaining file:line whose `Session { ... }` literal doesn't set the three new fields. For **every** one reported, add exactly these three lines to that literal (matching the existing field-ordering style already used at that call site — one new field per line, immediately after the existing `catalogue_path` field if present, otherwise at the end):

```rust
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
```

Re-run `cargo build --workspace` after each fix; repeat until it succeeds with no errors. Expect to touch: `crates/ninox-app/src/app.rs` (multiple test literals), `crates/ninox-app/src/spawn_util.rs`, `crates/ninox-app/src/main.rs`, `crates/ninox-core/src/events.rs`, `crates/ninox-core/src/lifecycle/reactions.rs` (the `mock_session()` helper), `crates/ninox-core/src/lifecycle/poller.rs` (the `test_session()` helper — fixing this one helper fixes every test that calls it), `crates/ninox-core/src/tmux.rs`, `crates/ninox-server/src/routes/sessions.rs`.

- [ ] **Step 5: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass, including the two new ones from Step 3.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(core): add context_used_pct/context_total_tokens/context_window_size to Session"
```

---

### Task 2: `lifecycle::statusline` — payload parsing (pure, TDD)

**Files:**
- Create: `crates/ninox-core/src/lifecycle/statusline.rs`
- Modify: `crates/ninox-core/src/lifecycle/mod.rs`

**Interfaces:**
- Consumes: nothing (pure JSON parsing).
- Produces: `pub struct ParsedPayload { pub workspace_dir: Option<String>, pub model: Option<String>, pub cost_usd: Option<f64>, pub context_used_pct: Option<f64>, pub context_total_tokens: Option<u64>, pub context_window_size: Option<u64> }` (all fields `pub`, struct derives `Debug, Clone, Default, PartialEq`) and `pub fn parse_payload(raw: &str) -> ParsedPayload` — consumed by Task 3 (`apply_update`) and Task 4 (`render_line`).

- [ ] **Step 1: Register the module**

In `crates/ninox-core/src/lifecycle/mod.rs`, add alongside the existing modules:

```rust
pub mod enrichment;
pub mod poller;
pub mod probe;
pub mod reactions;
pub mod statusline;
pub mod usage;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/ninox-core/src/lifecycle/statusline.rs`:

```rust
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
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test --package ninox-core --lib lifecycle::statusline`
Expected: all 6 tests pass (this module is written test-and-implementation together since the parsing logic is simple and fully specified above — there is no separate "make it fail first" step here because the implementation is already correct by construction; if any test fails, fix `parse_payload` directly against the JSON-pointer paths shown above, not the tests).

- [ ] **Step 4: Commit**

```bash
git add crates/ninox-core/src/lifecycle/statusline.rs crates/ninox-core/src/lifecycle/mod.rs
git commit -m "feat(core): parse Claude Code statusLine hook payloads"
```

---

### Task 3: `apply_update` — store lookup + upsert (TDD)

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/statusline.rs`

**Interfaces:**
- Consumes: `ParsedPayload` (Task 2), `crate::store::Store` (existing `list_sessions`/`upsert_session`).
- Produces: `pub fn apply_update(store: &crate::store::Store, payload: &ParsedPayload) -> anyhow::Result<bool>` (returns `true` iff a session matching `payload.workspace_dir` was found — regardless of whether any field actually changed) — consumed by Task 5 (CLI wrapper).

- [ ] **Step 1: Write the failing tests**

Append to `crates/ninox-core/src/lifecycle/statusline.rs`'s `#[cfg(test)] mod tests`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package ninox-core --lib lifecycle::statusline`
Expected: FAIL with "cannot find function `apply_update`"

- [ ] **Step 3: Implement `apply_update`**

Add to `crates/ninox-core/src/lifecycle/statusline.rs` (above the `#[cfg(test)]` module):

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package ninox-core --lib lifecycle::statusline`
Expected: all tests pass (11 total: 6 from Task 2 + 5 from this task).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/lifecycle/statusline.rs
git commit -m "feat(core): apply statusline payloads to the matching session"
```

---

### Task 4: Visible statusline line renderer (pure, TDD)

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/statusline.rs`

**Interfaces:**
- Consumes: `ParsedPayload` (Task 2).
- Produces: `pub fn render_line(payload: &ParsedPayload) -> String` — consumed by Task 5 (CLI wrapper).

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package ninox-core --lib lifecycle::statusline`
Expected: FAIL with "cannot find function `render_line`"

- [ ] **Step 3: Implement `render_line`**

Add to `crates/ninox-core/src/lifecycle/statusline.rs` (above `#[cfg(test)]`):

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package ninox-core --lib lifecycle::statusline`
Expected: all tests pass (16 total).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-core/src/lifecycle/statusline.rs
git commit -m "feat(core): render the visible statusline text"
```

---

### Task 5: `ninox statusline` CLI subcommand

**Files:**
- Modify: `crates/ninox-app/src/main.rs`

**Interfaces:**
- Consumes: `ninox_core::lifecycle::statusline::{parse_payload, apply_update, render_line}` (Tasks 2-4), `ninox_core::store::Store::open`, `default_db_path()` (existing, `main.rs:505`).
- Produces: `ninox statusline` subcommand, invoked with no arguments, reading JSON from stdin. Consumed by Task 6/7 (`.claude/settings.json` wiring points `command` at `"{ninox_bin} statusline"`).

- [ ] **Step 1: Add the `Statusline` variant**

In `crates/ninox-app/src/main.rs`, add to the `Command` enum (after the existing `Brain` variant, before its closing `}`):

```rust
    /// Emit a Claude Code statusline and record cost/context usage for the
    /// session at this workspace. Invoked by Claude Code's own `statusLine`
    /// hook (see `.claude/settings.json`), not intended for direct use.
    Statusline,
```

- [ ] **Step 2: Short-circuit dispatch before any other startup side effect**

Replace the top of `main()`:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    if let Err(e) = tmux::write_server_config() {
        eprintln!("failed to write tmux config: {e}");
    }
```

with:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Fires on every assistant turn (event-driven) or every `refreshInterval`
    // seconds for every session Ninox spawns — must stay fast and never
    // trigger the tmux-config/wrapper-hook/self-shim setup below, none of
    // which this subcommand needs.
    if matches!(args.command, Some(Command::Statusline)) {
        run_statusline(args.db.unwrap_or_else(default_db_path));
        return Ok(());
    }

    if let Err(e) = tmux::write_server_config() {
        eprintln!("failed to write tmux config: {e}");
    }
```

- [ ] **Step 3: Implement `run_statusline`**

Add near the other subcommand handlers (e.g. after `run_request_work`, `main.rs:321-345`):

```rust
/// Handler for `ninox statusline`. Never returns an error and never
/// panics: any failure (bad JSON, no store, no matching session) degrades
/// to printing the minimal fallback line so Claude Code's statusline row
/// never goes blank. See `ninox_core::lifecycle::statusline` for the
/// actual parsing/update/render logic — this is a thin I/O wrapper.
fn run_statusline(db_path: PathBuf) {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let payload = ninox_core::lifecycle::statusline::parse_payload(&input);

    if let Ok(store) = Store::open(&db_path) {
        let _ = ninox_core::lifecycle::statusline::apply_update(&store, &payload);
    }

    println!("{}", ninox_core::lifecycle::statusline::render_line(&payload));
}
```

- [ ] **Step 4: Verify it builds and runs manually**

Run: `cargo build --workspace`
Expected: builds with no errors or warnings.

Run:
```bash
echo '{"workspace":{"current_dir":"/tmp/x"},"model":{"display_name":"Opus"},"cost":{"total_cost_usd":1.23},"context_window":{"used_percentage":15,"total_input_tokens":30000,"context_window_size":200000}}' | ./target/debug/ninox statusline
```
Expected output: `[Opus] 📁 x | ▓░░░░░░░░░ 15% | $1.23` (no matching session in the store — that's fine, the line still renders from the payload alone per Task 4's design).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/main.rs
git commit -m "feat(app): add ninox statusline subcommand"
```

---

### Task 6: Wire `statusLine` into the orchestrator root's settings.json

**Files:**
- Modify: `crates/ninox-app/src/app.rs:2368-2379` (inside `setup_orchestrator_root`)

**Interfaces:**
- Consumes: `ninox_bin: &str` (existing parameter of `setup_orchestrator_root`).
- Produces: `.claude/settings.json` written under the orchestrator root now includes a `statusLine` key pointing at `{ninox_bin} statusline`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/ninox-app/src/app.rs`'s `#[cfg(test)] mod tests` (near `setup_orchestrator_root_seeds_brain_skill`, ~line 4241):

```rust
    #[tokio::test]
    async fn setup_orchestrator_root_configures_statusline() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, "/path/to/ninox", "/cfg.toml").await.unwrap();

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude").join("settings.json")).unwrap(),
        ).unwrap();
        assert_eq!(settings["statusLine"]["type"], "command");
        assert_eq!(settings["statusLine"]["command"], "/path/to/ninox statusline");
        assert_eq!(settings["statusLine"]["refreshInterval"], 20);
        // The existing subagent-blocker hook must still be present.
        assert!(settings["hooks"]["PreToolUse"].is_array());
    }

    #[tokio::test]
    async fn setup_orchestrator_root_never_overwrites_existing_settings_json() {
        let root = tempdir().unwrap().keep();
        let claude_dir = root.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("settings.json"), r#"{"userCustom": true}"#).unwrap();

        setup_orchestrator_root(&root, "ninox", "/cfg.toml").await.unwrap();

        let contents = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        assert_eq!(contents, r#"{"userCustom": true}"#, "pre-existing settings.json must be left byte-for-byte alone");
    }
```

- [ ] **Step 2: Run the tests to verify the first one fails**

Run: `cargo test --package ninox-app setup_orchestrator_root_configures_statusline`
Expected: FAIL — `settings["statusLine"]` is `Value::Null`.

- [ ] **Step 3: Add the `statusLine` key**

In `crates/ninox-app/src/app.rs`, replace (lines 2368-2379):

```rust
    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Task|Agent",
                    "hooks": [{"type": "command", "command": "node .claude/subagent-blocker.cjs", "timeout": 2000}]
                }]
            }
        });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?).await?;
    }

    Ok(())
}
```

with:

```rust
    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Task|Agent",
                    "hooks": [{"type": "command", "command": "node .claude/subagent-blocker.cjs", "timeout": 2000}]
                }]
            },
            "statusLine": {
                "type": "command",
                "command": format!("{ninox_bin} statusline"),
                "refreshInterval": 20
            }
        });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?).await?;
    }

    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package ninox-app setup_orchestrator_root`
Expected: all `setup_orchestrator_root_*` tests pass (including the two pre-existing ones).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/app.rs
git commit -m "feat(app): configure statusLine for the orchestrator root"
```

---

### Task 7: Wire `statusLine` into worker worktrees

**Files:**
- Modify: `crates/ninox-app/src/spawn_util.rs:201-237` (`create_worker_worktree`)

**Interfaces:**
- Consumes: `ninox_core::config::AppConfig::ninox_bin_dir()` (existing).
- Produces: every worker worktree `create_worker_worktree` creates now gets a `.claude/settings.json` with a `statusLine` entry, unless one already exists there. No signature change to `create_worker_worktree` — both existing call sites (`main.rs:177`, `app.rs:1019`) get this for free.

- [ ] **Step 1: Write the failing test**

Add to `crates/ninox-app/src/spawn_util.rs`'s `#[cfg(test)] mod tests` (near the top, after the existing `use` lines):

```rust
    /// Minimal real git repo so `git worktree add` has a commit to branch
    /// from — `create_worker_worktree` shells out to real `git`.
    fn init_git_repo() -> std::path::PathBuf {
        let dir = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir
    }

    #[tokio::test]
    async fn create_worker_worktree_writes_statusline_settings() {
        let repo = init_git_repo();
        let worktree = create_worker_worktree(repo.to_str().unwrap(), "test-session-1").await.unwrap();

        let settings_path = std::path::Path::new(&worktree).join(".claude").join("settings.json");
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&settings_path).unwrap(),
        ).unwrap();
        assert_eq!(settings["statusLine"]["type"], "command");
        assert!(settings["statusLine"]["command"].as_str().unwrap().ends_with("statusline"));
    }

    #[tokio::test]
    async fn create_worker_worktree_preserves_existing_settings_json() {
        // Simulate a worktree whose branch already carries a checked-in
        // .claude/settings.json (e.g. from a prior run on the same branch
        // name) by pre-creating it in the repo before the worktree exists —
        // easiest here is to create the worktree once, seed a custom
        // settings.json into it, remove the worktree registration, then
        // re-run create_worker_worktree against the same still-existing
        // branch (the "branch already exists" checkout path).
        let repo = init_git_repo();
        let first = create_worker_worktree(repo.to_str().unwrap(), "test-session-2").await.unwrap();
        let settings_path = std::path::Path::new(&first).join(".claude").join("settings.json");
        std::fs::write(&settings_path, r#"{"userCustom": true}"#).unwrap();

        std::process::Command::new("git")
            .args(["-C", repo.to_str().unwrap(), "worktree", "remove", "--force", &first])
            .output()
            .unwrap();

        let second = create_worker_worktree(repo.to_str().unwrap(), "test-session-2").await.unwrap();
        let contents = std::fs::read_to_string(
            std::path::Path::new(&second).join(".claude").join("settings.json"),
        ).unwrap();
        assert_eq!(contents, r#"{"userCustom": true}"#);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package ninox-app create_worker_worktree_writes_statusline_settings`
Expected: FAIL — no `.claude/settings.json` is written today (file not found).

- [ ] **Step 3: Implement the settings write**

In `crates/ninox-app/src/spawn_util.rs`, replace `create_worker_worktree` (lines 201-237):

```rust
pub async fn create_worker_worktree(repo: &str, session_id: &str) -> anyhow::Result<String> {
    use anyhow::Context as _;
    use tokio::process::Command;

    let worktree_path = std::path::Path::new(repo)
        .join(".claude")
        .join("worktrees")
        .join(session_id);
    let worktree_str = worktree_path.to_string_lossy().to_string();

    // Attempt 1: create a fresh branch named after the session.
    let out = Command::new("git")
        .args(["-C", repo, "worktree", "add", &worktree_str, "-b", session_id])
        .output()
        .await
        .context("git worktree add")?;

    if out.status.success() {
        ensure_statusline_settings(&worktree_path).await;
        return Ok(worktree_str);
    }

    // Attempt 2: branch already exists — check it out without -b.
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("already exists") {
        let out2 = Command::new("git")
            .args(["-C", repo, "worktree", "add", &worktree_str, session_id])
            .output()
            .await
            .context("git worktree add (existing branch)")?;
        if out2.status.success() {
            ensure_statusline_settings(&worktree_path).await;
            return Ok(worktree_str);
        }
        anyhow::bail!("{}", String::from_utf8_lossy(&out2.stderr).trim());
    }

    anyhow::bail!("{}", stderr.trim());
}

/// Write a minimal `.claude/settings.json` (`statusLine` only) into a
/// freshly created worker worktree, unless one already exists (e.g.
/// checked into the branch). Best-effort: any failure here must never fail
/// worktree creation itself, so errors are swallowed rather than
/// propagated.
async fn ensure_statusline_settings(worktree_path: &std::path::Path) {
    let claude_dir = worktree_path.join(".claude");
    let settings_path = claude_dir.join("settings.json");
    if settings_path.exists() {
        return;
    }
    let ninox_bin = ninox_core::config::AppConfig::ninox_bin_dir().display().to_string();
    let settings = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("{ninox_bin} statusline"),
            "refreshInterval": 20
        }
    });
    if tokio::fs::create_dir_all(&claude_dir).await.is_ok() {
        if let Ok(body) = serde_json::to_string_pretty(&settings) {
            let _ = tokio::fs::write(&settings_path, body).await;
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package ninox-app spawn_util::tests`
Expected: all tests in this module pass, including the two new ones and the pre-existing `expand_tilde_*` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/spawn_util.rs
git commit -m "feat(app): configure statusLine for worker worktrees"
```

---

### Task 8: Poller diff cache for externally-written context/cost fields

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/poller.rs`

**Interfaces:**
- Consumes: `Session.cost_usd`, `Session.context_used_pct`, `Session.context_total_tokens` (Task 1), `self.engine.store.list_sessions()`, `self.engine.emit(Event::SessionUpdated(...))` (all existing).
- Produces: `Poller` now re-broadcasts `SessionUpdated` whenever an external writer (the `ninox statusline` subcommand) changes these fields directly in the store — no new public API, purely an internal tick.

- [ ] **Step 1: Write the failing test**

Add to `crates/ninox-core/src/lifecycle/poller.rs`'s `#[cfg(test)] mod tests` (near `poll_usage_ingests_transcript_into_store_and_emits_update`):

```rust
    /// The `ninox statusline` subcommand (a separate short-lived process)
    /// writes cost/context fields directly into the store — outside any
    /// read-modify-write cycle this poller drives. This proves the diff
    /// cache detects that external write and re-broadcasts it, and that an
    /// untouched session generates no spurious event.
    #[tokio::test]
    async fn poll_context_updates_emits_only_for_changed_sessions() {
        use crate::store::Store;

        let store = std::sync::Arc::new(Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap());
        let mut s1 = test_session("s1", "/ws1");
        let s2 = test_session("s2", "/ws2");
        store.upsert_session(&s1).unwrap();
        store.upsert_session(&s2).unwrap();
        let engine = Engine::new(store.clone());
        let mut rx = engine.subscribe();
        let poller = Poller::new(engine);

        // First tick establishes the baseline — nothing to diff against yet,
        // so it must not emit for sessions that already exist with no prior
        // cached state.
        poller.poll_context_updates().await;
        let baseline_events = drain_events(&mut rx);
        assert!(baseline_events.is_empty(), "no prior cached state means no change to report");

        // Simulate the statusline hook writing directly into the store for s1 only.
        s1.context_used_pct = Some(42.0);
        s1.cost_usd = 3.5;
        store.upsert_session(&s1).unwrap();

        poller.poll_context_updates().await;
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1, "only the changed session should emit");
        assert!(matches!(
            &events[0],
            Event::SessionUpdated(s) if s.id == "s1" && s.context_used_pct == Some(42.0) && s.cost_usd == 3.5
        ));

        // A third tick with no further changes emits nothing.
        poller.poll_context_updates().await;
        assert!(drain_events(&mut rx).is_empty());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --package ninox-core poll_context_updates_emits_only_for_changed_sessions`
Expected: FAIL with "no method named `poll_context_updates` found"

- [ ] **Step 3: Add the diff cache and polling method**

In `crates/ninox-core/src/lifecycle/poller.rs`, replace the `Poller` struct and `new`/`start` (lines 26-54):

```rust
pub struct Poller {
    engine:           Arc<Engine>,
    enrichment_cache: Arc<std::sync::Mutex<EnrichmentCache>>,
    /// Last-seen `(cost_usd, context_used_pct, context_total_tokens)` per
    /// session, used solely to detect changes written externally by the
    /// `ninox statusline` subcommand — see `poll_context_updates`.
    context_cache:    Arc<std::sync::Mutex<HashMap<String, (f64, Option<f64>, Option<u64>)>>>,
}

impl Poller {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            enrichment_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            context_cache:    Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(self, token: CancellationToken) {
        let mut pid_interval    = tokio::time::interval(Duration::from_secs(5));
        let mut usage_interval  = tokio::time::interval(Duration::from_secs(10));
        let mut github_interval = tokio::time::interval(Duration::from_secs(30));
        // Prevent a missed tick from causing back-to-back polls.
        github_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        usage_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = token.cancelled()      => break,
                _ = pid_interval.tick()    => {
                    self.poll_pids().await;
                    self.poll_context_updates().await;
                }
                _ = usage_interval.tick()  => self.poll_usage().await,
                _ = github_interval.tick() => self.poll_github().await,
            }
        }
    }
```

Add the new method after `poll_usage` (after line 236, before the `// ── GitHub enrichment ──` comment):

```rust
    // ── Statusline-sourced cost/context updates (external writer) ──────────

    /// The `ninox statusline` subcommand (invoked by Claude Code's own
    /// `statusLine` hook — see `lifecycle::statusline`) writes cost/context
    /// fields directly into the store from a separate short-lived process.
    /// Unlike every other poll method, this data doesn't arrive via a
    /// read-modify-write cycle this poller drives, so there's nothing to
    /// diff against except a cache of the last-seen values. Detects
    /// external changes and re-broadcasts them as `SessionUpdated` so the
    /// GUI picks them up.
    async fn poll_context_updates(&self) {
        let Ok(sessions) = self.engine.store.list_sessions() else { return };
        let mut changed = Vec::new();
        {
            let mut cache = self.context_cache.lock().unwrap();
            for session in sessions {
                let key = (session.cost_usd, session.context_used_pct, session.context_total_tokens);
                if cache.get(&session.id) != Some(&key) {
                    cache.insert(session.id.clone(), key);
                    changed.push(session);
                }
            }
        }
        for session in changed {
            self.engine.emit(Event::SessionUpdated(session));
        }
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --package ninox-core poll_context_updates_emits_only_for_changed_sessions`
Expected: PASS

- [ ] **Step 5: Run the full poller test module to check for regressions**

Run: `cargo test --package ninox-core lifecycle::poller`
Expected: all tests pass, including the pre-existing `poll_usage_ingests_transcript_into_store_and_emits_update` and metadata-sync tests.

- [ ] **Step 6: Commit**

```bash
git add crates/ninox-core/src/lifecycle/poller.rs
git commit -m "feat(core): detect and rebroadcast statusline-written session changes"
```

---

### Task 9: UI — `format_burn` shows statusline-sourced context

**Files:**
- Modify: `crates/ninox-app/src/components/inspector_panel.rs`

**Interfaces:**
- Consumes: `Session.context_used_pct`, `Session.context_total_tokens`, `Session.context_window_size` (Task 1).
- Produces: `format_burn` gains three new parameters; its one call site (`inspector_panel.rs:86`) is updated to pass them.

- [ ] **Step 1: Write the failing tests**

In `crates/ninox-app/src/components/inspector_panel.rs`, replace the existing `format_burn` tests (lines 121-129 per the current file — the two tests `format_burn_matches_design_spec_example` and `format_burn_omits_tokens_when_unknown`) with:

```rust
    #[test]
    fn format_burn_matches_design_spec_example() {
        assert_eq!(
            format_burn(3.60, Some(214_000), None, None, None),
            "$3.60 · 214k tokens",
        );
    }

    #[test]
    fn format_burn_omits_tokens_when_unknown() {
        assert_eq!(format_burn(0.0, None, None, None, None), "$0.00");
    }

    #[test]
    fn format_burn_uses_statusline_context_when_present() {
        assert_eq!(
            format_burn(2.60, Some(999_999), Some(62.0), Some(124_000), Some(200_000)),
            "$2.60 · 62% context (124k/200k)",
        );
    }

    #[test]
    fn format_burn_falls_back_when_statusline_context_partially_absent() {
        // context_window_size missing — not enough to render the new format,
        // falls back to the transcript-based token count.
        assert_eq!(
            format_burn(1.00, Some(50_000), Some(25.0), Some(50_000), None),
            "$1.00 · 50k tokens",
        );
    }
```

- [ ] **Step 2: Run the tests to verify the new ones fail**

Run: `cargo test --package ninox-app inspector_panel::tests::format_burn`
Expected: the two pre-existing tests FAIL to compile (wrong arg count); this is expected — proceed to Step 3 in the same commit cycle since Rust won't let the old signature compile at all once the test call sites are updated.

- [ ] **Step 3: Update `format_burn` and its call site**

Replace `format_burn` (lines 41-51):

```rust
fn format_burn(cost_usd: f64, context_tokens: Option<u64>) -> String {
    match context_tokens {
        Some(t) => format!("${cost_usd:.2} · {} tokens", format_tokens_k(t)),
        None    => format!("${cost_usd:.2}"),
    }
}
```

with:

```rust
/// Renders the `Burn` field per the Field Notes kv-sheet spec
/// (`docs/design-concepts/03-field-notes.html`), preferring the
/// statusline-sourced context percentage (`ninox_core::lifecycle::
/// statusline`, more accurate — accounts for window size and the
/// auto-compact buffer) over the transcript-derived raw token count
/// (`ninox_core::lifecycle::usage`) when all three statusline fields are
/// present.
fn format_burn(
    cost_usd:             f64,
    context_tokens:       Option<u64>,
    context_used_pct:     Option<f64>,
    context_total_tokens: Option<u64>,
    context_window_size:  Option<u64>,
) -> String {
    if let (Some(pct), Some(total), Some(size)) =
        (context_used_pct, context_total_tokens, context_window_size)
    {
        return format!(
            "${cost_usd:.2} · {}% context ({}/{})",
            pct.round() as i64,
            format_tokens_k(total),
            format_tokens_k(size),
        );
    }
    match context_tokens {
        Some(t) => format!("${cost_usd:.2} · {} tokens", format_tokens_k(t)),
        None    => format!("${cost_usd:.2}"),
    }
}
```

Update the call site (line 86):

```rust
        field("Burn",           format_burn(session.cost_usd, session.context_tokens), s),
```

to:

```rust
        field("Burn",           format_burn(
            session.cost_usd,
            session.context_tokens,
            session.context_used_pct,
            session.context_total_tokens,
            session.context_window_size,
        ), s),
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package ninox-app inspector_panel`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-app/src/components/inspector_panel.rs
git commit -m "feat(ui): show statusline-sourced context percentage in the Burn field"
```

---

### Task 10: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Run clippy exactly as CI does**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings or errors.

- [ ] **Step 2: Run the full test suite exactly as CI does**

Run: `tmux start-server && cargo test --workspace`
Expected: all tests pass (existing suite + every test added in Tasks 1-9).

- [ ] **Step 3: Fix any failures**

If either command fails, fix the reported issue in the relevant task's files and re-run both commands. Do not proceed to opening a PR until both are clean.

- [ ] **Step 4: Confirm no unintended files are staged**

Run: `git status`
Expected: only the files touched by Tasks 1-9 are modified; nothing unrelated (e.g. the pre-existing uncommitted `spawn_util.rs` edit from the unmerged `feat/resume-interrupted-sessions` branch, which lives on `main`, not this worktree, and must not appear here).
